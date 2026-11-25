//! Publish a complete launch reservation and resolve either public generation.
use super::{decode_record, valid_id, valid_identity};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

pub(super) fn reserve(dir: &Path, id: &str, row: &Value) -> Result<PathBuf> {
    let staging = dir.join(format!("{id}.reserving"));
    let pending = dir.join(format!("{id}.pending"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // This per-ID private name excludes other publishers before the rename.
    // No callback, status query or relay enumerates a partially written JSON.
    let file = options.open(&staging).context("reserve dispatch record")?;
    let result = (|| -> Result<()> {
        ensure!(
            !pending.try_exists()? && !dir.join(format!("{id}.json")).try_exists()?,
            "dispatch already reserved"
        );
        serde_json::to_writer(&file, row)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staging, &pending)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result?;
    Ok(pending)
}

// Capture the pending handle first. A concurrent promotion either leaves that
// handle readable or supplies the published generation on the second open.
pub(super) fn open_assignment(dir: &Path, id: &str) -> std::io::Result<(File, PathBuf)> {
    let reservation = dir.join(format!("{id}.pending"));
    let pending = File::open(&reservation);
    let published = dir.join(format!("{id}.json"));
    match File::open(&published) {
        Ok(file) => Ok((file, published)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok((pending?, reservation)),
        Err(e) => Err(e),
    }
}

pub(super) fn identified(row: &Value) -> bool {
    row["attempt"]
        .as_str()
        .is_some_and(|s| valid_identity(s, "a-"))
        && row["round"].as_u64().is_some_and(|n| n > 0)
}

pub(super) const TMUX_HISTORY_ONLY: &str = "tmux assignment history is read-only";
pub(super) const UNVERIFIED_ASSIGNMENT: &str = "cannot verify assignment publication identity";

pub(super) fn same_publication_identity(before: &Value, after: &Value) -> bool {
    [
        "dispatch",
        "role",
        "round",
        "attempt",
        "scope",
        "controllerOwner",
        "backend",
        "executionId",
    ]
    .iter()
    .all(|&field| before[field] == after[field])
}

pub(super) fn tmux_history(row: &Value) -> bool {
    row["backend"] == "tmux"
        || ((row["backend"].is_null() || row["backend"] == "")
            && row["legacyArchived"] != true
            && (row["host"].is_string()
                || row["tmuxSocket"].is_string()
                || row["tmux"].as_str().is_some_and(|name| {
                    row["dispatch"]
                        .as_str()
                        .is_some_and(|id| name.strip_prefix("wfc-") == Some(id))
                })))
}

pub(super) fn require_not_tmux_history(row: &Value) -> Result<()> {
    ensure!(!tmux_history(row), "{TMUX_HISTORY_ONLY}");
    Ok(())
}

#[derive(Default)]
pub(super) enum MessageBoundary {
    #[default]
    Open,
    Retired,
    Unverified,
    RetiredAttempts(Vec<(u64, String)>),
}

impl MessageBoundary {
    pub(super) fn for_record(row: &Value, published: bool) -> Self {
        if tmux_history(row) {
            return Self::Retired;
        }
        if !row["publicationDiagnostic"].is_null() {
            return Self::Unverified;
        }
        if !published {
            return Self::Open;
        }
        let retired = retired_attempts(row);
        if retired.is_empty() {
            Self::Open
        } else {
            Self::RetiredAttempts(retired)
        }
    }

    pub(super) fn refusal(
        &self,
        round: Option<u64>,
        attempt: Option<&str>,
    ) -> Option<&'static str> {
        match self {
            Self::Retired => Some(TMUX_HISTORY_ONLY),
            Self::Unverified => Some(UNVERIFIED_ASSIGNMENT),
            Self::RetiredAttempts(retired)
                if retired
                    .iter()
                    .any(|(r, a)| round == Some(*r) && attempt == Some(a.as_str())) =>
            {
                Some(TMUX_HISTORY_ONLY)
            }
            _ => None,
        }
    }
}

fn receipt_attempt<'a>(round: &Value, attempt: &'a Value) -> Option<(u64, &'a str)> {
    let round = round.as_u64().filter(|round| *round > 0)?;
    let attempt = attempt.as_str().filter(|id| valid_identity(id, "a-"))?;
    Some((round, attempt))
}

pub(super) fn verified_handoff_close(row: &Value, closed: &Value) -> bool {
    closed["result"] == "completed"
        && closed["resourceCleanup"] == "confirmed"
        && ["dispatch", "round", "attempt"]
            .iter()
            .all(|key| closed[*key] == row[*key])
        && row["attempt"]
            .as_str()
            .is_some_and(|attempt| closed["closeOperationId"] == format!("close:{attempt}"))
}

fn published_handoff_from_tmux(
    row: &Value,
    event: &Value,
    source: (u64, &str),
    target: (u64, &str),
    target_backend: &str,
) -> bool {
    if !matches!(
        (event["phase"].as_str(), event["result"].as_str()),
        (Some("starting"), Some("pending" | "uncertain")) | (Some("complete"), Some("accepted"))
    ) || event["oldWriterStopped"] != true
    {
        return false;
    }
    let is_current_target = receipt_attempt(&row["round"], &row["attempt"]) == Some(target);
    let published_target = if is_current_target {
        row["backend"] == target_backend
    } else {
        row["rounds"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|round| receipt_attempt(&round["round"], &round["attempt"]) == Some(target))
    };
    if !published_target {
        return false;
    }
    let prior = json!({"dispatch":row["dispatch"], "round":source.0,"attempt":source.1,
        "executionId":source.1,"create":event["priorCreate"]});
    let stop = &event["writerStopEvidence"];
    verified_handoff_close(&prior, &event["close"])
        && event["priorCreate"]["result"] == "accepted"
        && stop["writerTreeStopped"] == true
        && stop["dispatch"] == row["dispatch"]
        && stop["attempt"] == source.1
        && stop["round"] == source.0.to_string()
        && super::backend::receipt_id(&prior)
            .is_ok_and(|launch| launch.is_some_and(|launch| stop["launchId"] == launch))
}

fn retired_attempts(row: &Value) -> Vec<(u64, String)> {
    let current = receipt_attempt(&row["round"], &row["attempt"]);
    let mut retired = BTreeSet::new();
    for event in row["audit"].as_array().into_iter().flatten().rev() {
        let op = event["operation"].as_str();
        // Older continue events carry their dispatch through the enclosing row.
        if event["dispatch"] != row["dispatch"]
            && !(op == Some("continue") && event["dispatch"].is_null())
        {
            continue;
        }
        let Some(target) = receipt_attempt(&event["round"], &event["attempt"]) else {
            continue;
        };
        if op != Some("continue") && event["toAttempt"] != event["attempt"] {
            continue;
        }
        let from_round = if op == Some("resume") && event["fromRound"].is_null() {
            // A resume can open the next round, so the target round is not evidence.
            let mut rounds = row["rounds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|round| round["attempt"] == event["fromAttempt"]);
            match (rounds.next(), rounds.next()) {
                (Some(round), None) => &round["round"],
                _ => continue,
            }
        } else {
            &event["fromRound"]
        };
        let Some(source) = receipt_attempt(from_round, &event["fromAttempt"]) else {
            continue;
        };
        if source == target || current == Some(source) {
            continue;
        }
        let retired_source = match op {
            Some("handoff")
                if event["fromBackend"] == "tmux" && event["target"]["backend"] == "herdr" =>
            {
                published_handoff_from_tmux(row, event, source, target, "herdr")
            }
            Some("handoff")
                if event["fromBackend"].is_null()
                    && event["target"].is_null()
                    && retired.contains(&target) =>
            {
                published_handoff_from_tmux(row, event, source, target, "tmux")
            }
            Some("continue" | "resume")
                if event["fromBackend"].is_null() && retired.contains(&target) =>
            {
                event["result"] == "accepted"
                    && !matches!(
                        event["launchResult"].as_str(),
                        Some("refused" | "definitely-not-sent")
                    )
            }
            _ => false,
        };
        if retired_source {
            retired.insert(source);
        }
    }
    retired
        .into_iter()
        .map(|(round, attempt)| (round, attempt.to_owned()))
        .collect()
}

pub(super) fn has_tmux_history(dir: &Path, id: &str) -> Result<bool> {
    if !valid_id(id) {
        // Older orphan receipts and private backend fixtures can have IDs
        // outside the public assignment namespace.
        return Ok(false);
    }
    Ok(public_record(dir, id)?.as_ref().is_some_and(tmux_history))
}

pub(super) fn resume_audit(row: &Value) -> Option<&Value> {
    row["audit"].as_array()?.iter().rev().find(|event| {
        event["operation"] == "resume"
            && event["attempt"] == row["attempt"]
            && event["round"] == row["round"]
    })
}

pub(super) fn unresolved_resume(row: &Value) -> Option<&Value> {
    resume_audit(row)
        .filter(|event| matches!(event["result"].as_str(), Some("pending" | "uncertain")))
}

pub(super) fn handoff_preflight(event: &Value) -> bool {
    // A diagnostic from the read-only probe precedes the durable handoff intent.
    event["operation"] == "handoff"
        && event["phase"] == "preflight"
        && event["toAttempt"].is_null()
        && event["close"].is_null()
        && event["launch"].is_null()
}

pub(super) fn unresolved_handoff(row: &Value) -> Option<&Value> {
    row["audit"].as_array()?.iter().rev().find(|event| {
        event["operation"] == "handoff"
            && event["dispatch"] == row["dispatch"]
            && !handoff_preflight(event)
            && (event["fromAttempt"] == row["attempt"] || event["toAttempt"] == row["attempt"])
            && matches!(event["result"].as_str(), Some("pending" | "uncertain"))
    })
}

pub(super) fn handoff_blocks_old_writer(row: &Value) -> bool {
    unresolved_handoff(row).is_some_and(|event| event["fromAttempt"] == row["attempt"])
}

pub(super) fn valid_reservation(row: &Value) -> bool {
    identified(row)
        && row["publicationDiagnostic"].is_null()
        && (row["backend"] != "tmux"
            || row["executionId"].is_null()
            || row["executionId"] == row["attempt"])
        && (row["host"].is_string() || row["worker"].is_object())
}

#[cfg(target_os = "linux")]
pub(super) fn unresolved_lifecycle(row: &Value) -> Option<&Value> {
    row["audit"].as_array()?.iter().rev().find(|event| {
        matches!(event["result"].as_str(), Some("pending" | "uncertain"))
            && !handoff_preflight(event)
    })
}

pub(super) fn public_record(dir: &Path, id: &str) -> Result<Option<Value>> {
    let (file, path) = match open_assignment(dir, id) {
        Ok(pair) => pair,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if path.extension().and_then(|s| s.to_str()) != Some("pending") {
        return Ok(Some(decode_record(file, &path)?));
    }
    let mut row = match decode_record(file, &path) {
        Ok(row) => row,
        Err(e) => {
            json!({"dispatch":id,"publicationDiagnostic":format!("cannot verify pending assignment: {e}")})
        }
    };
    row["createPending"] = json!(true);
    if row["publicationDiagnostic"].is_null() && !valid_reservation(&row) {
        row["publicationDiagnostic"] =
            json!("pending assignment lacks a verifiable execution identity");
    }
    Ok(Some(row))
}

pub(super) fn ids(dir: &Path) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ids),
        Err(e) => return Err(e).context("list dispatch records"),
    };
    for entry in entries {
        let path = entry?.path();
        if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("json" | "pending")
        ) {
            if let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| valid_id(s))
            {
                ids.insert(id.to_owned());
            }
        }
    }
    Ok(ids)
}

