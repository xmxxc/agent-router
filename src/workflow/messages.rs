//! A worker posts one completion for its attempt. A controller claims a batch
//! before printing it, so another waiter cannot deliver the same messages.
mod receipt;
pub use receipt::{Confirm, ACK_USAGE, RESOLVE_USAGE};
use receipt::{Delivery, Resolution, State};

use super::{state_dir, valid_id};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const DONE_USAGE: &str = "wfc done --report FILE [--attempt ID] [--message-id KEY]";
pub const WAIT_USAGE: &str = "wfc wait [--timeout SECONDS] [--dispatch ID]";

fn options<'a, const N: usize>(
    args: &'a [OsString],
    names: [&str; N],
    mut positional: Option<&mut Option<&'a str>>,
) -> Result<[Option<&'a str>; N]> {
    let mut values = [None; N];
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        let flag = token.to_str().context("expected UTF-8 argument")?;
        if let Some(slot) = positional.as_deref_mut() {
            if !flag.starts_with('-') {
                ensure!(
                    !flag.is_empty() && slot.replace(flag).is_none(),
                    "expected one message id"
                );
                continue;
            }
        }
        let index = names
            .iter()
            .position(|name| flag == *name)
            .with_context(|| format!("unknown option {flag:?}"))?;
        let value = tokens
            .next()
            .context("option requires a value")?
            .to_str()
            .context("expected UTF-8 option value")?;
        ensure!(!value.is_empty() || flag == "--body", "empty option value");
        ensure!(
            positional.is_none() || !value.starts_with("--"),
            "option requires a value"
        );
        ensure!(
            values[index].replace(value).is_none(),
            "duplicate option {flag:?}"
        );
    }
    Ok(values)
}

#[derive(Serialize, Deserialize)]
struct Identity {
    dispatch: String,
    round: u64,
    attempt: String,
    role: String,
}

impl Identity {
    fn from_worker(state: &Path, expected_attempt: Option<&str>) -> Result<Self> {
        let dispatch = env::var("WFC_DISPATCH")
            .context("run done inside a dispatched worker (WFC_DISPATCH)")?;
        ensure!(valid_id(&dispatch), "invalid dispatch ID");
        let round: u64 = env::var("WFC_ROUND")
            .context("missing worker round")?
            .parse()
            .context("invalid worker round")?;
        let attempt = env::var("WFC_ATTEMPT").context("missing worker attempt")?;
        let dir = state.join("dispatches");
        // Foreground dispatch writes and syncs .pending before launching us,
        // then renames it to .json. This order of opens also covers the rename.
        let file = match File::open(dir.join(format!("{dispatch}.pending"))) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                File::open(dir.join(format!("{dispatch}.json")))
            }
            result => result,
        }
        .with_context(|| format!("open dispatch {dispatch}"))?;
        let identity: Self = serde_json::from_reader(file).context("read worker identity")?;
        ensure!(
            identity.dispatch == dispatch && identity.round == round,
            "dispatch or round mismatch for {dispatch}"
        );
        ensure!(
            identity.attempt == attempt && expected_attempt.is_none_or(|id| id == attempt),
            "attempt is no longer accepted for {dispatch}"
        );
        Ok(identity)
    }
}

#[derive(Serialize, Deserialize)]
struct Completion {
    id: u64,
    #[serde(rename = "messageId")]
    message_id: String,
    #[serde(flatten)]
    worker: Identity,
    report: PathBuf,
    request_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_hash: Option<String>,
    #[serde(flatten)]
    delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution: Option<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    // Keep additive fields when rewriting a receipt from a newer publisher.
    #[serde(flatten)]
    additional_fields: Map<String, Value>,
}

