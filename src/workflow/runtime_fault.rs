//! Read the rollout held open by the worker in the signed bundled execution.
mod rollout;

use super::{assignment, backend, codex_rollout, messages::body_hash, valid_id};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const WINDOW: u64 = 512 * 1024;
const MAX_FDS: usize = 512;
const MAX_ROLLOUTS: usize = 8;

pub(super) fn max_turn() -> Duration {
    duration_setting("WFC_MAX_TURN_SECONDS", 3600)
}

pub(super) fn stall_after() -> Duration {
    duration_setting("WFC_ACTIVITY_AFTER", 300)
}

fn duration_setting(name: &str, default: u64) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .unwrap_or_else(|| Duration::from_secs(default))
}

pub(super) struct Observation {
    pub execution: String,
    pub generation: String,
    ticket: Value,
    binding: crate::bundled::RuntimeBinding,
    started_us: u64,
    sampled_us: u64,
    rollout: Option<Rollout>,
}

struct Rollout {
    file: File,
    stamp: FileStamp,
    fd: PathBuf,
    session: Uuid,
    slice: rollout::Slice,
}

impl Rollout {
    fn current(&self) -> bool {
        self.file
            .metadata()
            .is_ok_and(|meta| FileStamp::from(&meta) == self.stamp)
            && fs::metadata(&self.fd).is_ok_and(|meta| FileStamp::from(&meta) == self.stamp)
    }
}

#[derive(PartialEq, Eq)]
struct FileStamp(u64, u64, u64, i64, i64, i64, i64);

impl From<&Metadata> for FileStamp {
    fn from(meta: &Metadata) -> Self {
        Self(
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.mtime(),
            meta.mtime_nsec(),
        )
    }
}

#[derive(Serialize, Deserialize)]
enum TurnEvent {
    Active,
    Complete,
    Aborted,
    Unclassified,
    Error {
        code: String,
        category: String,
        status: Option<u16>,
    },
}

pub(super) struct Notice {
    pub fingerprint: String,
    pub kind: &'static str,
    pub message_type: &'static str,
    pub message: &'static str,
    pub fault: Value,
    pub evidence: Value,
}

impl Notice {
    fn new(
        fingerprint: String,
        kind: &'static str,
        message_type: &'static str,
        message: &'static str,
        category: &str,
        evidence: Value,
    ) -> Self {
        Self {
            fingerprint,
            kind,
            message_type,
            message,
            fault: json!({"category":category,"kind":kind,"diagnostic":message,
                "requiresDecision":!category.is_empty(),"evidence":evidence}),
            evidence,
        }
    }

    pub fn recovered(evidence: Value) -> Self {
        let fingerprint = format!(
            "{}:{}:{}",
            evidence["session"], evidence["turn"], evidence["offset"]
        );
        Self {
            fingerprint,
            kind: "runtime_recovered",
            message_type: "recovered",
            message: "Codex recorded new turn activity",
            fault: Value::Null,
            evidence,
        }
    }
}

fn turn_evidence(session: Uuid, id: &str, point: rollout::Point) -> Value {
    json!({"source":"codex_rollout","session":session.to_string(),"turn":id,
        "atMs":point.at / 1000,"offset":point.offset})
}

impl Observation {
    pub fn read(state: &Path, row: &Value) -> Option<Self> {
        let ticket = target(state, row)?;
        let binding = crate::bundled::runtime_binding(state, ticket).ok()??;
        let started_us = round_started(row)?;
        let sampled_us = epoch_us(SystemTime::now())?;
        let rollout = if binding.state == crate::bundled::RuntimeState::Online {
            owned_rollout(state, row, ticket, &binding, started_us)
        } else {
            None
        };
        if backend::close_blocks_callback(state, row).unwrap_or(true) || !binding.current() {
            return None;
        }
        Some(Self {
            execution: ticket["attempt"].as_str()?.to_owned(),
            generation: ticket["generation"].as_str()?.to_owned(),
            ticket: ticket.clone(),
            binding,
            started_us,
            sampled_us,
            rollout,
        })
    }

    pub fn current(&self, state: &Path, row: &Value) -> bool {
        let Some(ticket) = target(state, row) else {
            return false;
        };
        *ticket == self.ticket
            && crate::bundled::runtime_binding(state, ticket)
                .is_ok_and(|binding| binding.as_ref() == Some(&self.binding))
            && self.rollout.as_ref().is_none_or(Rollout::current)
    }

