//! A worker can retry a keyed completion. A controller claims a batch
//! before printing it, so another waiter cannot deliver the same messages.
mod receipt;
#[cfg(target_os = "linux")]
pub mod relay;
pub use receipt::{Confirm, ACK_USAGE, RESOLVE_USAGE};
use receipt::{Delivery, Resolution, State};

use super::{git_snapshot::Snapshot, resolve_report_path, state_dir, valid_id};
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
pub const WAIT_USAGE: &str = "wfc wait [--timeout SECONDS] [--scope NAME] [--dispatch ID]";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

impl Identity {
    fn scope(&self) -> &str {
        // Pre-scope receipts still have a stable, per-dispatch scope.
        self.scope.as_deref().unwrap_or(&self.dispatch)
    }
}

#[derive(Deserialize)]
struct Assignment {
    #[serde(flatten)]
    worker: Identity,
    report: Option<PathBuf>,
    cwd: PathBuf,
    #[serde(rename = "controllerOwner")]
    controller_owner: Option<Value>,
}

impl Assignment {
    fn current(
        state: &Path,
        dispatch: &str,
        expected_attempt: Option<&str>,
    ) -> Result<(Self, Value)> {
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
        let row: Value = serde_json::from_reader(file).context("read worker assignment")?;
        let assignment = Self::deserialize(&row).context("read worker assignment identity")?;
        let worker = &assignment.worker;
        ensure!(
            worker.dispatch == dispatch,
            "dispatch mismatch for {dispatch}"
        );
        ensure!(
            assignment.controller_owner.is_none(),
            "controller owner authorization required for {dispatch}"
        );
        if let Some(round) = env::var_os("WFC_ROUND") {
            ensure!(
                round.to_str().and_then(|s| s.parse::<u64>().ok()) == Some(worker.round),
                "round mismatch for {dispatch}"
            );
        }
        ensure!(
            env::var_os("WFC_ATTEMPT").is_none_or(|v| v.to_str() == Some(worker.attempt.as_str()))
                && expected_attempt.is_none_or(|id| id == worker.attempt),
            "attempt is no longer accepted for {dispatch}"
        );
        Ok((assignment, row))
    }
}

