//! Observe the existing worker binding while a controller is waiting.
use super::{body_hash, receipt, Assignment, Delivery, Identity, Inbox, Message};
use crate::workflow::{assignment, backend, herdr, valid_id, view, write_json};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::VecDeque,
    env,
    fs::{self, TryLockError},
    io::ErrorKind,
    path::Path,
    task::Poll,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

mod probe;
pub(crate) use probe::{run_runtime_owner, run_runtime_probe};
use probe::{Observation, Probe};

const OBSERVATION: &str = "host_runtime";

pub(super) fn seconds(value: &str) -> Result<Duration> {
    Duration::try_from_secs_f64(value.parse()?).context("expected finite nonnegative seconds")
}

pub(super) struct Monitor {
    next: Instant,
    stall: Duration,
    deadline: Duration,
    pending: VecDeque<String>,
    probe: Option<Probe>,
}

impl Monitor {
    pub(super) fn new(stall: Duration) -> Self {
        let deadline = env::var("WFC_MAX_TURN_SECONDS")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .and_then(|value| Duration::try_from_secs_f64(value.max(0.0)).ok())
            .unwrap_or(Duration::from_secs(3600));
        Self {
            next: Instant::now(),
            stall,
            deadline,
            pending: VecDeque::new(),
            probe: None,
        }
    }

    pub(super) fn delay(&self, default: Duration) -> Duration {
        if self.probe.is_some() {
            default.min(Duration::from_millis(25))
        } else {
            default
        }
    }

    pub(super) fn cancel_probe(&mut self) {
        self.probe = None;
    }

    pub(super) fn poll(
        &mut self,
        state: &Path,
        dispatch: Option<&str>,
        scope: Option<&str>,
        remaining: Duration,
    ) -> Result<bool> {
        let entered = Instant::now();
        if remaining.is_zero() || env::var_os("WFC_WORKER_CALLBACK").is_some_and(|v| !v.is_empty())
        {
            return Ok(false);
        }
        if let Some(mut running) = self.probe.take() {
            match running.poll()? {
                Poll::Pending => {
                    self.probe = Some(running);
                    return Ok(false);
                }
                Poll::Ready(Some(observed)) if entered.elapsed() < remaining => {
                    if self.commit(
                        state,
                        scope,
                        &running.id,
                        running.previous.as_ref(),
                        observed,
                    )? {
                        return Ok(true);
                    }
                }
                Poll::Ready(_) => (),
            }
        }
        let dir = state.join("dispatches");
        if self.pending.is_empty() {
            if Instant::now() < self.next {
                return Ok(false);
            }
            self.next = Instant::now() + Duration::from_secs(1);
            self.pending = match dispatch {
                Some(id) => [id.to_owned()].into(),
                None => assignment::ids(&dir)?.into_iter().collect(),
            };
        }
        let ledger = Inbox::snapshot(state)?;
        while let Some(id) = self.pending.pop_front() {
            let Some(row) = candidate(&dir, &id, scope) else {
                continue;
            };
            if finished(&row, &history(&row, &ledger)?) {
                continue;
            }
            let previous = watch(&dir.join(format!("{id}.host-watch")))?;
            let salt = watch_for(&row, previous.as_ref())
                .map_or_else(|| Uuid::new_v4().simple().to_string(), |w| w.salt.clone());
            let budget = remaining.saturating_sub(entered.elapsed());
            if budget.is_zero() {
                break;
            }
            self.probe = Some(Probe::start(state, &row, previous, &salt, budget)?);
            break;
        }
        Ok(false)
    }

