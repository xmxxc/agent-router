//! A worker posts one completion for its attempt. A controller claims a batch
//! before printing it, so another waiter cannot deliver the same messages.
use super::{state_dir, valid_id};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
) -> Result<[Option<&'a str>; N]> {
    let mut values = [None; N];
    for pair in args.chunks(2) {
        let [flag, value] = pair else {
            anyhow::bail!("option requires a value");
        };
        let index = names
            .iter()
            .position(|name| flag == name)
            .with_context(|| format!("unknown option {flag:?}"))?;
        let value = value
            .to_str()
            .filter(|v| !v.is_empty())
            .context("expected a nonempty UTF-8 value")?;
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
    request_key: String,
    received: bool,
}

impl Completion {
    fn receipt(&self, duplicate: bool) -> Value {
        let (state, version) = if self.received {
            ("received", 2)
        } else {
            ("unsent", 1)
        };
        json!({"message": self.id, "messageId": self.message_id, "state": state,
            "statusVersion": version, "duplicate": duplicate})
    }

    fn receive(&mut self) -> Value {
        self.received = true;
        json!({"id": self.id, "messageId": self.message_id, "dispatch": self.worker.dispatch,
            "type": "done", "state": "received", "statusVersion": 2,
            "report": self.report, "role": self.worker.role, "round": self.worker.round,
            "attempt": self.worker.attempt})
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
    fn read_locked(dir: &Path, lock: File) -> Result<Self> {
        let path = dir.join("messages.json");
        let messages = match File::open(&path) {
            Ok(file) => {
                serde_json::from_reader(BufReader::new(file)).context("read controller messages")?
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
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

pub struct Done<'a> {
    report: &'a Path,
    attempt: Option<&'a str>,
    key: Option<&'a str>,
}

impl<'a> Done<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [report, attempt, key] = options(args, ["--report", "--attempt", "--message-id"])?;
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
        let lock = lock_file(&state)?;
        lock.lock().context("lock controller messages")?;
        let worker = Identity::from_worker(&state, self.attempt)?;
        let key = self
            .key
            .map(str::to_owned)
            .unwrap_or_else(|| format!("done:{}:{}", worker.dispatch, worker.attempt));
        let mut inbox = Inbox::read_locked(&state, lock)?;
        if let Some(prior) = inbox.messages.iter().find(|m| {
            m.worker.dispatch == worker.dispatch
                && (m.request_key == key || m.worker.attempt == worker.attempt)
        }) {
            ensure!(
                prior.worker.attempt == worker.attempt
                    && prior.worker.round == worker.round
                    && prior.report == report
                    && prior.request_key == key,
                "message request {key} conflicts with an existing receipt"
            );
            return Ok(prior.receipt(true));
        }
        let message = Completion {
            id: inbox.messages.len() as u64 + 1,
            message_id: format!("m-{}", Uuid::new_v4().simple()),
            worker,
            report,
            request_key: key,
            received: false,
        };
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
        let [timeout, dispatch] = options(args, ["--timeout", "--dispatch"])?;
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
            .filter(|m| !m.received && self.dispatch.is_none_or(|id| id == m.worker.dispatch))
            .map(Completion::receive)
            .collect();
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