#[derive(Serialize, Deserialize)]
struct Message {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    message_type: Option<String>,
    id: u64,
    #[serde(rename = "messageId")]
    message_id: String,
    #[serde(flatten)]
    worker: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<PathBuf>,
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

fn canonical_body(body: Map<String, Value>) -> String {
    // Python #40 uses sorted keys, UTF-8, and a space after each separator.
    // Map iterates in sorted key order.
    let fields: Vec<_> = body
        .into_iter()
        .map(|(key, value)| format!("{}: {}", json!(key), canonical_value(value)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

fn canonical_value(value: Value) -> String {
    match value {
        Value::Object(fields) => canonical_body(fields),
        Value::Array(values) => format!(
            "[{}]",
            values
                .into_iter()
                .map(canonical_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        scalar => scalar.to_string(),
    }
}

impl Message {
    fn kind(&self) -> &str {
        self.message_type.as_deref().unwrap_or_else(|| {
            if self
                .additional_fields
                .get("question_payload")
                .is_some_and(Value::is_object)
            {
                "question"
            } else {
                "done"
            }
        })
    }

    fn original_body(&self) -> Map<String, Value> {
        let mut body = Map::from_iter([
            ("attempt".into(), json!(self.worker.attempt)),
            ("role".into(), json!(self.worker.role)),
            ("round".into(), json!(self.worker.round)),
        ]);
        if let Some(report) = &self.report {
            body.insert("report".into(), json!(report));
        }
        body
    }

    fn body(&self) -> Map<String, Value> {
        let mut body = self.original_body();
        if self.kind() != "done" {
            body.insert("scope".into(), json!(self.worker.scope()));
            if let Some(question) = self.additional_fields.get("question_payload") {
                body.insert("question".into(), question["text"].clone());
                body.insert("options".into(), question["options"].clone());
            }
            for key in ["kind", "host", "message", "fault"] {
                if let Some(value) = self.additional_fields.get(key) {
                    body.insert(key.into(), value.clone());
                }
            }
            return body;
        }
        for key in ["head", "dirty"] {
            if let Some(value) = self.additional_fields.get(key) {
                body.insert(key.into(), value.clone());
            }
        }
        // The scope-aware publisher uses the dispatch ID for older rows
        // without a scope when it writes their (possibly null) Git snapshot.
        if self.worker.scope.is_some() || body.contains_key("head") {
            body.insert("scope".into(), json!(self.worker.scope()));
        }
        body
    }

    fn computed_hash(&self) -> String {
        body_hash(&canonical_body(self.body()))
    }

    fn matches_hash(&self, hash: &str) -> bool {
        self.body_hash
            .as_deref()
            .is_none_or(|stored| stored == hash)
            && self.computed_hash() == hash
    }

    fn could_survive_legacy_write(&self) -> bool {
        // The four-field publisher read the whole inbox, requiring a string
        // request key on every row. Its rewrite dropped newer fields. Only
        // publication-time values count: later claims can bind a digest or
        // add null Git fields even on a genuine legacy row.
        self.kind() == "done"
            && self.request_key.is_some()
            && self.worker.scope.is_none()
            && self.created_at.is_none()
            && self
                .additional_fields
                .get("head")
                .is_none_or(Value::is_null)
            && self
                .additional_fields
                .get("dirty")
                .is_none_or(Value::is_null)
    }

    fn matches_ack_body(&self, body: &str, earlier: &[Self]) -> bool {
        if self.matches_hash(&body_hash(body)) {
            return true;
        }
        // A pre-scope message could already have been received when a later
        // publisher rewrote the inbox, adding the two null Git fields. That
        // row is indistinguishable from one claimed after the rewrite. The
        // original publisher accepted only one completion per dispatch and
        // attempt, and its write had to preserve the preceding publications.
        self.body_hash.is_none()
            && self.could_survive_legacy_write()
            && self.additional_fields.contains_key("head")
            && self.additional_fields.contains_key("dirty")
            && body == canonical_body(self.original_body())
            && earlier.iter().all(|m| {
                m.could_survive_legacy_write()
                    && (m.worker.dispatch != self.worker.dispatch
                        || m.worker.attempt != self.worker.attempt)
            })
    }

    fn receipt(&self, duplicate: bool) -> Value {
        let mut receipt = json!({"message": self.id, "messageId": self.message_id, "state": self.delivery.state,
            "statusVersion": self.delivery.status_version, "duplicate": duplicate, "scope": self.worker.scope()});
        if let Some(resolution) = &self.resolution {
            receipt["resolution"] = json!(resolution);
        }
        receipt
    }

    fn view(&self) -> Value {
        self.view_at(self.delivery.state, self.delivery.status_version)
    }

    fn view_at(&self, state: State, version: u64) -> Value {
        let mut view = self.body();
        view.extend([
            ("id".into(), json!(self.id)),
            ("messageId".into(), json!(self.message_id)),
            ("dispatch".into(), json!(self.worker.dispatch)),
            ("type".into(), json!(self.kind())),
            ("state".into(), json!(state)),
            ("statusVersion".into(), json!(version)),
        ]);
        Value::Object(view)
    }

    fn claim(&mut self, state: State) -> Result<()> {
        self.delivery.advance(state)?;
        if self.body_hash.is_none() {
            self.body_hash = Some(self.computed_hash());
        }
        Ok(())
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
    messages: Vec<Message>,
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

pub(super) fn lock_for_publish(dir: &Path) -> Result<File> {
    let lock = lock_file(dir)?;
    lock.lock().context("lock controller messages")?;
    Ok(lock)
}

impl Inbox {
    fn snapshot(dir: &Path) -> Result<Vec<Message>> {
        let path = dir.join("messages.json");
        match File::open(&path) {
            Ok(file) => {
                let mut messages: Vec<Message> = serde_json::from_reader(BufReader::new(file))
                    .context("read controller messages")?;
                // The first scoped wait on an unclaimed pre-snapshot completion
                // supplies an unknown Git snapshot. Preserve an already bound
                // body (including the earlier four-field form) byte for byte.
                for m in &mut messages {
                    if m.delivery.legacy_unclaimed
                        && m.body_hash.is_none()
                        && m.worker.scope.is_none()
                        && !m.additional_fields.contains_key("head")
                        && !m.additional_fields.contains_key("dirty")
                    {
                        m.additional_fields.insert("head".into(), Value::Null);
                        m.additional_fields.insert("dirty".into(), Value::Null);
                    }
                }
                Ok(messages)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn open(dir: &Path) -> Result<Self> {
        Self::read_locked(dir, lock_for_publish(dir)?)
    }

    fn read_locked(dir: &Path, lock: File) -> Result<Self> {
        let messages = Self::snapshot(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            messages,
            _lock: lock,
        })
    }

    fn try_open(state: &Path) -> Result<Option<Self>> {
        let lock = lock_file(state)?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e).context("lock controller messages"),
        }
        Ok(Some(Self::read_locked(state, lock)?))
    }

    fn claim(
        &mut self,
        dispatch: Option<&str>,
        scope: Option<&str>,
        state: State,
    ) -> Result<Vec<Value>> {
        let batch: Vec<_> = self
            .messages
            .iter_mut()
            .filter(|m| {
                m.delivery.state == State::Unsent
                    && dispatch.is_none_or(|id| id == m.worker.dispatch)
                    && scope.is_none_or(|scope| scope == m.worker.scope())
            })
            .map(|m| {
                m.claim(state)?;
                Ok(m.view())
            })
            .collect::<Result<_>>()?;
        if !batch.is_empty() {
            self.save().context("claim controller messages")?;
        }
        Ok(batch)
    }

    fn validate_done(
        &self,
        assignment: &Assignment,
        report: &Path,
        key: Option<&str>,
        attempt: Option<&str>,
    ) -> Result<Option<Value>> {
        let worker = &assignment.worker;
        let for_dispatch = || {
            self.messages
                .iter()
                .filter(|m| m.worker.dispatch == worker.dispatch)
        };
        let history = || for_dispatch().filter(|m| m.kind() == "done");
        let prior =
            key.and_then(|key| for_dispatch().find(|m| m.request_key.as_deref() == Some(key)));
        if let (Some(key), Some(prior)) = (key, prior) {
            ensure!(
                prior.kind() == "done"
                    && prior.worker.attempt == worker.attempt
                    && prior.worker.round == worker.round
                    && prior.report.as_deref() == Some(report),
                "message request {key} conflicts with an existing receipt"
            );
        }
        let previous = history()
            .find(|m| m.worker.round == worker.round && m.worker.attempt == worker.attempt);
        if let Some(bound) = assignment
            .report
            .as_deref()
            .or_else(|| previous.and_then(|m| m.report.as_deref()))
        {
            ensure!(
                report == bound,
                "the current round reports to {}",
                bound.display()
            );
        } else {
            // Pre-binding Rust records carry no intended filename. Only the original
            // attempt can establish it, and concurrent first submissions recheck it.
            ensure!(
                attempt.is_some() || env::var_os("WFC_ATTEMPT").is_some(),
                "legacy dispatch has no bound report; supply --attempt"
            );
        }
        if let Some(prior) = prior {
            return Ok(Some(prior.receipt(true)));
        }
        if attempt.is_none() {
            ensure!(
                !history().any(|m| (m.worker.round != worker.round
                    || m.worker.attempt != worker.attempt)
                    && m.report.as_deref() == Some(report)),
                "ambiguous report for {}; supply --attempt",
                worker.dispatch
            );
            let earlier_rounds: std::collections::BTreeSet<_> = history()
                .map(|m| m.worker.round)
                .filter(|round| (1..worker.round).contains(round))
                .collect();
            ensure!(
                worker.round.checked_sub(1) == Some(earlier_rounds.len() as u64),
                "incomplete round history for {}; supply --attempt",
                worker.dispatch
            );
        }
        Ok(None)
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
        self.sync_directory()
    }

    fn sync_directory(&self) -> Result<()> {
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn publish_startup_failure(
    state: &Path,
    dispatch: &str,
    attempt: &str,
    failure: &super::startup::Failure<'_>,
) -> Result<()> {
    ensure!(
        valid_id(dispatch) && super::valid_identity(attempt, "a-"),
        "invalid startup identity"
    );
    let mut inbox = Inbox::open(state)?;
    let dir = state.join("dispatches");
    let (file, path) = super::assignment::open_assignment(&dir, dispatch)?;
    let row = super::decode_record(file, &path)?;
    let assignment = Assignment::deserialize(&row)?;
    let worker = assignment.worker;
    ensure!(
        worker.attempt == attempt && worker.round > 0 && assignment.controller_owner.is_none(),
        "startup failure does not match the current assignment"
    );
    let key = format!("startup:{dispatch}:{}:{attempt}", worker.round);
    if inbox.messages.iter().any(|m| {
        m.worker.dispatch == worker.dispatch
            && m.worker.attempt == worker.attempt
            && m.worker.round == worker.round
            && (m.request_key.as_deref() == Some(&key)
                || m.additional_fields
                    .get("kind")
                    .is_some_and(|kind| kind == "startup_failure")
                || matches!(m.kind(), "done" | "question"))
    }) {
        return Ok(());
    }
    let mut message = Message {
        message_type: Some(failure.message_type().into()),
        id: inbox.messages.len() as u64 + 1,
        message_id: format!("m-{}", Uuid::new_v4().simple()),
        worker,
        report: assignment.report,
        request_key: Some(key),
        body_hash: None,
        delivery: Delivery::unsent(),
        resolution: None,
        created_at: Some(receipt::timestamp()),
        additional_fields: Map::from_iter([
            ("kind".into(), json!("startup_failure")),
            ("host".into(), row["host"].clone()),
            ("message".into(), json!(failure.diagnostic())),
            ("fault".into(), json!(failure)),
        ]),
    };
    message.body_hash = Some(message.computed_hash());
    inbox.messages.push(message);
    inbox.save().context("publish startup failure")
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
        let report = resolve_report_path(self.report)?
            .canonicalize()
            .with_context(|| format!("report not found: {}", self.report.display()))?;
        ensure!(report.is_file(), "report not found: {}", report.display());
        ensure!(report.to_str().is_some(), "report path must be UTF-8");
        let state = state_dir()?;
        let dispatch = env::var("WFC_DISPATCH")
            .context("run done inside a dispatched worker (WFC_DISPATCH)")?;
        ensure!(valid_id(&dispatch), "invalid dispatch ID");
        let check = || {
            let inbox = Inbox::open(&state)?;
            let (assignment, row) = Assignment::current(&state, &dispatch, self.attempt)?;
            // R1 dispatches had neither field and derived the default key from
            // the worker's launch environment. Assignment::current checked it.
            let r1_worker = assignment.report.is_none()
                && assignment.worker.scope.is_none()
                && env::var_os("WFC_ROUND").is_some()
                && env::var_os("WFC_ATTEMPT").is_some();
            let keyed_attempt = self
                .attempt
                .or_else(|| r1_worker.then_some(assignment.worker.attempt.as_str()));
            let key = self
                .key
                .map(str::to_owned)
                .or_else(|| keyed_attempt.map(|id| format!("done:{dispatch}:{id}")));
            let receipt =
                inbox.validate_done(&assignment, &report, key.as_deref(), self.attempt)?;
            if receipt.is_none() && row["backend"] == "herdr" {
                ensure!(
                    !super::backend::herdr_close_blocks_done(&state, &row)?,
                    "attempt {} is no longer accepted for {dispatch}; its execution resources are closing or closed",
                    assignment.worker.attempt
                );
            }
            Ok::<_, anyhow::Error>((assignment, inbox, receipt, key))
        };
        let (initial, inbox, duplicate, _) = check()?;
        if let Some(receipt) = duplicate {
            return Ok(receipt);
        }
        // Git can block. Let unrelated callbacks and waits proceed while sampling.
        drop(inbox);
        let snapshot = Snapshot::read(&initial.cwd);
        let (current, mut inbox, duplicate, key) = check()?;
        ensure!(
            initial.worker.round == current.worker.round
                && initial.worker.attempt == current.worker.attempt
                && initial.cwd == current.cwd
                && initial.worker.scope() == current.worker.scope(),
            "assignment changed while sampling {dispatch}"
        );
        if let Some(receipt) = duplicate {
            return Ok(receipt);
        }
        let mut message = Message {
            message_type: None,
            id: inbox.messages.len() as u64 + 1,
            message_id: format!("m-{}", Uuid::new_v4().simple()),
            worker: current.worker,
            report: Some(report),
            request_key: key,
            body_hash: None,
            delivery: Delivery::unsent(),
            resolution: None,
            created_at: Some(receipt::timestamp()),
            additional_fields: Map::from_iter([
                ("head".into(), json!(snapshot.head)),
                ("dirty".into(), json!(snapshot.dirty)),
            ]),
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
    scope: Option<&'a str>,
}

impl<'a> Wait<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [timeout, dispatch, scope] =
            options(args, ["--timeout", "--dispatch", "--scope"], None)?;
        let seconds: f64 = timeout
            .unwrap_or("3600")
            .parse()
            .context("invalid timeout")?;
        ensure!(seconds.is_finite(), "expected a finite timeout");
        let timeout = Duration::try_from_secs_f64(seconds.max(0.0)).context("invalid timeout")?;
        ensure!(dispatch.is_none_or(valid_id), "invalid dispatch ID");
        Ok(Self {
            timeout,
            dispatch,
            scope,
        })
    }

    pub fn run(self) -> Result<Vec<Value>> {
        let state = state_dir()?;
        let start = Instant::now();
        loop {
            let batch = match Inbox::try_open(&state)? {
                Some(mut inbox) => inbox.claim(self.dispatch, self.scope, State::Received)?,
                None => Vec::new(),
            };
            let remaining = self.timeout.saturating_sub(start.elapsed());
            if !batch.is_empty() || remaining.is_zero() {
                return Ok(batch);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}
