//! Public lifecycle clients for tmux and Herdr.
use super::{host::Launch, valid_identity, write_json};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

// Public assignments published with the session-aware tmux protocol.
pub(super) const LAUNCH_PROTOCOL: u64 = 8;

#[cfg(target_os = "linux")]
pub fn tmux_socket() -> Result<PathBuf> {
    let socket = env::var_os("WFC_TMUX_SOCKET")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var("TMUX")
                .ok()
                .and_then(|v| v.rsplitn(3, ',').nth(2).map(PathBuf::from))
        })
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into()))
                .join(format!("tmux-{}", unsafe { libc::getuid() }))
                .join("default")
        });
    std::path::absolute(socket).context("resolve tmux socket")
}

fn call(state: &Path, row: &Value, mut request: Value) -> Result<Value> {
    ensure!(
        row["backend"] == "tmux",
        "{} backend adapter is unavailable in this build",
        row["backend"]
    );
    request["dispatch"] = row["dispatch"].clone();
    let mut command = Command::new(env::current_exe()?);
    command.arg("--tmux-backend").env("WFC_STATE", state);
    if let Some(socket) = row["tmuxSocket"].as_str() {
        command.env("WFC_TMUX_SOCKET", socket);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let sent = serde_json::to_writer(
        child.stdin.take().context("missing backend input")?,
        &request,
    );
    // Once the pipe has been offered the request, a failed wait/read cannot
    // establish that the backend did nothing. Keep the caller's assignment.
    let out = match child.wait_with_output() {
        Ok(out) => out,
        Err(e) => {
            return Ok(
                json!({"result":"uncertain","diagnostic":format!("could not collect tmux backend response: {e}")}),
            )
        }
    };
    if out.status.code() == Some(2)
        && out.stdout.is_empty()
        && out.stderr.strip_suffix(b"\n") == Some(crate::UNEXPECTED_ARGUMENTS.as_bytes())
    {
        bail!("tmux backend is unavailable in this build (requires #31)");
    }
    if sent.is_err() || !out.status.success() {
        return Ok(
            json!({"result":"uncertain","diagnostic":"tmux backend did not confirm the request"}),
        );
    }
    let reply: Option<Value> = serde_json::from_slice(&out.stdout).ok();
    match reply {
        Some(reply)
            if matches!(
                reply["result"].as_str(),
                Some("accepted" | "refused" | "definitely-not-sent" | "uncertain")
            ) || (matches!(request["op"].as_str(), Some("close" | "revoke"))
                && reply["result"] == "completed") =>
        {
            Ok(reply)
        }
        _ => Ok(json!({"result":"uncertain","diagnostic":"invalid tmux backend response"})),
    }
}

pub fn create(state: &Path, row: &Value, launch: Launch) -> Result<Value> {
    call(
        state,
        row,
        json!({"op":"create","executionId":row["executionId"],"freshDispatch":true,"cwd":row["cwd"],"label":row["label"],"argv":launch.argv,"env":launch.env}),
    )
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

fn existing(state: &Path, row: &Value, op: &str) -> Result<Value> {
    let attempt = row["attempt"]
        .as_str()
        .filter(|v| valid_identity(v, "a-"))
        .context("missing existing execution identity")?;
    let mut request = json!({"op":op});
    if !row["executionId"].is_null() {
        ensure!(
            row["executionId"] == attempt,
            "execution identity does not match this attempt"
        );
        request["executionId"] = json!(attempt);
    } else {
        request["legacyExecutionId"] = json!(legacy_selector(row)?);
        if let Some(pane) = row.get("legacyWorkerPane") {
            request["legacyWorkerPane"] = pane.clone();
        }
        if let Some(hint) = legacy_launch_hint(row)? {
            request["legacyLaunchId"] = json!(hint);
        }
    }
    if matches!(op, "close" | "revoke") {
        request["closeOperationId"] = json!(close_operation(row)?);
    }
    call(state, row, request)
}

fn receipt_id(row: &Value) -> Result<Option<&str>> {
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

// The service resolves a lost create response using the execution registered
// before launch. Only read the receipt under this state for that exact launch.
fn inspect_launch<'a>(
    state: &Path,
    row: &'a Value,
    inspection: &'a Value,
) -> Result<Option<&'a str>> {
    let known = receipt_id(row)?;
    // Session reachability and a verified historical launch are independent.
    let historical = inspection["result"] == "uncertain" && !inspection["launchId"].is_null();
    if inspection["result"] != "accepted" && !historical {
        return Ok(known);
    }
    if row["executionId"].is_null() {
        let selector = legacy_selector(row)?;
        if inspection["launchId"].is_null() {
            ensure!(
                row["createPending"] != true
                    || known.is_some()
                    || (row["legacyWorkerPane"].is_string() && inspection["workerBound"] == true),
                "missing verified launch or worker for legacy reservation"
            );
            ensure!(
                legacy_launch_hint(row)?.is_none() || known.is_some(),
                "unconfirmed legacy launch hint"
            );
            return Ok(known);
        }
        ensure!(
            inspection["legacyExecutionId"] == selector,
            "inspection identifies another legacy execution"
        );
    } else {
        ensure!(
            inspection["executionId"] == row["attempt"] && row["executionId"] == row["attempt"],
            "inspection identifies another attempt"
        );
    }
    let id = inspection["launchId"]
        .as_str()
        .filter(|id| valid_launch(id))
        .context("missing original launch identity")?;
    ensure!(
        known.is_none_or(|known| known == id)
            && legacy_launch_hint(row)?.is_none_or(|hint| hint == id),
        "inspection identifies another launch"
    );
    let path = state
        .canonicalize()?
        .join("launches")
        .join(format!("{id}.json"));
    ensure!(
        inspection["receipt"].as_str().map(Path::new) == Some(path.as_path()),
        "unexpected launch receipt path"
    );
    Ok(Some(id))
}

fn private_receipt(state: &Path, launch: Option<&str>) -> Result<Value> {
    let Some(stem) = launch else {
        return Ok(Value::Null);
    };
    match File::open(state.join("launches").join(format!("{stem}.json"))) {
        Ok(f) => Ok(serde_json::from_reader(f)?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(e).context("open host launch receipt"),
    }
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

pub(super) fn archive(state: &Path, row: &Value) -> Result<Option<Value>> {
    match File::open(closed_path(state, row)?) {
        Ok(f) => {
            let saved: Value = serde_json::from_reader(f)?;
            ensure!(
                saved["dispatch"] == row["dispatch"]
                    && saved["attempt"] == row["attempt"]
                    && saved["round"] == row["round"],
                "closed record identity mismatch"
            );
            Ok(Some(saved))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
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

fn revocation_reply(state: &Path, row: &Value, answer: &Value) -> Result<Option<Value>> {
    (answer["category"] == "not_started")
        .then(|| revocation(state, row, answer))
        .transpose()
}

fn revocation(state: &Path, row: &Value, answer: &Value) -> Result<Value> {
    ensure!(
        matches_revocation(row, answer),
        "close uncertain: cancellation does not identify this reservation"
    );
    let stem = answer["launchId"].as_str().context("missing launchId")?;
    let prefix = state.canonicalize()?.join("launches").join(stem);
    let path = Path::new(
        answer["cancellationEvidence"]
            .as_str()
            .context("missing cancellation evidence")?,
    );
    ensure!(
        path == prefix.with_extension("json") || path == prefix.with_extension("revoked"),
        "unexpected cancellation evidence path"
    );
    let fact: Value = serde_json::from_reader(File::open(path)?)?;
    ensure!(
        matches_revocation(row, &fact) && fact["launchId"] == stem,
        "close uncertain: cancellation receipt mismatch"
    );
    Ok(fact)
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

fn supports_session_confirmation(row: &Value) -> bool {
    row["tmuxLaunchProtocol"] == LAUNCH_PROTOCOL
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

// The close effect describes the session. Its supervisor can still publish
// a process exit later. Both archived reads and close replays use this view.
fn closed_worker(state: &Path, row: &Value, saved: &Value) -> Result<Value> {
    let worker = &saved["worker"];
    if saved["closeEffect"] == "session_closed" && worker["state"] == "cancelled" {
        let known = receipt_id(row)?;
        let id = match saved.get("launchId") {
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
            // R3 kept the launch binding only in the immutable create reply.
            None if ["dispatch", "round", "attempt"]
                .iter()
                .all(|k| !row[*k].is_null() && row["create"][*k] == row[*k]) =>
            {
                known
            }
            // R2 embedded the original, unscoped create reply in the assignment.
            // Verify its selector with the backend before enriching an R3 archive.
            None if known.is_some() && row["round"] == 1 && row["executionId"].is_null() => {
                match existing(state, row, "inspect") {
                    Ok(reply) if reply["result"] == "accepted" && reply["alive"] == false => known,
                    _ => None,
                }
            }
            None => None,
        };
        if let Some(exit) = exited(&private_receipt(state, id)?) {
            return Ok(exit);
        }
    }
    Ok(worker.clone())
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
    private_receipt(state, Some(stem))
}

pub(super) fn herdr_closes(row: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    row["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["operation"] == "close")
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

pub(super) fn herdr_close_blocks_done(state: &Path, row: &Value) -> Result<bool> {
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
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::observe(state, row);
    }
    let saved = archive(state, row)?.unwrap_or(Value::Null);
    if confirmed(row, &saved) {
        let mut observation = json!({"result":"completed","alive":false});
        copy_close_fields(&saved, &mut observation);
        return Ok((closed_worker(state, row, &saved)?, observation));
    }

    let (worker, mut inspection) = inspect_worker(state, row, &saved)?;
    let revoked = saved_revocation(row, &saved).filter(|_| worker["state"] == "not_started");
    // A legacy release can replay an old absence from the backend's history,
    // even if the public archive never made it to disk.
    let legacy_absence = !supports_session_confirmation(row) && inspection["alive"] == false;
    if revoked.is_some()
        || unverified_absence(&saved)
        || legacy_absence
        || saved["resourceCleanup"] == "uncertain"
    {
        inspection["result"] = json!("uncertain");
        if inspection["alive"] != true || inspection["owned"] != true {
            inspection["alive"] = Value::Null;
        }
        inspection["resourceCleanup"] = json!("uncertain");
        if let Some(fact) = revoked {
            inspection["revocation"] = fact.clone();
            inspection["closeOperationId"] = fact["closeOperationId"].clone();
        } else if archived_exit(&saved)
            && close_operation(row).is_ok_and(|op| saved["closeOperationId"] == op)
        {
            inspection["closeOperationId"] = saved["closeOperationId"].clone();
        }
    }
    Ok((worker, inspection))
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

fn inspect_worker(state: &Path, row: &Value, saved: &Value) -> Result<(Value, Value)> {
    let mut inspection = existing(state, row, "inspect")
        .unwrap_or_else(|e| json!({"result":"uncertain","diagnostic":e.to_string()}));
    let evidence = match inspect_launch(state, row, &inspection) {
        Ok(id) => private_receipt(state, id)?,
        Err(e) => {
            inspection = json!({"result":"uncertain","diagnostic":e.to_string()});
            Value::Null
        }
    };
    let worker = if let Some(exit) = exited(&evidence) {
        exit
    } else if archived_exit(saved) || saved_revocation(row, saved).is_some() {
        saved["worker"].clone()
    } else if matches!(evidence["category"].as_str(), Some("started") | None)
        && inspection["result"] == "accepted"
        && inspection["alive"] == true
        && inspection["owned"] == true
        && inspection["workerBound"] == true
    {
        json!({"state":"running"})
    } else {
        json!({"state":"unknown"})
    };
    Ok((worker, inspection))
}

pub fn close(state: &Path, row: &Value) -> Result<Value> {
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::close(state, row);
    }
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    // Archived replay works on read-only state. A first close serializes on a
    // stable per-attempt file regardless of which publication the reader saw.
    let saved = archive(state, row)?.unwrap_or(Value::Null);
    let _lock;
    let saved = if confirmed(row, &saved) {
        saved
    } else {
        _lock = super::assignment::close_lock(state, row)?;
        archive(state, row)?.unwrap_or(Value::Null)
    };
    let mut receipt = json!({"dispatch":id,"operation":"close","result":"completed","round":row["round"],"attempt":row["attempt"]});
    if confirmed(row, &saved) {
        receipt["worker"] = closed_worker(state, row, &saved)?;
        copy_close_fields(&saved, &mut receipt);
        receipt["diagnostic"] = json!("session was already gone");
        return Ok(receipt);
    }

    let (before, observed) = inspect_worker(state, row, &saved)?;
    let previous_revocation = saved_revocation(row, &saved);
    let pending_native = (row["createPending"] == true || row["create"]["result"] == "uncertain")
        && row["executionId"].is_string()
        && row["executionId"] == row["attempt"]
        && receipt_id(row)?.is_none();
    let recovery =
        previous_revocation.is_some() || (pending_native && observed["result"] == "uncertain");
    ensure!(
        observed["result"] == "accepted" || recovery,
        "backend inspection {}: {}",
        observed["result"],
        observed["diagnostic"]
    );
    if observed["alive"] == true {
        ensure!(
            observed["owned"] == true,
            "tmux session is not owned by this dispatch"
        );
    }

    let launch = inspect_launch(state, row, &observed)?;
    let mut answer = existing(state, row, if recovery { "revoke" } else { "close" })?;
    let mut cancellation = revocation_reply(state, row, &answer)?;
    if let Some(previous) = previous_revocation {
        // A legacy archive can preserve the worker fact across disconnection.
        // A fresh keyed proof is required before retrying session cleanup.
        ensure!(
            cancellation.as_ref() == Some(previous) && before["state"] == "not_started",
            "close uncertain: conflicting worker evidence"
        );
        if answer["result"] == "uncertain" {
            answer = existing(state, row, "close")?;
            cancellation = revocation_reply(state, row, &answer)?;
        }
    }

    if let Some(fact) = cancellation {
        ensure!(
            matches!(before["state"].as_str(), Some("unknown" | "not_started"))
                && previous_revocation.is_none_or(|previous| previous == &fact)
                && launch.is_none_or(|id| fact["launchId"] == id),
            "close uncertain: conflicting worker evidence"
        );
        let cleanup_confirmed = answer["result"] == "completed" && revocation_close_effect(&answer);
        let mut snapshot = json!({
            "dispatch":id, "round":row["round"], "attempt":row["attempt"],
            "executionId":fact["executionId"], "launchId":fact["launchId"],
            "closeOperationId":fact["closeOperationId"], "revocation":fact,
            "worker":{"state":"not_started"},
            "resourceCleanup":if cleanup_confirmed { "confirmed" } else { "uncertain" }
        });
        if cleanup_confirmed {
            snapshot["closeEffect"] = answer["closeEffect"].clone();
        }
        write_json(&closed_path(state, row)?, &snapshot, None)?;
        ensure!(
            cleanup_confirmed,
            "close uncertain: worker launch was revoked; tmux cleanup is unconfirmed ({})",
            answer["diagnostic"]
        );
        receipt["worker"] = snapshot["worker"].clone();
        copy_close_fields(&snapshot, &mut receipt);
        return Ok(receipt);
    }

    ensure!(
        answer["result"] == "completed",
        "close {}: {}",
        answer["result"],
        answer["diagnostic"]
    );
    ensure!(
        !recovery,
        "close uncertain: missing verified reservation cancellation"
    );
    ensure!(
        answer["closeOperationId"] == close_operation(row)?,
        "close uncertain: backend did not confirm the close operation"
    );
    let worker = match answer["closeEffect"].as_str() {
        Some("already_absent")
            if !supports_session_confirmation(row) || unverified_absence(&saved) =>
        {
            bail!("close uncertain: old absence has no independently verified cleanup")
        }
        Some("already_absent" | "worker_exited") => {
            if before["state"] == "exited" {
                before
            } else {
                exited(&private_receipt(state, launch)?)
                    .context("close uncertain: missing same-attempt worker exit evidence")?
            }
        }
        Some("session_closed") if before["state"] == "exited" => before,
        Some("session_closed") => json!({"state":"cancelled","exit_code":null,"signal":null}),
        _ => bail!("close uncertain: backend did not confirm the close effect"),
    };
    if answer["closeEffect"] == "already_absent" {
        receipt["diagnostic"] = json!("session was already gone");
    }
    let snapshot = json!({"launchId":launch, "executionId":row["executionId"], "legacyExecutionId":observed["legacyExecutionId"], "closeOperationId":answer["closeOperationId"], "dispatch":id,"round":row["round"],"attempt":row["attempt"],
        "worker":worker,"closeEffect":answer["closeEffect"],"resourceCleanup":"confirmed"});
    write_json(&closed_path(state, row)?, &snapshot, None)?;
    receipt["worker"] = worker;
    copy_close_fields(&snapshot, &mut receipt);
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn recovered_launch_requires_the_registered_attempt_and_receipt_path() -> Result<()> {
        let state = std::env::temp_dir().canonicalize()?;
        let launch = "0123456789abcdef0123456789abcdef";
        let row = json!({"dispatch":"d-12345678","attempt":"a-123456789abc","round":2,
            "executionId":"a-123456789abc",
            "create":{"result":"uncertain","dispatch":"d-12345678","attempt":"a-123456789abc","round":2}});
        let mut answer = json!({"result":"accepted","executionId":row["attempt"],"launchId":launch,
            "receipt":state.join("launches").join(format!("{launch}.json"))});
        assert_eq!(inspect_launch(&state, &row, &answer)?, Some(launch));
        answer["executionId"] = json!("a-23456789abcd");
        assert!(inspect_launch(&state, &row, &answer).is_err());
        answer["executionId"] = row["attempt"].clone();
        answer["launchId"] = json!("ffffffffffffffffffffffffffffffff");
        assert!(inspect_launch(&state, &row, &answer).is_err());
        Ok(())
    }

    #[test]
    fn offline_legacy_receipt_requires_a_confirmed_association() -> Result<()> {
        let state = std::env::temp_dir().canonicalize()?;
        let id = "0123456789abcdef0123456789abcdef";
        let row = json!({"dispatch":"d-12345678","round":1,"attempt":"a-123456789abc","legacyLaunchId":id});
        let mut offline = json!({"result":"uncertain"});
        assert_eq!(inspect_launch(&state, &row, &offline)?, None);
        offline["legacyExecutionId"] = row["attempt"].clone();
        offline["launchId"] = json!(id);
        offline["receipt"] = json!(state.join("launches").join(format!("{id}.json")));
        assert_eq!(inspect_launch(&state, &row, &offline)?, Some(id));
        offline["legacyExecutionId"] = json!("a-aaaaaaaaaaaa");
        assert!(inspect_launch(&state, &row, &offline).is_err());
        Ok(())
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