    fn commit(
        &self,
        state: &Path,
        scope: Option<&str>,
        id: &str,
        previous: Option<&Watch>,
        observed: Observation,
    ) -> Result<bool> {
        let dir = state.join("dispatches");
        let path = dir.join(format!("{id}.host-watch"));
        let Some(row) = candidate(&dir, id, scope) else {
            return Ok(false);
        };
        let matches_observation = |row: &Value| {
            let current = if observed.timed_out {
                execution_binding(row)
            } else {
                binding(row)
            };
            current == observed.binding
        };
        if !matches_observation(&row) {
            return Ok(false);
        }
        // Same lock order as Herdr close: execution first, then its messages.
        // Contention defers an observation while the controller can keep polling.
        let close_guard = if row["backend"] == "herdr" {
            herdr::open_runtime_lock(state, &row)?
        } else {
            assignment::open_close_lock(state, &row)?
        };
        match close_guard.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let Some(mut inbox) = Inbox::try_open(state)? else {
            return Ok(false);
        };
        let Some(row) = candidate(&dir, id, scope) else {
            return Ok(false);
        };
        if !matches_observation(&row)
            || watch(&path)?.as_ref() != previous
            || backend::runtime_closing(state, &row)?
        {
            return Ok(false);
        }
        let history = history(&row, &inbox.messages)?;
        if finished(&row, &history) {
            return Ok(false);
        }
        let Ok(assignment) = Assignment::deserialize(&row) else {
            return Ok(false);
        };
        let watch = watch_for(&row, previous);
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        // A verified local terminal receipt takes priority over an older
        // screen. Neither terminal checks nor the commit perform backend I/O.
        let (next, notices) = if let Some(exit) = backend::runtime_exit(state, &row)? {
            let exit = Notice::Exited(exit);
            (
                None,
                (!reported(&row, &history, &exit))
                    .then_some(exit)
                    .into_iter()
                    .collect(),
            )
        } else {
            self.evaluate(&row, watch, observed.sample, &history, now)
        };
        let sent = !notices.is_empty();
        for notice in notices {
            append(&mut inbox, assignment.worker.clone(), &row, notice);
        }
        if sent {
            inbox.save().context("publish host observation")?;
        }
        if let Some(next) = next {
            write_json(&path, &serde_json::to_value(next)?, None)?;
            fs::File::open(&dir)?.sync_all()?;
        }
        Ok(sent)
    }

    fn evaluate(
        &self,
        row: &Value,
        previous: Option<&Watch>,
        sample: Sample,
        history: &[Value],
        now: f64,
    ) -> (Option<Watch>, Vec<Notice>) {
        let episode: Vec<_> = current_messages(row, history)
            .rev()
            .filter(|m| m["kind"] == OBSERVATION && !is_read_event(m))
            .take_while(|m| m["fault"]["evidence"] != "changed_activity")
            .collect();
        let seen = |n: &Notice| episode.iter().any(|m| same_fault(m, n));
        let fault_open = episode
            .iter()
            .any(|m| m["fault"]["evidence"] != "turn_deadline");
        let was_unreadable = read_event(row, history)
            .is_some_and(|m| m["fault"]["evidence"] == "backend_unavailable");
        let Sample::Screen {
            fingerprint,
            salt,
            fault,
            nonblank,
        } = sample
        else {
            let notice = Notice::Unavailable;
            return (
                None,
                (previous.is_some() && !was_unreadable)
                    .then_some(notice)
                    .into_iter()
                    .collect(),
            );
        };
        let paused = current_messages(row, history).any(|m| m["type"] == "question");
        let changed = previous.is_none_or(|w| w.fingerprint != fingerprint);
        let reset = changed || was_unreadable || previous.is_some_and(|w| w.paused != paused);
        let since = if reset {
            now
        } else {
            previous.map_or(now, |w| w.since)
        };
        let idle = (now - since).max(0.0);
        let total = (now - row["created"].as_f64().unwrap_or(now)).max(0.0);
        let mut notices = Vec::new();
        if let Some(notice) = fault {
            if !seen(&notice) {
                notices.push(notice);
            }
        } else if !paused {
            if changed && nonblank && previous.is_some() && fault_open {
                notices.push(Notice::Recovered);
            } else if !was_unreadable
                && !fault_open
                && !self.stall.is_zero()
                && idle >= self.stall.as_secs_f64()
            {
                notices.push(Notice::Stalled(idle as u64));
            }
        }
        if was_unreadable {
            // A successful capture ends the read interruption on its own.
            // The screen fault and the pending answer have independent histories.
            notices.push(Notice::ReadRestored);
        }
        // A changing quota/error banner does not suspend the total timer.
        let deadline = Notice::Deadline(total as u64);
        if !paused
            && !self.deadline.is_zero()
            && total >= self.deadline.as_secs_f64()
            && !reported(row, history, &deadline)
        {
            notices.push(deadline);
        }
        let next = reset.then(|| Watch {
            at: now,
            attempt: row["attempt"].as_str().unwrap_or_default().into(),
            round: row["round"].as_u64().unwrap_or_default(),
            scope: row_scope(row).unwrap_or_default().into(),
            execution: execution_binding(row),
            salt,
            fingerprint,
            since,
            paused,
        });
        (next, notices)
    }
}