fn body_hash(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

impl Completion {
    // The immutable done payload. Wait, history and body verification all use
    // these stored fields, including when the original report has disappeared.
    fn body(&self) -> Map<String, Value> {
        let mut body = Map::from_iter([
            ("attempt".into(), json!(self.worker.attempt)),
            ("report".into(), json!(self.report)),
            ("role".into(), json!(self.worker.role)),
            ("round".into(), json!(self.worker.round)),
        ]);
        for key in ["scope", "head", "dirty"] {
            if let Some(value) = self.additional_fields.get(key) {
                body.insert(key.into(), value.clone());
            }
        }
        // The scope-aware publisher uses the dispatch ID for older rows
        // without a scope when it writes their (possibly null) Git snapshot.
        if body.contains_key("head") && !body.contains_key("scope") {
            body.insert("scope".into(), json!(self.worker.dispatch));
        }
        body
    }

    fn canonical_body(&self) -> String {
        // Python #40 uses sorted keys, UTF-8, and a space after each separator.
        // All done payload fields are scalar. Map iterates in sorted key order.
        let fields: Vec<_> = self
            .body()
            .into_iter()
            .map(|(key, value)| format!("{}: {value}", json!(key)))
            .collect();
        format!("{{{}}}", fields.join(", "))
    }

    fn computed_hash(&self) -> String {
        body_hash(&self.canonical_body())
    }

    fn matches_hash(&self, hash: &str) -> bool {
        self.body_hash
            .as_deref()
            .is_none_or(|stored| stored == hash)
            && self.computed_hash() == hash
    }

    fn receipt(&self, duplicate: bool) -> Value {
        let mut receipt = json!({"message": self.id, "messageId": self.message_id, "state": self.delivery.state,
            "statusVersion": self.delivery.status_version, "duplicate": duplicate});
        if let Some(resolution) = &self.resolution {
            receipt["resolution"] = json!(resolution);
        }
        receipt
    }

    fn view(&self) -> Value {
        let mut view = self.body();
        view.extend([
            ("id".into(), json!(self.id)),
            ("messageId".into(), json!(self.message_id)),
            ("dispatch".into(), json!(self.worker.dispatch)),
            ("type".into(), json!("done")),
            ("state".into(), json!(self.delivery.state)),
            ("statusVersion".into(), json!(self.delivery.status_version)),
        ]);
        Value::Object(view)
    }

    fn receive(&mut self) -> Result<Value> {
        self.delivery.advance(State::Received)?;
        Ok(self.view())
    }
}

fn private_file() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

struct Inbox {
    dir: PathBuf,
    messages: Vec<Completion>,
    // The lock lives on a stable file because save replaces the data file.
    _lock: File,
}

fn lock_file(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir).context("create message directory")?;
    private_file()
        .create(true)
        .open(dir.join("messages.lock"))
        .context("open message lock")
}

