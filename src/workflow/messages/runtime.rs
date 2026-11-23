//! Observe the existing worker binding while a controller is waiting.
use super::{body_hash, receipt, Assignment, Delivery, Identity, Inbox, Message};
use crate::workflow::{assignment, backend, valid_id, write_json};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    env, fs,
    io::ErrorKind,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const OBSERVATION: &str = "host_runtime";

pub(super) fn seconds(value: &str) -> Result<Duration> {
    Duration::try_from_secs_f64(value.parse()?).context("expected finite nonnegative seconds")
}

pub(super) struct Monitor {
    next: Instant,
    stall: Duration,
    deadline: Duration,
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
        }
    }

    pub(super) fn poll(
        &mut self,
        state: &Path,
        dispatch: Option<&str>,
        scope: Option<&str>,
    ) -> Result<bool> {
        if Instant::now() < self.next
            || env::var_os("WFC_WORKER_CALLBACK").is_some_and(|v| !v.is_empty())
        {
            return Ok(false);
        }
        self.next = Instant::now() + Duration::from_secs(1);
        let dir = state.join("dispatches");
        let history = Inbox::snapshot(state)?;
        let ids = match dispatch {
            Some(id) => [id.to_owned()].into(),
            None => assignment::ids(&dir)?,
        };
        for id in ids {
            let Some(mut row) = candidate(&dir, &id, scope) else {
                continue;
            };
            let Ok(assignment) = Assignment::deserialize(&row) else {
                continue;
            };
            let worker = assignment.worker;
            if blocked(&history, &worker) {
                continue;
            }
            let path = dir.join(format!("{id}.host-watch"));
            let previous = watch(&path)?;
            let baseline = previous.as_ref().filter(|w| {
                w.attempt == worker.attempt && w.round == worker.round && w.scope == worker.scope()
            });
            let sample = sample(state, &mut row, baseline.is_some());
            let Some(sample) = sample else {
                continue;
            };
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
            let Some(mut inbox) = Inbox::try_open(state)? else {
                continue;
            };
            // Do not carry a pane observation across a changed assignment,
            // a completed close, another poller, or a worker callback.
            let Some(current) = candidate(&dir, &id, scope) else {
                continue;
            };
            if !same_execution(&row, &current)
                || blocked(&inbox.messages, &worker)
                || watch(&path)? != previous
                || (current["backend"] == "tmux" && backend::archive(state, &current)?.is_some())
            {
                continue;
            }
            let matching = || inbox.messages.iter().filter(|m| matches_worker(m, &worker));
            let last = matching()
                .rfind(|m| {
                    m.additional_fields
                        .get("kind")
                        .is_some_and(|k| k == OBSERVATION)
                })
                .and_then(|m| m.additional_fields.get("fault"));
            let deadline_reported = matching().any(|m| {
                m.additional_fields
                    .get("fault")
                    .is_some_and(|f| f["evidence"] == "turn_deadline")
            });
            let (next, notice) =
                self.evaluate(&current, baseline, sample, last, deadline_reported, now);
            let sent = if let Some(notice) = notice {
                publish(&mut inbox, worker, &current, notice)?;
                true
            } else {
                false
            };
            if let Some(next) = next {
                write_json(&path, &serde_json::to_value(next)?, None)?;
                fs::File::open(&dir)?.sync_all()?;
            }
            if sent {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn evaluate(
        &self,
        row: &Value,
        previous: Option<&Watch>,
        sample: Sample,
        last: Option<&Value>,
        deadline_reported: bool,
        now: f64,
    ) -> (Option<Watch>, Option<Notice>) {
        let (next, notice) = match sample {
            Sample::Unavailable => (None, Some(Notice::Disconnected("backend_unavailable"))),
            Sample::Exited(evidence) => (None, Some(Notice::Exited(evidence))),
            Sample::Screen(screen) => {
                let start = screen.ceil_char_boundary(screen.len().saturating_sub(4096));
                let tail = screen[start..].trim();
                let reading = classify(tail);
                let salt = previous
                    .map_or_else(|| Uuid::new_v4().simple().to_string(), |w| w.salt.clone());
                let fingerprint = body_hash(&format!("{salt}{tail}"));
                let changed = previous.is_none_or(|w| w.fingerprint != fingerprint);
                let since = previous.filter(|_| !changed).map_or(now, |w| w.since);
                let total = (now - row["created"].as_f64().unwrap_or(now)).max(0.0);
                let idle = (now - since).max(0.0);
                let fault_open = last.is_some_and(|fault| fault["category"] != "recovered");
                let notice = match reading {
                    Reading::Fault(notice) => Some(notice),
                    _ if !deadline_reported
                        && !self.deadline.is_zero()
                        && total >= self.deadline.as_secs_f64() =>
                    {
                        Some(Notice::Deadline(total as u64))
                    }
                    Reading::Active
                        if changed
                            && fault_open
                            && last.is_some_and(|fault| fault["evidence"] != "turn_deadline") =>
                    {
                        Some(Notice::Recovered)
                    }
                    _ if !fault_open
                        && !self.stall.is_zero()
                        && idle >= self.stall.as_secs_f64() =>
                    {
                        Some(Notice::Stalled(idle as u64))
                    }
                    _ => None,
                };
                let next = Watch {
                    at: now,
                    attempt: row["attempt"].as_str().unwrap_or_default().into(),
                    round: row["round"].as_u64().unwrap_or_default(),
                    scope: row["scope"]
                        .as_str()
                        .or_else(|| row["dispatch"].as_str())
                        .unwrap_or_default()
                        .into(),
                    salt,
                    fingerprint,
                    since,
                };
                // Preserve an ordering token across meaningful changes, avoiding
                // a write on each poll of a screen that is still identical.
                (if changed { Some(next) } else { None }, notice)
            }
        };
        (
            next,
            notice.filter(|n| {
                last.is_none_or(|old| {
                    old["category"] != n.category()
                        || (n.category() == "timeout" && old["evidence"] != n.evidence())
                })
            }),
        )
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
            Some("herdr") => {
                row["backendHandle"].is_object()
                    && backend::pending_herdr_close(&row).is_none()
                    && !backend::herdr_closes(&row)
                        .next_back()
                        .is_some_and(|event| event["result"] == "completed")
            }
            _ => false,
        }
    {
        return None;
    }
    Some(row)
}

fn same_execution(a: &Value, b: &Value) -> bool {
    [
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
    ]
    .iter()
    .all(|&k| a[k] == b[k])
}

fn matches_worker(m: &Message, w: &Identity) -> bool {
    m.worker.dispatch == w.dispatch
        && m.worker.round == w.round
        && m.worker.attempt == w.attempt
        && m.worker.scope() == w.scope()
}

fn blocked(messages: &[Message], worker: &Identity) -> bool {
    // This base has no reply command. A question receipt for the current
    // attempt stays outstanding until the reply path is integrated.
    messages.iter().any(|m| {
        matches_worker(m, worker)
            && (matches!(m.kind(), "done" | "question")
                || m.additional_fields
                    .get("kind")
                    .is_some_and(|k| k == "startup_failure"))
    })
}

#[derive(Deserialize, PartialEq, Serialize)]
struct Watch {
    at: f64,
    attempt: String,
    round: u64,
    scope: String,
    salt: String,
    fingerprint: String,
    since: f64,
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

enum Sample {
    Screen(String),
    Unavailable,
    Exited(Map<String, Value>),
}

fn sample(state: &Path, row: &mut Value, observed: bool) -> Option<Sample> {
    let (worker, inspection) = backend::observe(state, row).ok()?;
    if row["closePending"] == true
        || inspection["result"] == "completed"
        || worker["state"] == "cancelled"
        || worker["state"] == "not_started"
    {
        return None;
    }
    if worker["state"] == "exited" {
        // An early exit already in the #13 inbox suppresses this observation.
        return backend::runtime_exit(state, row, &inspection)
            .ok()
            .flatten()
            .map(Sample::Exited);
    }
    if inspection["result"] == "accepted"
        && inspection["alive"] == true
        && inspection["owned"] == true
        && worker["state"] == "running"
    {
        if let Ok(Some(screen)) = backend::runtime_screen(state, row) {
            return Some(Sample::Screen(screen));
        }
    }
    observed.then_some(Sample::Unavailable)
}

enum Reading {
    Fault(Notice),
    Active,
    Other,
}

fn classify(screen: &str) -> Reading {
    for line in screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(8)
    {
        let line = line.trim().to_ascii_lowercase();
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
            Notice::Disconnected("screen_notice")
        } else if prefix(&["login required", "authentication failed", "unauthorized"]) {
            Notice::Authentication
        } else if prefix(&["policy rejected", "policy denied", "forbidden by policy"]) {
            Notice::Policy
        } else if prefix(&["request timed out", "operation timed out", "host timeout"]) {
            Notice::HostTimeout
        } else if prefix(&["working", "processing", "resumed activity"]) {
            return Reading::Active;
        } else {
            continue;
        };
        return Reading::Fault(notice);
    }
    Reading::Other
}

enum Notice {
    Quota,
    Authentication,
    Policy,
    Permission,
    Disconnected(&'static str),
    HostTimeout,
    Stalled(u64),
    Deadline(u64),
    Exited(Map<String, Value>),
    Recovered,
}

impl Notice {
    fn category(&self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Authentication => "authentication",
            Self::Policy => "policy_rejected",
            Self::Permission => "permission_required",
            Self::Disconnected(_) => "protocol_disconnected",
            Self::HostTimeout | Self::Stalled(_) | Self::Deadline(_) => "timeout",
            Self::Exited(_) => "process_exited",
            Self::Recovered => "recovered",
        }
    }

    fn evidence(&self) -> &'static str {
        match self {
            Self::Disconnected(source) => source,
            Self::Stalled(_) => "unchanged_screen",
            Self::Deadline(_) => "turn_deadline",
            Self::Exited(_) => "verified_exit",
            Self::Recovered => "changed_activity",
            _ => "screen_notice",
        }
    }

    fn description(&self) -> (&'static str, &'static str) {
        match self {
            Self::Quota => ("blocked", "Host reported a quota limit"),
            Self::Authentication => ("blocked", "Host reported an authentication problem"),
            Self::Policy => ("blocked", "Host reported a policy rejection"),
            Self::Permission => ("blocked", "Host is waiting for permission"),
            Self::Disconnected(_) => ("unknown", "Host connection or observation was lost"),
            Self::HostTimeout => ("unknown", "Host reported a timeout"),
            Self::Stalled(_) => ("stalled", "Host screen has stayed unchanged"),
            Self::Deadline(_) => ("unknown", "Host turn exceeded its time limit"),
            Self::Exited(_) => ("exited", "Verified host process exited without completion"),
            Self::Recovered => ("recovered", "Host activity resumed"),
        }
    }
}

fn publish(inbox: &mut Inbox, worker: Identity, row: &Value, notice: Notice) -> Result<()> {
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
    inbox.save().context("publish host observation")
}

pub(in crate::workflow) fn project(row: &mut Value, history: &[Value]) {
    if matches!(row["status"].as_str(), Some("completed" | "cancelled")) {
        return;
    }
    let current = || {
        history.iter().filter(|m| {
            m["dispatch"] == row["dispatch"]
                && m["round"] == row["round"]
                && m["attempt"] == row["attempt"]
                && m["scope"].as_str().or_else(|| m["dispatch"].as_str())
                    == row["scope"].as_str().or_else(|| row["dispatch"].as_str())
        })
    };
    if current().any(|m| m["type"] == "question") {
        row["hostActivity"] = json!({"state":"awaiting_answer"});
    } else if let Some(latest) = current().rfind(|m| m["kind"] == OBSERVATION) {
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
    }
}