fn candidate(dir: &Path, id: &str, scope: Option<&str>) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    let row = assignment::public_record(dir, id).ok().flatten()?;
    if row["dispatch"] != id
        || row["createPending"] == true
        || row["create"]["result"] != "accepted"
        || !row["controllerOwner"].is_null()
        || !assignment::identified(&row)
        || !matches!(row["host"].as_str(), Some("codex" | "opencode" | "claude"))
        || !scope.is_none_or(|scope| scope == row["scope"].as_str().unwrap_or(id))
        || !match row["backend"].as_str() {
            Some("tmux") => row["executionId"] == row["attempt"] && row["tmuxSocket"].is_string(),
            Some("herdr") => row["backendHandle"].is_object(),
            _ => false,
        }
    {
        return None;
    }
    Some(row)
}

fn binding(row: &Value) -> String {
    let fields = [
        "dispatch",
        "round",
        "attempt",
        "role",
        "scope",
        "created",
        "host",
        "backend",
        "tmuxSocket",
        "executionId",
        "backendHandle",
        "create",
        "endpoint",
        "report",
    ];
    let values = Value::Array(fields.iter().map(|&key| row[key].clone()).collect());
    body_hash(&values.to_string())
}

// Unlike the publication generation, this identity survives new Herdr
// writer checkpoints and still ties the last good read to the original pane.
fn execution_binding(row: &Value) -> String {
    let fields = [
        "dispatch",
        "round",
        "attempt",
        "scope",
        "created",
        "host",
        "backend",
        "tmuxSocket",
        "executionId",
        "create",
        "endpoint",
    ];
    let handle_fields = [
        "service",
        "workspace",
        "pane",
        "terminal",
        "nonce",
        "receipt",
    ];
    let fields = fields.iter().map(|&key| row[key].clone());
    let handle_fields = handle_fields
        .iter()
        .map(|&key| row["backendHandle"][key].clone());
    body_hash(&Value::Array(fields.chain(handle_fields).collect()).to_string())
}

fn row_scope(row: &Value) -> Option<&str> {
    row["scope"].as_str().or_else(|| row["dispatch"].as_str())
}

fn current_messages<'a>(
    row: &'a Value,
    history: &'a [Value],
) -> impl DoubleEndedIterator<Item = &'a Value> {
    history.iter().filter(|m| {
        m["dispatch"] == row["dispatch"]
            && m["round"] == row["round"]
            && m["attempt"] == row["attempt"]
            && row_scope(m) == row_scope(row)
    })
}

fn history(row: &Value, ledger: &[Message]) -> Result<Vec<Value>> {
    view::merged_history(
        row,
        ledger
            .iter()
            .filter(|m| row["dispatch"] == m.worker.dispatch.as_str())
            .map(Message::view)
            .collect(),
    )
}

fn finished(row: &Value, history: &[Value]) -> bool {
    let round = row["rounds"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["round"] == row["round"] && r["attempt"] == row["attempt"]);
    round.is_some_and(|r| matches!(r["state"].as_str(), Some("completed" | "cancelled")))
        || view::round_completion(row, round.unwrap_or(row), history).is_some()
        || current_messages(row, history).any(|m| m["kind"] == "startup_failure")
}

fn is_read_event(message: &Value) -> bool {
    matches!(
        message["fault"]["evidence"].as_str(),
        Some("backend_unavailable" | "read_restored")
    )
}

fn read_event<'a>(row: &Value, history: &'a [Value]) -> Option<&'a Value> {
    // Earlier versions could use changed_activity alone when one capture
    // both restored reading and provided new content.
    let mut old_activity_restore = false;
    for m in history.iter().rev().filter(|m| {
        m["dispatch"] == row["dispatch"]
            && m["round"] == row["round"]
            && m["attempt"] == row["attempt"]
            && row_scope(m) == row_scope(row)
            && m["kind"] == OBSERVATION
    }) {
        match m["fault"]["evidence"].as_str() {
            Some("read_restored") => return Some(m),
            Some("backend_unavailable") => return (!old_activity_restore).then_some(m),
            Some("changed_activity") => old_activity_restore = true,
            _ => (),
        }
    }
    None
}

fn same_fault(message: &Value, notice: &Notice) -> bool {
    message["kind"] == OBSERVATION
        && message["fault"]["category"] == notice.category()
        && message["fault"]["evidence"] == notice.evidence()
}

fn reported(row: &Value, history: &[Value], notice: &Notice) -> bool {
    current_messages(row, history).any(|m| same_fault(m, notice))
}