// The pending and published files can have different inodes. Close and new
// questions must still serialize on the same attempt across that transition.
pub(super) fn close_lock(state: &Path, row: &Value) -> Result<File> {
    let file = attempt_lock_file(state, row)?;
    file.lock()?;
    super::ensure_state_writable(state)?;
    Ok(file)
}

#[cfg(target_os = "linux")]
pub(super) fn try_reply_lock(state: &Path, row: &Value) -> Result<Option<File>> {
    let file = attempt_lock_file(state, row)?;
    if !try_input_lock(&file)? {
        return Ok(None);
    }
    super::ensure_state_writable(state)?;
    Ok(Some(file))
}

#[cfg(target_os = "linux")]
pub(super) fn try_input_lock(file: &File) -> Result<bool> {
    use std::fs::TryLockError;

    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error).context("lock worker input"),
    }
}

fn attempt_lock_file(state: &Path, row: &Value) -> Result<File> {
    super::ensure_state_writable(state)?;
    let row = super::backend::execution_row(row)?;
    let dir = state.join("close-locks");
    let id = row["dispatch"]
        .as_str()
        .filter(|s| valid_id(s))
        .context("invalid dispatch")?;
    let attempt = row["attempt"]
        .as_str()
        .filter(|s| valid_identity(s, "a-"))
        .context("invalid attempt")?;
    fs::create_dir_all(&dir)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    Ok(options.open(dir.join(format!("{id}.{attempt}")))?)
}

