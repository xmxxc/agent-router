//! Bind backend operations to the assignment. A durable close intent turns
//! concurrent requests and retries after a lost reply into receipt reads.
mod input;
pub(in crate::workflow) use input::{
    submit_continue, submit_nudge, submit_reply, APPENDED_USER_ENTRY,
};

use super::{closed_path, execution_row, Launch};
use crate::workflow::{assignment, controller, messages, record_scope, write_json};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(super) fn create(
    state: &Path,
    row: &Value,
    launch: Launch,
    owner: Option<&str>,
) -> Result<Value> {
    let request = launch_request(row, launch)?;
    let answer = if row["bundledStartupCommit"] == true {
        // The original dispatch CLI is the only process that may commit ready.
        Some(crate::bundled::create(state, &request, || {
            let scope = record_scope(row).context("invalid dispatch scope")?;
            let Some(scope_lock) = controller::try_scope_lock(state, scope)? else {
                return Ok(None);
            };
            let Some(ledger) = messages::try_lock_for_publish(state)? else {
                return Ok(None);
            };
            controller::Owners::read(state)?.require(scope, owner, "dispatch")?;
            let id = row["dispatch"].as_str().context("missing dispatch")?;
            let mut reserved = assignment::public_record(&state.join("dispatches"), id)?
                .context("missing bundled dispatch reservation")?;
            ensure!(
                reserved
                    .as_object_mut()
                    .context("invalid dispatch reservation")?
                    .remove("createPending")
                    == Some(Value::Bool(true))
                    && reserved == *row
                    && !close_blocks_callback(state, row)?,
                "bundled dispatch reservation changed before ready"
            );
            Ok(Some(((scope_lock, ledger), move || {
                controller::Owners::read(state)?.require(scope, owner, "dispatch")?;
                Ok(())
            })))
        })?)
    } else {
        create_in_backend(state, &request)?
    };
    launch_answer(answer)
}

fn launch_request(row: &Value, mut launch: Launch) -> Result<Value> {
    for (name, value) in env::vars_os() {
        let name = name
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid environment name"))?;
        if !controller::is_credential(&name) && !launch.env.contains_key(&name) {
            launch.env.insert(
                name,
                json!(value.to_str().context("invalid worker environment")?),
            );
        }
    }
    Ok(
        json!({"op":"create","unit":row["dispatch"],"attempt":row["attempt"],
        "cwd":row["cwd"],"argv":launch.argv,"env":launch.env}),
    )
}

pub(in crate::workflow) fn observe_for_resume(state: &Path, row: &Value) -> Result<(Value, Value)> {
    let ticket = registered_ticket(row)?;
    ensure!(
        ticket["version"] == 2
            && ticket["attempt"] == row["attempt"]
            && (ticket["startupProtocol"] == 1) == (row["bundledStartupCommit"] == true)
            && env::var_os("WFC_BUNDLED_CGROUP_ROOT").is_none()
                == (row["bundledStartupCommit"] == true)
            && ["dispatch", "round", "attempt"]
                .iter()
                .all(|&key| row["create"][key] == row[key]),
        "unverified original bundled launch"
    );
    let (bound, intent) = binding(state, row, &bundled_close_path(state, row)?)?;
    ensure!(
        &bound == ticket && (intent.is_some() || !close_blocks_callback(state, row)?),
        "cannot verify the original bundled close intent"
    );
    let proof = crate::bundled::inspect_resume_cleanup(state, ticket)?;
    let answer = json!({"result":"completed","ticket":ticket,"proof":proof});
    let worker = terminal_worker(&answer).context("missing original bundled worker outcome")?;
    let mut inspection = evidence(&answer);
    inspection["alive"] = json!(false);
    Ok((worker, inspection))
}