    pub fn activity(&self) -> Option<Value> {
        let rollout = self.rollout.as_ref()?;
        let activity = rollout.slice.activity()?;
        Some(turn_evidence(rollout.session, &activity.id, activity.point))
    }

    pub fn save_cursor(&self, state: &Path) -> anyhow::Result<()> {
        if let Some(rollout) = &self.rollout {
            rollout.slice.save(state, &self.binding, &rollout.file)?;
        }
        Ok(())
    }

    pub fn notice(&self, max: Duration, stall: Duration, input_us: Option<u64>) -> Option<Notice> {
        use crate::bundled::RuntimeState;
        match self.binding.state {
            RuntimeState::Exited => {
                return Some(Notice::new(
                    "verified-exit".into(),
                    "process_exit",
                    "exited",
                    "Bundled worker exited without a completion receipt",
                    "process_exited",
                    json!({"source":"bundled_cleanup"}),
                ))
            }
            RuntimeState::Disconnected => {
                return Some(Notice::new(
                    "runtime-disconnected".into(),
                    "backend_observation",
                    "unknown",
                    "Bundled runtime stopped while its worker was still alive",
                    "protocol_disconnected",
                    json!({"source":"bundled_process_identity"}),
                ))
            }
            RuntimeState::Online => {}
        }
        let started_us = self.started_us.max(input_us.unwrap_or_default());
        let bound = self.rollout.as_ref().and_then(|rollout| {
            let turn = rollout.slice.turn()?;
            (turn.started > self.started_us && turn.latest.at >= started_us)
                .then_some((rollout.session, turn))
        });
        if let Some((session, turn)) = bound {
            if let TurnEvent::Error {
                code,
                category,
                status,
            } = &turn.event
            {
                let mut evidence = turn_evidence(session, &turn.id, turn.latest);
                evidence["code"] = json!(code);
                if let Some(status) = status {
                    evidence["httpStatus"] = json!(status);
                }
                return Some(Notice::new(
                    host_identity(category, code, *status),
                    "host_error",
                    "blocked",
                    "Codex recorded a terminal turn error",
                    category,
                    evidence,
                ));
            }
        }
        if let Some(deadline) = self.deadline(max, input_us) {
            return Some(deadline);
        }
        if let Some((session, turn)) = bound {
            let idle = self
                .sampled_us
                .saturating_sub(turn.progress.at.max(started_us));
            if matches!(turn.event, TurnEvent::Active)
                && !stall.is_zero()
                && Duration::from_micros(idle) >= stall
            {
                let mut evidence = turn_evidence(session, &turn.id, turn.latest);
                evidence["lastProgressAtMs"] = json!(turn.progress.at / 1000);
                evidence["progressOffset"] = json!(turn.progress.offset);
                return Some(Notice::new(
                    format!("{}:{}:{}", session, turn.id, turn.progress.offset),
                    "stall_timeout",
                    "stalled",
                    "An open Codex turn has no recent recorded progress",
                    "timeout",
                    evidence,
                ));
            }
        }
        None
    }
    pub fn deadline(&self, max: Duration, input_us: Option<u64>) -> Option<Notice> {
        if max.is_zero() || self.binding.state != crate::bundled::RuntimeState::Online {
            return None;
        }
        let started_us = self.started_us.max(input_us.unwrap_or_default());
        let elapsed = Duration::from_micros(self.sampled_us.checked_sub(started_us)?);
        if elapsed < max {
            return None;
        }
        Some(Notice::new(
            "deadline".into(),
            "turn_timeout",
            "unknown",
            "Worker exceeded the configured round deadline",
            "timeout",
            json!({"source":"round_deadline","startedAtMs":started_us / 1000,
                "timeoutMs":u64::try_from(max.as_millis()).ok()?}),
        ))
    }
}

pub(super) fn host_identity(category: &str, code: &str, status: Option<u16>) -> String {
    if category == "host_error" {
        format!("{code}:{status:?}")
    } else {
        category.to_owned()
    }
}