pub(super) fn input_unresolved(row: &Value) -> bool {
    row["audit"].as_array().into_iter().flatten().any(|event| {
        matches!(event["operation"].as_str(), Some("continue" | "nudge"))
            && matches!(event["result"].as_str(), Some("pending" | "uncertain"))
            && event["round"] == row["round"]
            && event["attempt"] == row["attempt"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_identity_allows_new_evidence_but_fences_identity_changes() {
        let pending = json!({"dispatch":"d-00000031","role":"worker","round":1,
            "attempt":"a-000000000031","scope":"ours","backend":"herdr",
            "executionId":"a-000000000031","worker":{"state":"running"},"createPending":true});
        let mut published = pending.clone();
        published.as_object_mut().unwrap().remove("createPending");
        published["create"] = json!({"result":"accepted"});
        published["backendHandle"] = json!({"ticket":"fixture"});
        published["worker"]["state"] = json!("exited");
        assert!(same_publication_identity(&pending, &published));
        for (field, value) in [
            ("dispatch", json!("d-00000032")),
            ("role", json!("reviewer")),
            ("round", json!(2)),
            ("attempt", json!("a-000000000032")),
            ("scope", json!("neighbor")),
            ("controllerOwner", json!({"generation":2})),
            ("backend", json!("bundled")),
            ("executionId", json!("a-000000000032")),
        ] {
            let mut changed = published.clone();
            changed[field] = value;
            assert!(!same_publication_identity(&pending, &changed), "{field}");
        }
    }

    #[test]
    fn retired_tmux_classification_keeps_explicit_backends() {
        let ordinary = json!({"dispatch":"d-00000031","host":"codex","tmux":"wfc-d-00000031"});
        assert!(tmux_history(&ordinary));
        assert!(tmux_history(
            &json!({"dispatch":"d-00000031","tmux":"wfc-d-00000031"})
        ));
        assert!(tmux_history(&json!({"backend":"tmux"})));
        assert!(!tmux_history(
            &json!({"dispatch":"d-00000031","worker":{"state":"running"}})
        ));
        for backend in ["herdr", "bundled"] {
            let mut hosted = ordinary.clone();
            hosted["backend"] = json!(backend);
            assert!(require_not_tmux_history(&hosted).is_ok());
        }
    }

    fn tmux_handoff_history() -> Value {
        let dispatch = "d-00000031";
        let first = "a-000000000030";
        let previous = "a-000000000031";
        let current = "a-000000000032";
        let older_herdr = "a-000000000029";
        let launch = "1".repeat(32);
        json!({"dispatch":dispatch,"round":2,"attempt":current,"backend":"herdr",
        "rounds":[{"round":1,"attempt":first},{"round":1,"attempt":previous},
                  {"round":2,"attempt":current}],
        "audit":[
            {"operation":"handoff","dispatch":dispatch,"fromBackend":"herdr",
             "fromRound":1,"fromAttempt":older_herdr,"round":1,"attempt":first,"toAttempt":first},
            {"operation":"resume","dispatch":dispatch,"result":"accepted",
             "fromAttempt":first,"round":1,"attempt":previous,"toAttempt":previous},
            {"operation":"handoff","dispatch":dispatch,"fromBackend":"tmux",
             "fromRound":1,"fromAttempt":previous,"round":2,"attempt":current,"toAttempt":current,
             "target":{"backend":"herdr"},"phase":"complete","result":"accepted","oldWriterStopped":true,
             "priorCreate":{"dispatch":dispatch,"round":1,"attempt":previous,"result":"accepted",
                            "receipt":format!("/fixture/{launch}.json")},
             "close":{"dispatch":dispatch,"round":1,"attempt":previous,"result":"completed",
                      "resourceCleanup":"confirmed","closeOperationId":format!("close:{previous}")},
             "writerStopEvidence":{"dispatch":dispatch,"round":"1","attempt":previous,
                                   "launchId":launch,"writerTreeStopped":true}}
        ]})
    }

    #[test]
    fn handoff_receipts_are_classified_by_original_round_and_attempt() {
        let first = "a-000000000030";
        let previous = "a-000000000031";
        let current = "a-000000000032";
        let mut row = tmux_handoff_history();
        let boundary = MessageBoundary::for_record(&row, true);
        assert_eq!(
            boundary.refusal(Some(1), Some(previous)),
            Some(TMUX_HISTORY_ONLY)
        );
        assert_eq!(boundary.refusal(Some(2), Some(previous)), None);
        assert_eq!(
            boundary.refusal(Some(1), Some(first)),
            Some(TMUX_HISTORY_ONLY)
        );
        assert_eq!(boundary.refusal(Some(1), Some("a-000000000029")), None);
        assert_eq!(boundary.refusal(Some(2), Some(current)), None);

        let bundled = "a-000000000033";
        row["backend"] = json!("bundled");
        row["round"] = json!(3);
        row["attempt"] = json!(bundled);
        let boundary = MessageBoundary::for_record(&row, true);
        assert_eq!(
            boundary.refusal(Some(1), Some(first)),
            Some(TMUX_HISTORY_ONLY)
        );
        assert_eq!(boundary.refusal(Some(3), Some(bundled)), None);
        row["publicationDiagnostic"] = json!("identity unavailable");
        assert_eq!(
            MessageBoundary::for_record(&row, true).refusal(Some(3), Some(bundled)),
            Some(UNVERIFIED_ASSIGNMENT)
        );
    }

    #[test]
    fn handoff_archive_requires_a_published_target_and_proven_stop() {
        let attempt = "a-000000000031";
        let earlier = "a-000000000030";
        let mut row = tmux_handoff_history();
        let refused = |record: &Value| {
            MessageBoundary::for_record(record, true).refusal(Some(1), Some(attempt))
        };

        row["audit"][2]["phase"] = json!("starting");
        for result in ["pending", "uncertain"] {
            row["audit"][2]["result"] = json!(result);
            assert_eq!(refused(&row), Some(TMUX_HISTORY_ONLY));
        }
        assert_eq!(
            MessageBoundary::for_record(&row, false).refusal(Some(1), Some(attempt)),
            None
        );
        for (field, value) in [
            ("dispatch", json!("d-00000032")),
            ("phase", json!("closing")),
            ("result", json!("refused")),
            ("oldWriterStopped", json!(false)),
            ("close", Value::Null),
            ("writerStopEvidence", Value::Null),
        ] {
            let mut invalid = row.clone();
            invalid["audit"][2][field] = value;
            assert_eq!(refused(&invalid), None, "{field}");
        }
        let mut invalid = row.clone();
        invalid["audit"][2]["writerStopEvidence"]["launchId"] = json!("0".repeat(32));
        assert_eq!(refused(&invalid), None);
        invalid = row.clone();
        invalid["audit"][2]["close"]["closeOperationId"] = json!("close:other");
        assert_eq!(refused(&invalid), None);
        invalid = row.clone();
        invalid["audit"][2]["attempt"] = json!("a-000000000039");
        invalid["audit"][2]["toAttempt"] = json!("a-000000000039");
        assert_eq!(refused(&invalid), None);

        for result in ["refused", "definitely-not-sent", "pending"] {
            row["audit"][1]["result"] = json!(result);
            let boundary = MessageBoundary::for_record(&row, true);
            assert_eq!(
                boundary.refusal(Some(1), Some(attempt)),
                Some(TMUX_HISTORY_ONLY)
            );
            assert_eq!(boundary.refusal(Some(1), Some(earlier)), None);
        }
    }
}