// Resume already holds the scope and old execution locks. The published
// attempt and Inbox lock fence readiness for the replacement.
pub(in crate::workflow) fn resume(
    state: &Path,
    row: &Value,
    previous: &Value,
    launch: Launch,
    owner: Option<&str>,
) -> Result<Value> {
    let mut request = launch_request(row, launch)?;
    let ticket = &previous["ticket"];
    let scope = record_scope(row).context("invalid dispatch scope")?;
    let authorize = || -> Result<()> {
        controller::Owners::read(state)?.require(scope, owner, "resume")?;
        Ok(())
    };
    authorize()?;
    crate::bundled::apply_resume_cleanup(state, ticket, &previous["proof"])?;
    authorize()?;
    request["previous"] = ticket.clone();
    let answer = crate::bundled::create(state, &request, || {
        authorize_resume_publication(state, row, authorize)
    })?;
    launch_answer(Some(answer))
}

fn authorize_resume_publication<Check: Fn() -> Result<()>>(
    state: &Path,
    row: &Value,
    authorize: Check,
) -> Result<Option<(messages::PublishLock, Check)>> {
    // Explicit startup already holds the unit lock. Do not wait for the
    // Inbox while another operation can be waiting for that unit.
    let Some(ledger) = messages::try_lock_for_publish(state)? else {
        return Ok(None);
    };
    authorize()?;
    let id = row["dispatch"]
        .as_str()
        .context("missing bundled dispatch ID")?;
    ensure!(
        messages::read_callback_record(&state.join("dispatches"), id)? == *row
            && !close_blocks_callback(state, row)?,
        "bundled resume changed before launch authorization"
    );
    Ok(Some((ledger, authorize)))
}

pub(in crate::workflow) fn create_handoff<G, Check: FnOnce() -> Result<()>>(
    state: &Path,
    row: &Value,
    launch: Launch,
    previous: &Value,
    authorize_ready: impl Fn() -> Result<Option<(G, Check)>>,
) -> Result<Value> {
    let mut request = launch_request(row, launch)?;
    request["previous"] = previous.clone();
    launch_answer(Some(crate::bundled::create(state, &request, authorize_ready)?))
}

fn create_in_backend(state: &Path, request: &Value) -> Result<Option<Value>> {
    let mut command = Command::new("/proc/self/exe");
    controller::strip_credentials(&mut command);
    let mut child = command
        .arg("--bundled")
        .arg(state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start bundled backend")?;
    let sent = serde_json::to_writer(
        child.stdin.take().context("missing bundled request pipe")?,
        request,
    );
    Ok(child
        .wait_with_output()
        .ok()
        .filter(|out| sent.is_ok() && out.status.success())
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok()))
}

fn launch_answer(answer: Option<Value>) -> Result<Value> {
    if let Some(mut answer) = answer.filter(|reply| {
        matches!(
            reply["result"].as_str(),
            Some("accepted" | "definitely-not-sent" | "uncertain" | "refused")
        )
    }) {
        if answer["diagnostic"].is_null() {
            if let Some(reason) = answer["unstarted"]["reason"].as_str() {
                answer["diagnostic"] = json!(if reason == "runtime did not start" {
                    "bundled runtime did not start; verify that the delegation permits creating and entering child cgroups"
                } else {
                    reason
                });
            }
        }
        return Ok(answer);
    }
    // A lost response can follow a real launch. Retain the assignment for
    // read-only lookup by unit and attempt instead of issuing another create.
    Ok(
        json!({"result":"uncertain","diagnostic":"bundled backend did not confirm create; inspect the original dispatch"}),
    )
}

fn bundled_close_path(state: &Path, row: &Value) -> Result<PathBuf> {
    Ok(closed_path(state, execution_row(row)?.as_ref())?.with_extension("closing"))
}

pub(in crate::workflow) fn registered_ticket(row: &Value) -> Result<&Value> {
    let physical = execution_row(row)?;
    let create = &row["create"];
    let ticket = &create["ticket"];
    ensure!(
        row["backend"] == "bundled"
            && row["createPending"] != true
            && create["result"] == "accepted"
            && ticket.is_object()
            && ticket["unit"] == row["dispatch"]
            && ticket["attempt"] == physical["attempt"]
            && row["backendHandle"] == *ticket,
        "bundled input requires the registered current writable execution"
    );
    Ok(ticket)
}