#[derive(Deserialize, PartialEq, Serialize)]
struct Watch {
    at: f64,
    attempt: String,
    round: u64,
    scope: String,
    #[serde(default)]
    execution: String,
    salt: String,
    fingerprint: String,
    since: f64,
    #[serde(default)]
    paused: bool,
}

fn watch_for<'a>(row: &Value, watch: Option<&'a Watch>) -> Option<&'a Watch> {
    watch.filter(|w| {
        row["attempt"] == w.attempt.as_str()
            && row["round"] == w.round
            && row_scope(row) == Some(w.scope.as_str())
            && execution_binding(row) == w.execution
    })
}

fn watch(path: &Path) -> Result<Option<Watch>> {
    match fs::File::open(path) {
        // Timers can be rebuilt from a fresh capture. Episode deduplication
        // uses the separate durable inbox, even if this file is unreadable JSON.
        Ok(file) => Ok(serde_json::from_reader(file).ok()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize, Serialize)]
enum Sample {
    Screen {
        fingerprint: String,
        salt: String,
        fault: Option<Notice>,
        nonblank: bool,
    },
    Unavailable,
}

fn classify(host: &str, screen: &str) -> Option<Notice> {
    if host == "codex" && crate::tmux::codex_read_only(screen) {
        return Some(Notice::Permission);
    }
    for line in screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(8)
    {
        let line = line.trim();
        // Codex uses this marker for assistant output. A bare footer, timer or
        // redraw underneath an older fault is not a new assistant response.
        let assistant_marker = host == "codex" && line.starts_with("• ");
        let line = line
            .strip_prefix(['•', '■'])
            .unwrap_or(line)
            .trim_start()
            .to_ascii_lowercase();
        let assistant_output = assistant_marker
            && line.chars().next().is_some_and(char::is_alphabetic)
            && ![
                "elapsed:",
                "time elapsed:",
                "this conversation is read-only or unavailable;",
            ]
            .iter()
            .any(|&prefix| line.starts_with(prefix))
            && !line.contains("esc to interrupt");
        let prefix = |choices: &[&str]| {
            choices.iter().any(|choice| {
                line.strip_prefix(choice).is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with([' ', ':', '.', '!', '(', '\t'])
                })
            })
        };
        let notice = if prefix(&[
            "quota exceeded",
            "usage limit reached",
            "rate limit exceeded",
            "rate limit reached",
            "insufficient credits",
            "error: quota exceeded",
        ]) {
            Notice::Quota
        } else if prefix(&[
            "approval required",
            "permission required",
            "waiting for approval",
            "read-only session",
            "trust this workspace",
        ]) {
            Notice::Permission
        } else if prefix(&[
            "connection reset",
            "protocol disconnected",
            "connection lost",
            "broken pipe",
        ]) {
            Notice::Disconnected
        } else if prefix(&["login required", "authentication failed", "unauthorized"]) {
            Notice::Authentication
        } else if prefix(&["policy rejected", "policy denied", "forbidden by policy"]) {
            Notice::Policy
        } else if prefix(&["request timed out", "operation timed out", "host timeout"]) {
            Notice::HostTimeout
        } else if assistant_output || prefix(&["working", "processing", "resumed activity"]) {
            return None;
        } else {
            continue;
        };
        return Some(notice);
    }
    None
}

#[derive(Deserialize, Serialize)]
enum Notice {
    Quota,
    Authentication,
    Policy,
    Permission,
    Disconnected,
    Unavailable,
    HostTimeout,
    Stalled(u64),
    Deadline(u64),
    Exited(Map<String, Value>),
    Recovered,
    ReadRestored,
}

impl Notice {
    fn category(&self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Authentication => "authentication",
            Self::Policy => "policy_rejected",
            Self::Permission => "permission_required",
            Self::Disconnected | Self::Unavailable => "protocol_disconnected",
            Self::HostTimeout | Self::Stalled(_) | Self::Deadline(_) => "timeout",
            Self::Exited(_) => "process_exited",
            Self::Recovered | Self::ReadRestored => "recovered",
        }
    }

    fn evidence(&self) -> &'static str {
        match self {
            Self::Unavailable => "backend_unavailable",
            Self::Stalled(_) => "unchanged_screen",
            Self::Deadline(_) => "turn_deadline",
            Self::Exited(_) => "verified_exit",
            Self::Recovered => "changed_activity",
            Self::ReadRestored => "read_restored",
            _ => "screen_notice",
        }
    }

    fn description(&self) -> (&'static str, &'static str) {
        match self {
            Self::Quota => ("blocked", "Host reported a quota limit"),
            Self::Authentication => ("blocked", "Host reported an authentication problem"),
            Self::Policy => ("blocked", "Host reported a policy rejection"),
            Self::Permission => ("blocked", "Host is waiting for permission"),
            Self::Disconnected | Self::Unavailable => {
                ("unknown", "Host connection or observation was lost")
            }
            Self::HostTimeout => ("unknown", "Host reported a timeout"),
            Self::Stalled(_) => ("stalled", "Host screen has stayed unchanged"),
            Self::Deadline(_) => ("unknown", "Host turn exceeded its time limit"),
            Self::Exited(_) => ("exited", "Verified host process exited without completion"),
            Self::Recovered => ("recovered", "Host activity resumed"),
            Self::ReadRestored => ("recovered", "Host screen reads resumed"),
        }
    }
}

