//! A worker can retry a keyed completion. A controller claims a batch
//! before printing it, so another waiter cannot deliver the same messages.
use super::{git_snapshot::Snapshot, resolve_report_path, state_dir, valid_id};
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
pub const WAIT_USAGE: &str = "wfc wait [--timeout SECONDS] [--scope NAME] [--dispatch ID]";

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
    fn current(state: &Path, dispatch: &str, expected_attempt: Option<&str>) -> Result<Self> {
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
        let assignment: Self = serde_json::from_reader(file).context("read worker assignment")?;
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
        Ok(assignment)
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
    #[serde(flatten)]
    snapshot: Snapshot,
    received: bool,
}

impl Completion {
    fn delivery(&self) -> (&'static str, u8) {
        if self.received {
            ("received", 2)
        } else {
            ("unsent", 1)
        }
    }

    fn receipt(&self, duplicate: bool) -> Value {
        let (state, version) = self.delivery();
        json!({"message": self.id, "messageId": self.message_id, "state": state,
            "statusVersion": version, "duplicate": duplicate, "scope": self.worker.scope()})
    }

    fn receive(&mut self) -> Value {
        self.received = true;
        let (state, version) = self.delivery();
        json!({"id": self.id, "messageId": self.message_id, "dispatch": self.worker.dispatch,
            "type": "done", "state": state, "statusVersion": version,
            "scope": self.worker.scope(), "head": self.snapshot.head, "dirty": self.snapshot.dirty,
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

    fn validate_done(
        &self,
        assignment: &Assignment,
        report: &Path,
        key: Option<&str>,
        attempt: Option<&str>,
    ) -> Result<Option<Value>> {
        let worker = &assignment.worker;
        let history = || {
            self.messages
                .iter()
                .filter(|m| m.worker.dispatch == worker.dispatch)
        };
        let prior = key.and_then(|key| history().find(|m| m.request_key.as_deref() == Some(key)));
        if let (Some(key), Some(prior)) = (key, prior) {
            ensure!(
                prior.worker.attempt == worker.attempt
                    && prior.worker.round == worker.round
                    && prior.report == report,
                "message request {key} conflicts with an existing receipt"
            );
        }
        let previous = history()
            .find(|m| m.worker.round == worker.round && m.worker.attempt == worker.attempt);
        if let Some(bound) = assignment
            .report
            .as_deref()
            .or_else(|| previous.map(|m| m.report.as_path()))
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
                    && m.report == report),
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
            let lock = lock_file(&state)?;
            lock.lock().context("lock controller messages")?;
            let assignment = Assignment::current(&state, &dispatch, self.attempt)?;
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
            let inbox = Inbox::read_locked(&state, lock)?;
            let receipt =
                inbox.validate_done(&assignment, &report, key.as_deref(), self.attempt)?;
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
        let message = Completion {
            id: inbox.messages.len() as u64 + 1,
            message_id: format!("m-{}", Uuid::new_v4().simple()),
            worker: current.worker,
            snapshot,
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
    scope: Option<&'a str>,
}

impl<'a> Wait<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [timeout, dispatch, scope] = options(args, ["--timeout", "--dispatch", "--scope"])?;
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
                !m.received
                    && self.dispatch.is_none_or(|id| id == m.worker.dispatch)
                    && self.scope.is_none_or(|scope| scope == m.worker.scope())
            })
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
