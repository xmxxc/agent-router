//! Public lifecycle clients for hosted workers.
#[cfg(target_os = "linux")]
mod bundled;
#[cfg(target_os = "linux")]
pub(super) use bundled::{
    observe_for_resume as observe_bundled_for_resume, registered_ticket as bundled_input_ticket,
    resume as resume_bundled, submit_continue as submit_bundled_continue,
    submit_nudge as submit_bundled_nudge, submit_reply as submit_bundled_reply,
    APPENDED_USER_ENTRY as BUNDLED_APPENDED_USER_ENTRY,
};

use super::{host::Launch, valid_identity};
#[cfg(all(test, target_os = "linux"))]
use super::write_json;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
#[cfg(not(target_os = "linux"))]
use std::fs::File;
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

// Public assignments published with the session-aware tmux protocol.
pub(super) const LAUNCH_PROTOCOL: u64 = 8;

// Continue changes the public round without changing the writer or its
// launch receipt. All native lifecycle operations still name that execution.
pub(super) fn execution_row(row: &Value) -> Result<Cow<'_, Value>> {
    let execution = &row["executionId"];
    if !matches!(row["backend"].as_str(), Some("tmux" | "bundled"))
        || execution.is_null()
        || execution == &row["attempt"]
    {
        return Ok(Cow::Borrowed(row));
    }
    let create = &row["create"];
    ensure!(
        execution
            .as_str()
            .is_some_and(|id| valid_identity(id, "a-"))
            && create["attempt"] == *execution
            && create["result"] == "accepted"
            && create["dispatch"] == row["dispatch"]
            && (row["backend"] != "bundled"
                || (create["ticket"].is_object()
                    && create["ticket"]["unit"] == row["dispatch"]
                    && create["ticket"]["attempt"] == *execution
                    && row["backendHandle"] == create["ticket"]))
            && create["round"]
                .as_u64()
                .zip(row["round"].as_u64())
                .is_some_and(|(old, new)| old < new)
            && row["rounds"]
                .as_array()
                .is_some_and(|rounds| rounds.iter().any(|round| {
                    round["attempt"] == *execution
                        && round["round"] == create["round"]
                        && round["state"] == "completed"
                })),
        "execution identity does not match this attempt or its original launch"
    );
    let mut physical = row.clone();
    physical["attempt"] = execution.clone();
    physical["round"] = create["round"].clone();
    Ok(Cow::Owned(physical))
}

// The old branch shapes remain available to the caller until host handoff,
// resume and input can use a supported backend. No transport is installed here.
#[cfg(target_os = "linux")]
pub(super) fn tmux_socket() -> Result<PathBuf> {
    bail!(super::assignment::TMUX_HISTORY_ONLY)
}

pub fn create(state: &Path, row: &Value, launch: Launch, _owner: Option<&str>) -> Result<Value> {
    #[cfg(target_os = "linux")]
    {
        ensure!(row["backend"] == "bundled", "bundled assignment required");
        bundled::create(state, row, launch, _owner)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, row, launch);
        bail!("bundled dispatch requires Linux")
    }
}

#[cfg(target_os = "linux")]
pub(super) fn create_for_handoff(
    _state: &Path,
    _row: &Value,
    _audit: usize,
    _launch: Launch,
) -> Result<Value> {
    bail!(super::assignment::TMUX_HISTORY_ONLY)
}

#[cfg(target_os = "linux")]
pub(super) fn handoff_target(_state: &Path, _row: &Value) -> Result<Value> {
    bail!(super::assignment::TMUX_HISTORY_ONLY)
}

#[cfg(target_os = "linux")]
pub(super) fn resume(_state: &Path, _row: &Value, _launch: Launch) -> Result<Value> {
    bail!(super::assignment::TMUX_HISTORY_ONLY)
}

#[cfg(target_os = "linux")]
pub(super) fn observe_for_resume(_state: &Path, _row: &Value) -> Result<(Value, Value)> {
    bail!(super::assignment::TMUX_HISTORY_ONLY)
}

fn close_operation(row: &Value) -> Result<String> {
    let attempt = row["attempt"]
        .as_str()
        .filter(|v| valid_identity(v, "a-"))
        .context("invalid close attempt")?;
    Ok(format!("close:{attempt}"))
}