fn target<'a>(state: &Path, row: &'a Value) -> Option<&'a Value> {
    if row["host"] != "codex"
        || row["legacyArchived"] == true
        || !assignment::identified(row)
        || !row["publicationDiagnostic"].is_null()
        || assignment::input_unresolved(row)
        || assignment::unresolved_lifecycle(row).is_some()
        || assignment::resume_audit(row).is_some_and(|event| event["result"] != "accepted")
        || assignment::unresolved_handoff(row).is_some()
    {
        return None;
    }
    let ticket = backend::bundled_input_ticket(row).ok()?;
    let physical = backend::execution_row(row).ok()?;
    if ["dispatch", "round", "attempt"]
        .iter()
        .any(|&key| row["create"][key] != physical[key])
    {
        return None;
    }
    (!backend::close_blocks_callback(state, row).ok()?).then_some(ticket)
}

fn round_started(row: &Value) -> Option<u64> {
    let round = row["rounds"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|round| round["round"] == row["round"] && round["attempt"] == row["attempt"]);
    if round.is_some_and(|round| {
        matches!(
            round["state"].as_str(),
            Some("completed" | "cancelled" | "failed")
        )
    }) {
        return None;
    }
    if round.is_none() && row["round"] != 1 {
        return None;
    }
    let created = round
        .map_or(&row["created"], |round| &round["created"])
        .as_f64()?;
    let created = Duration::try_from_secs_f64(created).ok()?;
    created.as_micros().try_into().ok()
}

pub(super) fn last_accepted_input(row: &Value, history: &[Value]) -> Option<u64> {
    history
        .iter()
        .filter(|message| {
            message["attempt"] == row["attempt"]
                && message["round"] == row["round"]
                && (message["type"] == "nudge"
                    || (message["type"] == "reply" && message["inputResult"] == "accepted"))
        })
        .filter_map(|message| parse_timestamp_us(message["at"].as_str()?))
        .max()
}

fn parse_timestamp_us(value: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(value)
        .ok()?
        .timestamp_micros()
        .try_into()
        .ok()
}

fn epoch_us(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()?
        .as_micros()
        .try_into()
        .ok()
}

fn owned_rollout(
    state: &Path,
    row: &Value,
    ticket: &Value,
    binding: &crate::bundled::RuntimeBinding,
    started_us: u64,
) -> Option<Rollout> {
    let pid = binding.worker["pid"].as_u64()?;
    let dispatch = row["dispatch"].as_str().filter(|id| valid_id(id))?;
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let home = Path::new(row["codexHome"].as_str()?);
    if !home.is_absolute() {
        return None;
    }
    let sessions = home.canonicalize().ok()?.join("sessions");
    if sessions.canonicalize().ok()? != sessions {
        return None;
    }
    let known = if row["hostSession"].is_null() {
        None
    } else {
        Some(Uuid::parse_str(row["hostSession"].as_str()?).ok()?)
    };
    let cwd = row["cwd"].as_str()?;
    let mut found = None;
    let mut candidates = 0;
    for (index, entry) in fs::read_dir(proc.join("fd")).ok()?.enumerate() {
        if index >= MAX_FDS {
            return None;
        }
        let entry = entry.ok()?;
        let fd = entry.path();
        let Ok(path) = fs::read_link(&fd) else {
            continue;
        };
        if !path.starts_with(&sessions) {
            continue;
        }
        let Some(session) = codex_rollout::id(&path) else {
            continue;
        };
        if known.is_some_and(|id| id != session) || !writable_fd(&proc, &entry.file_name()) {
            continue;
        }
        // Pin the descriptor's inode before opening for reading. O_PATH cannot
        // open a device or block on a pipe, even if the worker reuses the fd.
        let Ok(pinned) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&fd)
        else {
            continue;
        };
        let meta = pinned.metadata().ok()?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::geteuid() }
            || fs::read_link(&fd).ok().as_ref() != Some(&path)
            || fs::symlink_metadata(&path)
                .ok()
                .is_none_or(|named| named.dev() != meta.dev() || named.ino() != meta.ino())
        {
            continue;
        }
        let stamp = FileStamp::from(&meta);
        if let Some((_, prior, _, _)) = &found {
            if *prior == stamp {
                continue;
            }
        }
        candidates += 1;
        if candidates > MAX_ROLLOUTS {
            return None;
        }
        let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd())).ok()?;
        let brief = if known.is_some() {
            None
        } else {
            Some(row["codexBriefHash"].as_str()?)
        };
        if !codex_rollout::opening_matches(&file, WINDOW, session, cwd, brief, false).ok()? {
            continue;
        }
        // Multiple matching writers give no unique binding, even if one has
        // no parseable turn yet.
        if found.is_some() {
            return None;
        }
        found = Some((file, stamp, session, fd));
    }
    let (mut file, stamp, session, fd) = found?;
    let identity = body_hash(
        &json!([
            dispatch,
            row["round"],
            row["attempt"],
            started_us,
            ticket["generation"],
            session.to_string(),
            stamp.0,
            stamp.1
        ])
        .to_string(),
    );
    let slice = rollout::Slice::read(
        state.join(format!("runtime-rollout-{dispatch}.json")),
        identity,
        binding,
        &mut file,
        stamp.2,
        started_us,
    )?;
    let rollout = Rollout {
        file,
        stamp,
        fd,
        session,
        slice,
    };
    rollout.current().then_some(rollout)
}