impl Inbox {
    fn snapshot(dir: &Path) -> Result<Vec<Completion>> {
        let path = dir.join("messages.json");
        match File::open(&path) {
            Ok(file) => {
                serde_json::from_reader(BufReader::new(file)).context("read controller messages")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn open(dir: &Path) -> Result<Self> {
        let lock = lock_file(dir)?;
        lock.lock().context("lock controller messages")?;
        Self::read_locked(dir, lock)
    }

    fn read_locked(dir: &Path, lock: File) -> Result<Self> {
        let messages = Self::snapshot(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            messages,
            _lock: lock,
        })
    }

    fn save(&self) -> Result<()> {
        let tmp = self.dir.join("messages.pending");
        // A crash before rename can only leave a scratch file. The next writer
        // holds the same lock and rewrites it from the committed snapshot.
        match fs::remove_file(&tmp) {
            Ok(()) => (),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let file = private_file().create_new(true).open(&tmp)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &self.messages)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        fs::rename(tmp, self.dir.join("messages.json"))?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

// Writers publish complete snapshots by rename. A status reader can open
// either generation without taking a lock or touching the state directory.
pub(super) fn history(state: &Path, dispatch: &str) -> Result<Vec<Value>> {
    Ok(Inbox::snapshot(state)?
        .into_iter()
        .filter(|m| m.worker.dispatch == dispatch)
        .map(|m| {
            let mut view = m.view();
            view["to"] = json!("controller");
            view["delivered"] = json!(m.delivery.delivered());
            view["at"] = json!(m.created_at);
            if let Some(resolution) = &m.resolution {
                view["resolution"] = json!(resolution);
            }
            view
        })
        .collect())
}

pub struct Done<'a> {
    report: &'a Path,
    attempt: Option<&'a str>,
    key: Option<&'a str>,
}

impl<'a> Done<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [report, attempt, key] =
            options(args, ["--report", "--attempt", "--message-id"], None)?;
        Ok(Self {
            report: Path::new(report.context("missing --report")?),
            attempt,
            key,
        })
    }

    pub fn run(self) -> Result<Value> {
        let report = self
            .report
            .canonicalize()
            .with_context(|| format!("report not found: {}", self.report.display()))?;
        ensure!(report.is_file(), "report not found: {}", report.display());
        ensure!(report.to_str().is_some(), "report path must be UTF-8");
        let state = state_dir()?;
        let mut inbox = Inbox::open(&state)?;
        let worker = Identity::from_worker(&state, self.attempt)?;
        let key = self
            .key
            .map(str::to_owned)
            .unwrap_or_else(|| format!("done:{}:{}", worker.dispatch, worker.attempt));
        if let Some(prior) = inbox.messages.iter().find(|m| {
            m.worker.dispatch == worker.dispatch
                && (m.request_key.as_deref() == Some(key.as_str())
                    || m.worker.attempt == worker.attempt)
        }) {
            ensure!(
                prior.worker.attempt == worker.attempt
                    && prior.worker.round == worker.round
                    && prior.report == report
                    && prior.request_key.as_deref() == Some(key.as_str()),
                "message request {key} conflicts with an existing receipt"
            );
            return Ok(prior.receipt(true));
        }
        let mut message = Completion {
            id: inbox.messages.len() as u64 + 1,
            message_id: format!("m-{}", Uuid::new_v4().simple()),
            worker,
            report,
            request_key: Some(key),
            body_hash: None,
            delivery: Delivery::unsent(),
            resolution: None,
            created_at: Some(receipt::timestamp()),
            additional_fields: Map::new(),
        };
        message.body_hash = Some(message.computed_hash());
        let receipt = message.receipt(false);
        inbox.messages.push(message);
        inbox.save().context("publish completion")?;
        Ok(receipt)
    }
}

pub struct Wait<'a> {
    timeout: Duration,
    dispatch: Option<&'a str>,
}

impl<'a> Wait<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [timeout, dispatch] = options(args, ["--timeout", "--dispatch"], None)?;
        let seconds: f64 = timeout
            .unwrap_or("3600")
            .parse()
            .context("invalid timeout")?;
        ensure!(seconds.is_finite(), "expected a finite timeout");
        let timeout = Duration::try_from_secs_f64(seconds.max(0.0)).context("invalid timeout")?;
        ensure!(dispatch.is_none_or(valid_id), "invalid dispatch ID");
        Ok(Self { timeout, dispatch })
    }

    fn take(&self, state: &Path) -> Result<Vec<Value>> {
        let lock = lock_file(state)?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(Vec::new()),
            Err(TryLockError::Error(e)) => return Err(e).context("lock controller messages"),
        }
        let mut inbox = Inbox::read_locked(state, lock)?;
        let batch: Vec<_> = inbox
            .messages
            .iter_mut()
            .filter(|m| {
                m.delivery.state == State::Unsent
                    && self.dispatch.is_none_or(|id| id == m.worker.dispatch)
            })
            .map(Completion::receive)
            .collect::<Result<_>>()?;
        if !batch.is_empty() {
            inbox.save().context("claim controller messages")?;
        }
        Ok(batch)
    }

    pub fn run(self) -> Result<Vec<Value>> {
        let state = state_dir()?;
        let start = Instant::now();
        loop {
            let batch = self.take(&state)?;
            let remaining = self.timeout.saturating_sub(start.elapsed());
            if !batch.is_empty() || remaining.is_zero() {
                return Ok(batch);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}