fn legacy_selector(row: &Value) -> Result<&str> {
    if let Some(id) = receipt_id(row)? {
        return Ok(id);
    }
    ensure!(
        row["create"].is_null(),
        "missing confirmation of the original native launch"
    );
    let attempt = row["attempt"]
        .as_str()
        .filter(|s| valid_identity(s, "a-"))
        .context("missing legacy attempt")?;
    // An external ID in an old row can identify a launch, but cannot prove
    // which public attempt owns it. Register the attempt itself with #31.
    if let Some(external) = row["legacyExecutionId"].as_str() {
        ensure!(
            !valid_identity(external, "a-") || external == attempt,
            "legacy execution does not match this attempt"
        );
    }
    Ok(attempt)
}

fn legacy_launch_hint(row: &Value) -> Result<Option<&str>> {
    let id = row["legacyExecutionId"]
        .as_str()
        .filter(|id| valid_launch(id));
    let hint = row["legacyLaunchId"].as_str();
    ensure!(
        hint.is_none_or(valid_launch) && id.zip(hint).is_none_or(|(a, b)| a == b),
        "conflicting legacy launch hints"
    );
    Ok(hint.or(id))
}

pub(super) fn receipt_id(row: &Value) -> Result<Option<&str>> {
    let create = &row["create"];
    if !create.is_null() {
        // R2 only created first-round assignments and stored the reply inline.
        let r2 = row["executionId"].is_null()
            && row["round"] == 1
            && ["dispatch", "attempt", "round"]
                .iter()
                .all(|k| create.get(k).is_none());
        ensure!(
            r2 || ["dispatch", "attempt", "round"]
                .iter()
                .all(|k| create[*k] == row[*k]),
            "launch evidence does not match this attempt"
        );
    }
    let Some(name) = create["receipt"].as_str() else {
        return Ok(None);
    };
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("invalid launch receipt")?;
    ensure!(
        stem.len() == 32
            && stem.bytes().all(|b| b.is_ascii_hexdigit())
            && path.extension() == Some("json".as_ref()),
        "invalid launch receipt"
    );
    Ok(Some(stem))
}