fn writable_fd(proc: &Path, name: &std::ffi::OsStr) -> bool {
    let Ok(file) = File::open(proc.join("fdinfo").join(name)) else {
        return false;
    };
    let mut info = String::new();
    file.take(1024).read_to_string(&mut info).is_ok()
        && info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
            .is_some_and(|flags| {
                flags & libc::O_ACCMODE as u32 != 0 && flags & libc::O_APPEND as u32 != 0
            })
}

fn classify(info: &Value) -> TurnEvent {
    let (code, status, object) = if let Some(code) = info.as_str() {
        (code, None, false)
    } else if let Some(fields) = info.as_object().filter(|fields| fields.len() == 1) {
        let (code, fields) = fields.iter().next().unwrap();
        let Some(fields) = fields.as_object() else {
            return TurnEvent::Unclassified;
        };
        let status = match fields.get("http_status_code") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64().filter(|code| (100..=599).contains(code)) {
                Some(code) => Some(code as u16),
                None => return TurnEvent::Unclassified,
            },
        };
        (code.as_str(), status, true)
    } else {
        return TurnEvent::Unclassified;
    };
    let (code, category) = match (code, object) {
        ("usage_limit_exceeded", false) => ("usage_limit_exceeded", "quota"),
        ("session_budget_exceeded", false) => ("session_budget_exceeded", "quota"),
        ("rate_limit_exceeded", false) => ("rate_limit_exceeded", "rate_limited"),
        ("unauthorized", false) => ("unauthorized", "authentication"),
        ("sandbox_error", false) => ("sandbox_error", "permission_denied"),
        ("response_stream_disconnected", true) => {
            ("response_stream_disconnected", "protocol_disconnected")
        }
        ("response_stream_connection_failed", true) => {
            ("response_stream_connection_failed", "protocol_disconnected")
        }
        ("http_connection_failed", true) => ("http_connection_failed", "protocol_disconnected"),
        ("response_too_many_failed_attempts", true) => {
            ("response_too_many_failed_attempts", "host_error")
        }
        ("internal_server_error", false) => ("internal_server_error", "host_error"),
        ("server_overloaded", false) => ("server_overloaded", "host_error"),
        ("context_window_exceeded", false) => ("context_window_exceeded", "host_error"),
        ("bad_request", false) => ("bad_request", "host_error"),
        ("invalid_prompt", false) => ("invalid_prompt", "host_error"),
        ("model_not_found", false) => ("model_not_found", "host_error"),
        ("flex_unavailable", false) => ("flex_unavailable", "host_error"),
        ("policy_capacity_unavailable", false) => {
            ("policy_capacity_unavailable", "policy_rejected")
        }
        ("inference_hook_denied", false) => ("inference_hook_denied", "policy_rejected"),
        ("cyber_policy", false) => ("cyber_policy", "policy_rejected"),
        ("bio_policy", false) => ("bio_policy", "policy_rejected"),
        ("misalignment_policy_violation", false) => {
            ("misalignment_policy_violation", "policy_rejected")
        }
        ("too_many_denials", false) => ("too_many_denials", "policy_rejected"),
        ("other", false) => ("other", "host_error"),
        _ => return TurnEvent::Unclassified,
    };
    TurnEvent::Error {
        code: code.to_owned(),
        category: category.to_owned(),
        status,
    }
}
