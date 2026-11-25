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
    if row["publicationDiagnostic"].is_null() {
        if !identified(&row)
            || (row["backend"] == "tmux"
                && !row["executionId"].is_null()
                && row["executionId"] != row["attempt"])
            || (!row["host"].is_string() && !row["worker"].is_object())
        {
            row["publicationDiagnostic"] =
                json!("pending assignment lacks a verifiable execution identity");
        }
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
}