fn valid_launch(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn private_receipt(state: &Path, launch: Option<&str>) -> Result<Value> {
    let Some(stem) = launch else {
        return Ok(Value::Null);
    };
    Ok(read_evidence(&state.join("launches").join(format!("{stem}.json")))?.unwrap_or(Value::Null))
}

#[cfg(target_os = "linux")]
pub(super) fn publish_unconfirmed_tmux_exit(
    state: &Path,
    row: &Value,
    ledger: Option<super::messages::PublishLock>,
) -> Result<()> {
    if super::assignment::tmux_history(row) {
        return Ok(());
    }
    if let Some(receipt) = verified_unconfirmed_tmux_receipt(state, row)? {
        super::startup::unconfirmed_tmux_exit(
            state,
            row,
            receipt["launchId"].as_str().context("missing launch ID")?,
            &receipt,
            ledger,
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn verified_unconfirmed_tmux_receipt(
    state: &Path,
    row: &Value,
) -> Result<Option<Value>> {
    let create = &row["create"];
    if create["result"] != "uncertain" || create["category"] != "process_exited" {
        return Ok(None);
    }
    ensure!(
        row["backend"] == "tmux" && row["executionId"] == row["attempt"],
        "early-exit execution identity mismatch"
    );
    let id = receipt_id(row)?.context("missing early-exit launch receipt")?;
    let path = state
        .canonicalize()?
        .join("launches")
        .join(format!("{id}.json"));
    ensure!(
        create["receipt"].as_str().map(Path::new) == Some(path.as_path()),
        "unexpected early-exit receipt path"
    );
    let receipt =
        super::legacy_tmux::read_receipt(&path).context("missing verified early-exit receipt")?;
    ensure!(
        receipt["launchId"] == id
            && receipt["category"] == "process_exited"
            && receipt["exitCode"] == create["exitCode"]
            && receipt["exitCode"].as_u64().is_some_and(|code| code <= 255),
        "early-exit launch identity or outcome mismatch"
    );
    Ok(Some(receipt))
}

fn script_preflight_failed(row: &Value) -> bool {
    let create = &row["create"];
    create["result"] == "definitely-not-sent"
        && matches!(
            create["category"].as_str(),
            Some("configured_script_missing" | "configured_script_permission_denied")
        )
        && matches!(row["backend"].as_str(), Some("tmux" | "herdr" | "bundled"))
        && row["dispatch"].as_str().is_some_and(super::valid_id)
        && super::assignment::identified(row)
        && row["scope"].as_str().is_some_and(|scope| !scope.is_empty())
        && same_execution(row, create)
        // Before scope was repeated in this marker, the bound assignment
        // was the authoritative scope used for publication and replay.
        && create.get("scope").is_none_or(|scope| scope == &row["scope"])
        && (row["backend"] != "tmux" || row["executionId"] == row["attempt"])
        && row["createPending"] != true
        && create["receipt"].is_null()
        && row["backendHandle"].is_null()
        && row["worker"] == json!({"state":"not_started","exit_code":null,"signal":null})
}

fn script_preflight_close(row: &Value) -> Option<Value> {
    script_preflight_failed(row).then(|| {
        json!({
            "dispatch":row["dispatch"],"operation":"close","result":"completed",
            "round":row["round"],"attempt":row["attempt"],"worker":row["worker"],
            "diagnostic":"session was already gone",
        })
    })
}

#[cfg(target_os = "linux")]
pub(super) fn handoff_writer(state: &Path, row: &Value) -> Result<Option<wfc_herdr::Process>> {
    let launch = receipt_id(row)?.context("missing original launch receipt")?;
    let known = state
        .canonicalize()?
        .join("launches")
        .join(format!("{launch}.json"));
    ensure!(
        row["create"]["receipt"].as_str().map(Path::new) == Some(known.as_path()),
        "unexpected original launch receipt path"
    );
    let receipt = private_receipt(state, Some(launch))?;
    ensure!(
        receipt["launchId"] == launch && receipt["treeTracked"] == true,
        "unverified original worker tree; handoff requires a tracked native launch"
    );
    if exited(&receipt).is_some() {
        return Ok(None);
    }
    let pid = receipt["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0 && receipt["category"] == "started")
        .context("missing original writer process")?;
    Ok(wfc_herdr::Process::observe(pid)?)
}

#[cfg(target_os = "linux")]
pub(super) fn handoff_tree_stopped(state: &Path, row: &Value) -> Result<Option<Value>> {
    let launch = receipt_id(row)?.context("missing original launch receipt")?;
    let path = state.join("launches").join(format!("{launch}.stopped"));
    let Some(evidence) = super::handoff::read_order(&path)? else {
        return Ok(None);
    };
    Ok((evidence["writerTreeStopped"] == true
        && evidence["launchId"] == launch
        && evidence["dispatch"] == row["dispatch"]
        && evidence["attempt"] == row["attempt"]
        && row["round"]
            .as_u64()
            .is_some_and(|round| evidence["round"] == round.to_string()))
    .then_some(evidence))
}

fn closed_path(state: &Path, row: &Value) -> Result<PathBuf> {
    let attempt = row["attempt"]
        .as_str()
        .filter(|v| valid_identity(v, "a-"))
        .context("invalid attempt")?;
    Ok(state.join("dispatches").join(format!(
        "{}.{attempt}.closed",
        row["dispatch"].as_str().context("invalid dispatch")?
    )))
}

fn read_evidence(path: &Path) -> Result<Option<Value>> {
    #[cfg(target_os = "linux")]
    return super::handoff::read_order(path);
    #[cfg(not(target_os = "linux"))]
    match File::open(path) {
        Ok(f) => Ok(Some(serde_json::from_reader(f)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn archive(state: &Path, row: &Value) -> Result<Option<Value>> {
    let row = execution_row(row)?;
    let Some(saved) = read_evidence(&closed_path(state, &row)?)? else {
        return Ok(None);
    };
    ensure!(
        same_execution(&row, &saved),
        "closed record identity mismatch"
    );
    Ok(Some(saved))
}

fn archived_exit(saved: &Value) -> bool {
    let worker = &saved["worker"];
    worker["state"] == "exited" && worker["exit_code"].as_u64().is_some_and(|code| code <= 255)
}

// A missing session does not establish whether a worker ever ran. The
// backend supplies the fact only after winning the one-shot launch reservation.
fn matches_revocation(row: &Value, fact: &Value) -> bool {
    row["executionId"].is_string()
        && row["executionId"] == row["attempt"]
        && fact["dispatch"] == row["dispatch"]
        && fact["executionId"] == row["executionId"]
        && close_operation(row).is_ok_and(|op| fact["closeOperationId"] == op)
        && fact["launchId"].as_str().is_some_and(valid_launch)
        && fact["category"] == "not_started"
}

fn saved_revocation<'a>(row: &Value, saved: &'a Value) -> Option<&'a Value> {
    let fact = &saved["revocation"];
    (saved["worker"] == json!({"state":"not_started"})
        && matches_revocation(row, fact)
        && saved["closeOperationId"] == fact["closeOperationId"]
        && saved["launchId"] == fact["launchId"]
        && saved["executionId"] == row["attempt"]
        && receipt_id(row).is_ok_and(|known| known.is_none_or(|id| fact["launchId"] == id)))
    .then_some(fact)
}

fn revocation_close_effect(receipt: &Value) -> bool {
    matches!(
        receipt["closeEffect"].as_str(),
        Some("already_absent" | "unstarted_session_closed")
    )
}

fn unverified_absence(saved: &Value) -> bool {
    saved["closeEffect"] == "already_absent" && saved["resourceCleanup"] != "confirmed"
}

fn confirmed(row: &Value, saved: &Value) -> bool {
    if saved["resourceCleanup"] == "uncertain" {
        return false;
    }
    if saved["worker"]["state"] == "not_started" {
        return saved["resourceCleanup"] == "confirmed"
            && revocation_close_effect(saved)
            && saved_revocation(row, saved).is_some();
    }
    match saved["closeEffect"].as_str() {
        Some("already_absent") => !unverified_absence(saved) && archived_exit(saved),
        Some("worker_exited") => archived_exit(saved),
        Some("session_closed") => archived_exit(saved) || saved["worker"]["state"] == "cancelled",
        _ => false,
    }
}

fn exited(evidence: &Value) -> Option<Value> {
    let code = evidence["exitCode"].as_u64().filter(|n| *n <= 255)?;
    (evidence["category"] == "process_exited")
        .then(|| json!({"state":"exited", "exit_code":code, "signal":null}))
}

fn bound_close_launch<'a>(
    row: &'a Value,
    saved: &'a Value,
    known: Option<&'a str>,
) -> Option<&'a str> {
    match saved.get("launchId") {
        Some(launch) => launch.as_str().filter(|id| {
            valid_launch(id)
                && match known {
                    Some(known) => *id == known,
                    None if row["executionId"].is_null() => {
                        let bound = saved["legacyExecutionId"].as_str();
                        bound.is_some()
                            && legacy_selector(row).ok() == bound
                            && legacy_launch_hint(row)
                                .is_ok_and(|hint| hint.is_none_or(|hint| hint == *id))
                    }
                    None => {
                        saved["executionId"] == row["attempt"]
                            && row["executionId"] == row["attempt"]
                    }
                }
        }),
        // R3 kept the binding only in the immutable create reply.
        None if ["dispatch", "round", "attempt"]
            .iter()
            .all(|k| !row[*k].is_null() && row["create"][*k] == row[*k]) =>
        {
            known
        }
        None => None,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn herdr_receipt(state: &Path, create: &Value) -> Result<Value> {
    let Some(name) = create["receipt"].as_str() else {
        return Ok(Value::Null);
    };
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("invalid launch receipt")?;
    ensure!(
        stem.len() == 32
            && stem.bytes().all(|b| b.is_ascii_hexdigit())
            && path.extension() == Some("json".as_ref()),
        "invalid launch receipt"
    );
    Ok(
        super::handoff::read_order(&state.join("launches").join(format!("{stem}.json")))?
            .unwrap_or(Value::Null),
    )
}

pub(super) fn herdr_closes(row: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    let resumed_from = super::assignment::resume_audit(row).map(|event| &event["fromAttempt"]);
    row["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |event| {
            event["operation"] == "close"
                && !resumed_from.is_some_and(|old| {
                    old.is_string()
                        && event["attempt"] == *old
                        && event["dispatch"] == row["dispatch"]
                        && event["round"] == row["round"]
                })
        })
}

fn close_is_pending(event: &Value) -> bool {
    matches!(event["result"].as_str(), Some("pending" | "uncertain"))
}

#[cfg(target_os = "linux")]
pub(super) fn pending_herdr_close(row: &Value) -> Option<&Value> {
    herdr_closes(row)
        .next_back()
        .filter(|event| close_is_pending(event))
}

fn same_execution(row: &Value, event: &Value) -> bool {
    ["dispatch", "round", "attempt"]
        .iter()
        .all(|&k| event[k] == row[k])
}

pub(super) fn close_blocks_callback(state: &Path, row: &Value) -> Result<bool> {
    #[cfg(target_os = "linux")]
    if row["backend"] == "bundled" {
        return bundled::close_blocks_callback(state, row);
    }
    Ok(row["backend"] == "herdr" && herdr_close_blocks_callback(state, row)?)
}

pub(super) fn require_callback_start(_state: &Path, _row: &Value) -> Result<()> {
    #[cfg(target_os = "linux")]
    if _row["backend"] == "bundled" && _row["bundledStartupCommit"] == true {
        let id = _row["dispatch"].as_str().context("missing bundled dispatch ID")?;
        let execution = execution_row(_row)?;
        ensure!(
            crate::bundled::assignment_ready(_state, id, &execution).unwrap_or(false),
            "bundled startup has not committed ready for this attempt"
        );
    }
    Ok(())
}

pub(super) fn herdr_close_blocks_callback(state: &Path, row: &Value) -> Result<bool> {
    let latest = herdr_closes(row).rfind(|event| same_execution(row, event));
    Ok(
        latest.is_some_and(|e| close_is_pending(e) || e["result"] == "completed")
            || archive(state, row)?.is_some(),
    )
}

#[cfg(target_os = "linux")]
pub(super) fn verify_herdr_close_target(row: &Value, event: &Value) -> Result<()> {
    let current = &row["backendHandle"];
    let target = &event["target"];
    ensure!(
        [
            "dispatch",
            "round",
            "attempt",
            "endpoint",
            "service",
            "workspace",
            "pane",
            "terminal",
            "nonce"
        ]
        .iter()
        .all(|&k| target[k] == current[k])
            && same_execution(row, event)
            && same_execution(row, current)
            && current["endpoint"] == row["endpoint"]
            && current["receipt"] == row["create"]["receipt"],
        "close intent target changed"
    );
    let (archived, known) = target["writerProcesses"]
        .as_array()
        .zip(current["writerProcesses"].as_array())
        .context("close intent target changed: missing Herdr writer list")?;
    ensure!(
        archived.first() == known.first(),
        "close intent target changed: Herdr root writer identity differs or is missing"
    );
    ensure!(
        archived.iter().all(|writer| known.contains(writer)),
        "close intent target changed: unrecognized Herdr writer"
    );
    let verify_stop = |binding: &Value| -> Result<bool> {
        let handle: wfc_herdr::Handle = serde_json::from_value(binding.clone())?;
        Ok(handle.verified_stopped()?)
    };
    let current_stopped = verify_stop(current).context("close intent target changed")?;
    let archived_stopped = verify_stop(target).context("close intent target changed")?;
    ensure!(
        current_stopped == archived_stopped
            && (!known.is_empty() || current_stopped)
            && target["writerStoppedEvidence"] == current["writerStoppedEvidence"],
        "close intent target changed: missing or changed Herdr writer stop evidence"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn herdr_worker(evidence: &Value, inspection: &Value) -> Value {
    match evidence["category"].as_str() {
        Some("process_exited") => {
            json!({"state":"exited","exit_code":evidence["exitCode"],"signal":null})
        }
        Some("started") | None if inspection["alive"] == true && inspection["owned"] == true => {
            json!({"state":"running"})
        }
        _ => json!({"state":"unknown"}),
    }
}

pub fn observe(state: &Path, row: &mut Value) -> Result<(Value, Value)> {
    observe_with_mode(state, row, false)
}

pub(super) fn observe_with_mode(
    state: &Path,
    row: &mut Value,
    read_only: bool,
) -> Result<(Value, Value)> {
    if super::assignment::tmux_history(row) {
        let mut inspection = json!({"result":"uncertain","alive":null,
            "diagnostic":super::assignment::TMUX_HISTORY_ONLY});
        let mut worker = if script_preflight_failed(row) {
            row["worker"].clone()
        } else {
            json!({"state":"unknown"})
        };
        if let Ok(physical) = execution_row(row) {
            let saved = archive(state, &physical).ok().flatten();
            if let Some(saved) = &saved {
                copy_close_fields(saved, &mut inspection);
                if archived_exit(saved) || confirmed(&physical, saved) {
                    worker = saved["worker"].clone();
                }
            }
            if let Some(exit) = recorded_tmux_exit(state, &physical, saved.as_ref()) {
                worker = exit;
            }
        }
        return Ok((worker, inspection));
    }
    if script_preflight_failed(row) {
        return Ok((
            row["worker"].clone(),
            json!({"result":"definitely-not-sent","alive":false}),
        ));
    }
    #[cfg(target_os = "linux")]
    if row["backend"] == "bundled" {
        return bundled::observe(state, row);
    }
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::observe(state, row, read_only);
    }
    let _ = read_only;
    Ok((
        json!({"state":"unknown"}),
        json!({"result":"uncertain","alive":null,"diagnostic":"unsupported worker backend"}),
    ))
}

fn recorded_tmux_exit(state: &Path, row: &Value, saved: Option<&Value>) -> Option<Value> {
    let known = receipt_id(row).ok()?;
    let closed = saved.filter(|saved| {
        saved["closeEffect"] == "session_closed" && saved["worker"]["state"] == "cancelled"
    });
    let launch = closed
        .and_then(|saved| bound_close_launch(row, saved, known))
        .or_else(|| known.filter(|_| closed.is_none() || same_execution(row, &row["create"])))?;
    let receipt = private_receipt(state, Some(launch)).ok()?;
    if receipt.get("launchId").is_some_and(|id| id != launch)
        || ["dispatch", "executionId", "attempt"]
            .iter()
            .any(|&key| receipt.get(key).is_some_and(|value| value != &row[key]))
        || receipt.get("round").is_some_and(|value| {
            value != &row["round"] && value.as_str() != Some(row["round"].to_string().as_str())
        })
    {
        return None;
    }
    exited(&receipt)
}

pub(super) fn copy_close_fields(saved: &Value, output: &mut Value) {
    for field in [
        "closeOperationId",
        "closeEffect",
        "revocation",
        "resourceCleanup",
    ] {
        if let Some(value) = saved.get(field) {
            output[field] = value.clone();
        }
    }
}

pub(super) fn read_only_close(state: &Path, row: &Value) -> Result<Option<Value>> {
    if let Some(receipt) = script_preflight_close(row) {
        return Ok(Some(receipt));
    }
    #[cfg(target_os = "linux")]
    if row["backend"] == "bundled" {
        return bundled::read_only_close(state, row);
    }
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::read_only_close(state, row);
    }
    let _ = state;
    Ok(None)
}

pub fn close(state: &Path, row: &Value) -> Result<Value> {
    if let Some(receipt) = script_preflight_close(row) {
        return Ok(receipt);
    }
    #[cfg(target_os = "linux")]
    if row["backend"] == "bundled" {
        return bundled::close(state, row);
    }
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::close(state, row);
    }
    let _ = state;
    super::assignment::require_not_tmux_history(row)?;
    bail!("unsupported worker backend")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn archived_startup_terminal_evidence_does_not_publish() -> Result<()> {
        let state = std::env::temp_dir().join(format!("wfc-tmux-startup-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let launch = "0123456789abcdef0123456789abcdef";
            let dispatch = "d-aabbccdd";
            let attempt = "a-123456789abc";
            let launches = state.join("launches");
            let dispatches = state.join("dispatches");
            std::fs::create_dir_all(&launches)?;
            std::fs::create_dir_all(&dispatches)?;
            let receipt = launches.join(format!("{launch}.json"));
            let evidence = json!({"category":"process_exited","exitCode":143,"launchId":launch,
                "startupExit":{"exitCode":null,"signal":15,"pid":123,"processStart":null}});
            write_json(&receipt, &evidence, None)?;
            let mut row = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"scope":"scope",
                "role":"test","cwd":state,"report":state.join("report"),"backend":"tmux","host":"codex","executionId":attempt});
            let answer = json!({"result":"uncertain","dispatch":dispatch,"attempt":attempt,"round":1,
                "category":"process_exited","exitCode":143,"receipt":receipt});
            write_json(&dispatches.join(format!("{dispatch}.json")), &row, None)?;
            for (field, value) in [
                ("result", json!("accepted")),
                ("category", Value::Null),
                ("category", json!("not_started")),
            ] {
                row["create"] = answer.clone();
                row["create"][field] = value;
                assert!(verified_unconfirmed_tmux_receipt(&state, &row)?.is_none());
            }
            for (field, value) in [
                ("exitCode", json!(17)),
                ("attempt", json!("a-aaaaaaaaaaaa")),
                (
                    "receipt",
                    json!(state.join("wrong").join(format!("{launch}.json"))),
                ),
            ] {
                row["create"] = answer.clone();
                row["create"][field] = value;
                assert!(verified_unconfirmed_tmux_receipt(&state, &row).is_err());
            }
            assert!(super::super::messages::history(&state, dispatch)?.is_empty());
            row["create"] = answer;
            assert_eq!(
                verified_unconfirmed_tmux_receipt(&state, &row)?,
                Some(evidence)
            );
            publish_unconfirmed_tmux_exit(&state, &row, None)?;
            assert!(super::super::messages::history(&state, dispatch)?.is_empty());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(state);
        result
    }

    #[test]
    fn cancellation_requires_the_original_keyed_receipt() -> Result<()> {
        let attempt = "a-123456789abc";
        let launch = "0123456789abcdef0123456789abcdef";
        let row =
            json!({"dispatch":"d-12345678","attempt":attempt,"executionId":attempt,"round":1});
        let fact = json!({"category":"not_started","dispatch":row["dispatch"],"executionId":attempt,
            "closeOperationId":close_operation(&row)?,"launchId":launch});
        let mut saved = json!({"closeEffect":"already_absent","executionId":attempt,
            "closeOperationId":fact["closeOperationId"],"launchId":launch,"worker":{"state":"not_started"}});
        assert!(!confirmed(&row, &saved));
        saved["revocation"] = fact;
        assert!(!confirmed(&row, &saved)); // R5 stored only the worker fact.
        saved["resourceCleanup"] = json!("uncertain");
        assert!(!confirmed(&row, &saved));
        saved["resourceCleanup"] = json!("confirmed");
        assert!(confirmed(&row, &saved));
        saved["closeEffect"] = json!("unstarted_session_closed");
        assert!(confirmed(&row, &saved));
        for name in [
            "executionId",
            "dispatch",
            "launchId",
            "category",
            "closeOperationId",
        ] {
            let mut other = saved.clone();
            other["revocation"][name] = json!("mismatch");
            assert!(!confirmed(&row, &other));
        }
        saved["worker"]["exit_code"] = json!(0);
        assert!(!confirmed(&row, &saved));
        Ok(())
    }

    #[test]
    fn old_absence_does_not_discard_exit_or_confirm_cleanup() {
        let mut old =
            json!({"closeEffect":"already_absent","worker":{"state":"exited","exit_code":37}});
        assert!(archived_exit(&old));
        assert!(!confirmed(&Value::Null, &old));
        old["resourceCleanup"] = json!("confirmed");
        assert!(confirmed(&Value::Null, &old));
    }

    #[test]
    fn archived_close_launch_requires_original_legacy_association() {
        let launch = "0123456789abcdef0123456789abcdef";
        let mut row = json!({"attempt":"a-123456789abc","legacyLaunchId":launch});
        let mut close = json!({"launchId":launch,"legacyExecutionId":row["attempt"]});
        assert_eq!(bound_close_launch(&row, &close, None), Some(launch));
        close["legacyExecutionId"] = json!("a-23456789abcd");
        assert_eq!(bound_close_launch(&row, &close, None), None);
        close["legacyExecutionId"] = row["attempt"].clone();
        row["legacyLaunchId"] = json!("ffffffffffffffffffffffffffffffff");
        assert_eq!(bound_close_launch(&row, &close, None), None);
    }

    #[test]
    fn terminal_evidence_belongs_to_the_current_attempt() {
        let stem = "0123456789abcdef0123456789abcdef";
        let mut row = json!({"dispatch":"d-12345678","attempt":"a-123456789abc","round":1,
            "create":{"receipt":format!("launches/{stem}.json")}});
        assert_eq!(receipt_id(&row).unwrap(), Some(stem)); // first-round R2 archive
        row["round"] = json!(2);
        assert!(receipt_id(&row).is_err());
        for field in ["dispatch", "attempt", "round"] {
            row["create"][field] = row[field].clone();
        }
        assert_eq!(receipt_id(&row).unwrap(), Some(stem));
        row["create"]["attempt"] = json!("a-23456789abcd");
        assert!(receipt_id(&row).is_err());
        assert!(exited(&json!({"category":"process_exited"})).is_none());
        assert!(exited(&json!({"category":"started","exitCode":37})).is_none());
    }
}