fn append(inbox: &mut Inbox, worker: Identity, row: &Value, notice: Notice) {
    let (kind, diagnostic) = notice.description();
    let mut fault = Map::from_iter([
        ("category".into(), json!(notice.category())),
        ("evidence".into(), json!(notice.evidence())),
        ("diagnostic".into(), json!(diagnostic)),
        ("requiresDecision".into(), json!(kind != "recovered")),
    ]);
    match notice {
        Notice::Stalled(seconds) | Notice::Deadline(seconds) => {
            fault.insert("elapsedSeconds".into(), json!(seconds));
        }
        Notice::Exited(evidence) => {
            fault.extend(evidence);
        }
        _ => {}
    }
    let mut message = Message {
        message_type: Some(kind.into()),
        id: inbox.messages.len() as u64 + 1,
        message_id: format!("m-{}", Uuid::new_v4().simple()),
        worker,
        report: row["report"].as_str().map(Into::into),
        request_key: None,
        body_hash: None,
        delivery: Delivery::unsent(),
        resolution: None,
        created_at: Some(receipt::timestamp()),
        additional_fields: Map::from_iter([
            ("kind".into(), json!(OBSERVATION)),
            ("host".into(), row["host"].clone()),
            ("message".into(), json!(diagnostic)),
            ("fault".into(), Value::Object(fault)),
        ]),
    };
    message.body_hash = Some(message.computed_hash());
    inbox.messages.push(message);
}

pub(in crate::workflow) fn project(row: &mut Value, history: &[Value]) {
    if !row["host"].is_string() || matches!(row["status"].as_str(), Some("completed" | "cancelled"))
    {
        return;
    }
    let current = || current_messages(row, history);
    if row["worker"]["state"] == "exited" {
        let terminal = current().rfind(|m| m["fault"]["category"] == "process_exited");
        row["hostActivity"] = json!({"state":"exited","fault":terminal.map(|m| &m["fault"])});
        return;
    }
    let pending = current().any(|m| m["type"] == "question");
    let last_read = read_event(row, history);
    let unreadable = last_read.is_some_and(|m| m["fault"]["evidence"] == "backend_unavailable");
    let latest = if pending {
        current()
            .rev()
            .take_while(|m| m["type"] != "question")
            .find(|m| {
                m["kind"] == OBSERVATION
                    && matches!(
                        m["fault"]["evidence"].as_str(),
                        Some("screen_notice" | "verified_exit")
                    )
            })
    } else {
        current().rfind(|m| m["kind"] == OBSERVATION && !is_read_event(m))
    };
    if let Some(latest) = latest.filter(|m| !unreadable || m["type"] != "recovered") {
        let state = match latest["fault"]["category"].as_str() {
            Some("recovered") => "active",
            Some("permission_required") => "awaiting_permission",
            Some("protocol_disconnected") => "disconnected",
            Some("timeout") if latest["type"] == "stalled" => "stalled",
            Some("timeout") => "timed_out",
            Some("process_exited") => "exited",
            _ => "blocked",
        };
        row["hostActivity"] = json!({"state":state,"fault":latest["fault"]});
        if pending {
            row["hostActivity"]["awaitingAnswer"] = json!(true);
        }
    } else if pending {
        row["hostActivity"] = json!({"state":"awaiting_answer"});
    } else if unreadable {
        row["hostActivity"] =
            json!({"state":"disconnected","fault":last_read.map(|m| &m["fault"])});
    } else if last_read.is_some() {
        row["hostActivity"] = json!({"state":"readable"});
    }
    if last_read.is_some() {
        row["hostActivity"]["readState"] = json!(if unreadable {
            "unavailable"
        } else {
            "readable"
        });
    }
}