// Check the marker itself, including entries whose contents cannot be read.
pub(super) fn close_blocks_callback(state: &Path, row: &Value) -> Result<bool> {
    match fs::symlink_metadata(bundled_close_path(state, row)?) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn binding(state: &Path, row: &Value, intent: &Path) -> Result<(Value, Option<Value>)> {
    let physical = execution_row(row)?;
    let mut ticket = row["create"]["ticket"].clone();
    let closing = match fs::File::open(intent) {
        Ok(file) => {
            let saved: Value = serde_json::from_reader(file)?;
            // R1 wrote the bare ticket. New intents bind the pre-close worker
            // outcome and business round to the same generation.
            let target = saved.get("ticket").unwrap_or(&saved);
            ensure!(
                target.is_object()
                    && (ticket.is_null() || target == &ticket)
                    && (saved.get("ticket").is_none() || saved["round"] == row["round"]),
                "bundled close intent does not match assignment"
            );
            ticket = target.clone();
            Some(saved)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if ticket.is_null() {
        // The publisher may have lost create's response. Only lookup of this
        // assignment can recover the binding. Never repeat create.
        let lookup = crate::bundled::lookup_assignment(
            state,
            row["dispatch"].as_str().context("missing dispatch")?,
            &physical["attempt"],
        )?;
        ensure!(
            lookup["result"] == "accepted",
            "bundled create binding is unknown"
        );
        ticket = lookup["ticket"].clone();
    }
    ensure!(
        ticket.is_object()
            && ticket["unit"] == row["dispatch"]
            && ticket["attempt"] == physical["attempt"]
            && (row["backendHandle"].is_null() || row["backendHandle"] == ticket),
        "bundled ticket does not match assignment"
    );
    Ok((ticket, closing))
}

fn unknown() -> Value {
    json!({"result":"uncertain","cleanup":"unknown"})
}

fn evidence(answer: &Value) -> Value {
    // Inspect also returns the worker's terminal contents. Expose only the
    // lifecycle fields, even when the terminal happens to contain credentials.
    let mut view = json!({});
    for key in [
        "result",
        "diagnostic",
        "scope",
        "ticket",
        "cleanup",
        "proof",
        "execution_cleanup",
        "execution_proof",
        "reconcile",
        "category",
        "startup",
        "startupIdentity",
    ] {
        if let Some(value) = answer.get(key) {
            view[key] = value.clone();
        }
    }
    if view["result"] == "completed" && view["proof"]["proves"] == "execution-empty" {
        view["cleanup"] = json!("confirmed");
    }
    view
}

fn terminal_worker(answer: &Value) -> Option<Value> {
    let proof = answer.get("execution_proof").unwrap_or(&answer["proof"]);
    match proof["state"].as_str() {
        Some("closed" | "startup-aborted") => {
            Some(json!({"state":"cancelled","exit_code":null,"signal":null}))
        }
        Some("exited" | "faulted") => {
            Some(json!({"state":"exited","exit_code":null,"signal":null}))
        }
        _ => None,
    }
}

pub(super) fn observe(state: &Path, row: &Value) -> Result<(Value, Value)> {
    let mut answer = unknown();
    let mut prior_worker = None;
    let path = bundled_close_path(state, row)?;
    if let Ok((ticket, closing)) = binding(state, row, &path) {
        let op = if let Some(intent) = closing {
            prior_worker = intent
                .get("workerOnClose")
                .filter(|w| matches!(w["state"].as_str(), Some("exited" | "cancelled")))
                .cloned();
            "cleanup-receipt"
        } else {
            "inspect"
        };
        answer = crate::bundled::assignment_operation(state, &ticket, op).0;
        answer["ticket"] = ticket;
    }
    let mut view = evidence(&answer);
    let live = answer["result"] == "accepted"
        && answer["identity"]["worker"].is_object()
        && answer["data"]["exit"].is_null()
        && answer["data"]["read_error"].is_null();
    let worker = prior_worker
        .or_else(|| terminal_worker(&answer))
        .unwrap_or_else(|| {
            json!({"state": if live {"running"} else if answer["startup"] == "unstarted" {
                "not_started"
            } else {"unknown"}})
        });
    if answer["scope"] != "execution" {
        if live {
            view["alive"] = json!(true);
            view["owned"] = json!(true);
        } else if view["cleanup"] == "confirmed" {
            view["alive"] = json!(false);
        }
    }
    Ok((worker, view))
}

fn close_new(state: &Path, row: &Value, path: &Path, ticket: &Value) -> Result<Value> {
    let read_receipt = || crate::bundled::assignment_operation(state, ticket, "cleanup-receipt").0;
    // A signed terminal receipt can establish the business outcome before
    // cleanup. Keep that outcome even if the receipt later becomes unreadable.
    let prior = terminal_worker(&read_receipt());
    let mut intent = json!({"ticket":ticket,"round":row["round"],"workerOnClose":prior,
        "requestId":uuid::Uuid::new_v4().to_string()});
    let dir = path.parent().context("missing dispatch directory")?;
    let directory = fs::File::open(dir)?;
    let validate_assignment = || -> Result<()> {
        let current =
            assignment::public_record(dir, row["dispatch"].as_str().context("missing dispatch")?)?
                .context("missing bundled assignment")?;
        ensure!(
            current["backend"] == "bundled"
                && current["round"] == row["round"]
                && current["attempt"] == row["attempt"]
                && record_scope(&current) == record_scope(row)
                && current["controllerOwner"].is_null()
                && (current["create"]["ticket"].is_null()
                    || &current["create"]["ticket"] == ticket)
                && (current["backendHandle"].is_null() || &current["backendHandle"] == ticket),
            "bundled assignment changed before close"
        );
        Ok(())
    };
    let messages = messages::lock_for_publish(state)?;
    validate_assignment()?;
    write_json(path, &intent, None)?;
    if let Err(error) = directory.sync_all() {
        // No RPC can have started. Done and other closes are still excluded.
        fs::remove_file(path).context("withdraw unsent bundled close")?;
        directory
            .sync_all()
            .context("sync withdrawal of unsent bundled close")?;
        return Err(error).context("sync bundled close intent");
    }
    messages.release_for_bundled_close()?;
    if prior.is_some() {
        return Ok(read_receipt());
    }
    let (answer, withdraw) = crate::bundled::assignment_operation(state, ticket, "close");
    // The worker may have exited while the intent waited for the ledger.
    // Preserve a newly verified natural exit before returning that proof.
    if let Some(worker) = terminal_worker(&answer).filter(|w| w["state"] == "exited") {
        // A confirmed terminal receipt is independently readable when
        // the optional snapshot update is blocked by the write fence.
        let Ok(_messages) = messages::lock_for_publish(state) else {
            return Ok(answer);
        };
        validate_assignment()?;
        require_close_intent(&intent, &serde_json::from_reader(fs::File::open(path)?)?)?;
        intent["workerOnClose"] = worker;
        write_json(path, &intent, None)?;
        directory.sync_all()?;
    } else if withdraw {
        // Scope and attempt locks are still held. Undo only the marker this
        // invocation wrote, without reopening any ledger publication path.
        messages.serialize_bundled_close_withdrawal()?;
        validate_assignment()?;
        require_close_intent(&intent, &serde_json::from_reader(fs::File::open(path)?)?)?;
        fs::remove_file(path)?;
        directory.sync_all()?;
    }
    Ok(answer)
}

fn require_close_intent(expected: &Value, current: &Value) -> Result<()> {
    ensure!(
        current == expected && expected["requestId"].is_string(),
        "bundled close intent changed before withdrawal"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_resume_rechecks_expired_owner_after_unit_lock() -> Result<()> {
        let state = std::env::temp_dir().join(format!("wfc-resume-lease-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&state)?;
        let result = (|| -> Result<()> {
            let dir = state.join("dispatches");
            fs::create_dir(&dir)?;
            let dispatch = "d-1234abcd";
            let attempt = "a-0123456789ab";
            let scope = "resume-fixture";
            let owner = "synthetic delegated owner";
            let row = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"scope":scope,
                "role":"worker","backend":"bundled","host":"codex",
                "audit":[{"id":1,"operation":"resume","result":"pending"}]});
            write_json(&dir.join(format!("{dispatch}.json")), &row, None)?;
            let lease = |until| {
                write_json(
                    &state.join("controller-owners.json"),
                    &json!({(scope):{"owner":"fixture","generation":1,"until":until,
                        "fingerprint":messages::body_hash(owner),"retired":[]}}),
                    None,
                )
            };
            lease(chrono::Utc::now().timestamp() as f64 + 600.0)?;
            let authorize = || -> Result<()> {
                controller::Owners::read(&state)?.require(scope, Some(owner), "resume")?;
                Ok(())
            };
            let _scope = controller::lock_scope(&state, scope)?;
            let _execution = assignment::close_lock(&state, &row)?;
            authorize()?;
            let path = state.join("synthetic-unit.lock");
            fs::File::create(&path)?;
            let open_unit = || fs::OpenOptions::new().read(true).write(true).open(&path);
            let mut tries = 0;
            let mut busy = Some(messages::lock_for_publish(&state)?);
            let (guard, publication) = crate::bundled::authorize_create(
                || {
                    tries += 1;
                    if tries == 2 {
                        drop(busy.take());
                    }
                    let file = open_unit()?;
                    // A fresh lock succeeds only if the first guard was released.
                    assert!(file.try_lock().is_ok());
                    Ok(file)
                },
                true,
                &|| authorize_resume_publication(&state, &row, authorize),
            )?;
            assert_eq!(tries, 2);
            assert!(publication.is_some());
            drop(publication);
            drop(guard);
            let result = crate::bundled::authorize_create(
                || {
                    let file = open_unit()?;
                    file.lock()?;
                    // Model the lease observed after a wait without a timer
                    // or concurrent writer.
                    lease(1.0)?;
                    Ok(file)
                },
                true,
                &|| {
                    let probe = open_unit()?;
                    assert!(matches!(
                        probe.try_lock(),
                        Err(std::fs::TryLockError::WouldBlock)
                    ));
                    authorize_resume_publication(&state, &row, authorize)
                },
            );
            let denied = result.err().context("expired owner must prevent launch")?;
            assert!(denied.to_string().contains("stale_controller_owner"));
            Ok(())
        })();
        fs::remove_dir_all(&state)?;
        result
    }

    #[test]
    fn unsent_withdrawal_requires_the_original_intent() {
        let intent = json!({"ticket":{"unit":"fixture"},"round":1,"requestId":"first"});
        assert!(require_close_intent(&intent, &intent).is_ok());
        let mut later = intent.clone();
        later["requestId"] = json!("later");
        assert!(require_close_intent(&intent, &later).is_err());
        later = intent.clone();
        later["ticket"]["unit"] = json!("other");
        assert!(require_close_intent(&intent, &later).is_err());
    }
}

/// Only a confirmed exact receipt is eligible for the read-only close path.
pub(super) fn read_only_close(state: &Path, row: &Value) -> Result<Option<Value>> {
    let path = bundled_close_path(state, row)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let (ticket, Some(_)) = binding(state, row, &path)? else {
        return Ok(None);
    };
    let answer = crate::bundled::assignment_operation(state, &ticket, "cleanup-receipt").0;
    Ok((answer["result"] == "completed").then(|| close_reply(&answer, row)))
}

pub(super) fn close(state: &Path, row: &Value) -> Result<Value> {
    let path = bundled_close_path(state, row)?;
    let mut target = binding(state, row, &path);
    // Existing intents may be replayed from a read-only archive. A first
    // close rechecks after locking across both reservation and publication.
    let _lock;
    if matches!(target, Ok((_, None))) {
        _lock = assignment::close_lock(state, row)?;
        target = binding(state, row, &path);
    }
    let answer = match target {
        Err(error) => {
            json!({"result":"uncertain","cleanup":"unknown","diagnostic":error.to_string()})
        }
        Ok((ticket, Some(_))) => {
            crate::bundled::assignment_operation(state, &ticket, "cleanup-receipt").0
        }
        Ok((ticket, None)) => close_new(state, row, &path, &ticket)?,
    };
    ensure!(
        answer["result"] != "refused",
        "close refused: {}",
        answer["diagnostic"]
    );
    Ok(close_reply(&answer, row))
}

fn close_reply(answer: &Value, row: &Value) -> Value {
    let mut answer = evidence(answer);
    answer["operation"] = json!("close");
    for key in ["dispatch", "round", "attempt"] {
        answer[key] = row[key].clone();
    }
    answer
}
