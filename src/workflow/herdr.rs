//! R1 Herdr assignments. A stable lock covers inspection, input claims, close
//! and reconciliation while atomic replacements publish complete record generations.
use super::{
    backend, controller, host, read_record, record_scope, state_dir, valid_id, valid_identity,
    write_json,
};
use crate::host_launch;
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wfc_herdr::{FailureKind, Handle, Launch, Mapping, Process};

pub const USAGE: &str = "wfc herdr-reconcile DISPATCH --pane P --terminal T --actor NAME --evidence TEXT [--workspace W] [--old-writer-evidence TEXT] [--owner-token TOKEN]";

pub(super) fn native_state(state: &Path) -> Result<()> {
    ensure!(
        !state.join("router.db").try_exists()?,
        "legacy router database requires owner and assignment migration"
    );
    Ok(())
}

fn launch_receipt(state: &Path, create: &Value) -> Result<Value> {
    let launch = backend::herdr_receipt(state, create)?;
    ensure!(
        launch["category"] != "process_exited" || launch["exitCode"].is_i64(),
        "invalid host launch exit code"
    );
    Ok(launch)
}

fn launch_binding(handle: &Value) -> Value {
    json!({"endpoint":handle["endpoint"],"service":handle["service"],
        "workspace":handle["workspace"],"pane":handle["pane"],"terminal":handle["terminal"],
        "nonce":handle["nonce"],"receipt":handle["receipt"]})
}

pub(super) fn verify_handoff_source(state: &Path, row: &Value) -> Result<()> {
    let launch = backend::receipt_id(row)?.context("missing original Herdr launch")?;
    let binding = &row["create"]["herdrBinding"];
    let receipt = launch_receipt(state, &row["create"])?;
    ensure!(
        row["executionId"] == row["attempt"]
            && row["create"]["receipt"]
                == json!(state.join("launches").join(format!("{launch}.json")))
            && binding == &launch_binding(&row["backendHandle"])
            && [
                "endpoint",
                "workspace",
                "pane",
                "terminal",
                "nonce",
                "receipt"
            ]
            .iter()
            .all(|&key| binding[key].as_str().is_some_and(|value| !value.is_empty()))
            && binding["service"].is_object()
            && receipt["backend"] == "herdr"
            && matches!(
                receipt["category"].as_str(),
                Some("started" | "process_exited")
            )
            && receipt["treeTracked"] == true
            && receipt["launchId"] == launch
            && receipt["dispatch"] == row["dispatch"]
            && receipt["attempt"] == row["attempt"]
            && row["round"]
                .as_u64()
                .is_some_and(|round| receipt["round"] == round.to_string()),
        "original Herdr launch has no matching worker tree supervision"
    );
    Ok(())
}

pub(super) fn handoff_close(row: &Value, source: &Value) -> Result<Value> {
    let attempt = source["attempt"]
        .as_str()
        .context("missing Herdr attempt")?;
    let closed = backend::herdr_closes(row)
        .rfind(|event| event["attempt"] == attempt)
        .context("missing original Herdr close audit")?;
    ensure!(
        closed["result"] == "completed"
            && closed["closeOperationId"] == format!("close:{attempt}")
            && closed["executionId"] == source["executionId"]
            && closed["dispatch"] == source["dispatch"]
            && closed["round"] == source["round"]
            && launch_binding(&closed["target"]) == source["create"]["herdrBinding"],
        "Herdr close does not confirm this original execution"
    );
    if row["attempt"] == attempt {
        backend::verify_herdr_close_target(row, closed)?;
    }
    let mut evidence = closed.clone();
    evidence["resourceCleanup"] = json!("confirmed");
    Ok(evidence)
}

pub(super) fn handoff_tree_stopped(state: &Path, source: &Value) -> Result<Option<Value>> {
    Ok(
        backend::handoff_tree_stopped(state, source)?.filter(|evidence| {
            evidence["backend"] == "herdr"
                && evidence["treeTracked"] == true
                && evidence["category"] == "process_exited"
        }),
    )
}

const INVALID_CLOSE_WORKER: &str = "invalid closed worker outcome";

fn valid_close_worker(worker: &Value) -> bool {
    worker["state"] == "cancelled" || (worker["state"] == "exited" && worker["exit_code"].is_i64())
}

struct Assignment<'a> {
    state: &'a Path,
    _lock: File,
    path: PathBuf,
    row: Value,
    published: bool,
}

impl<'a> Assignment<'a> {
    fn lock_unowned(state: &'a Path, id: &str, operation: &str) -> Result<(Self, File)> {
        ensure!(valid_id(id), "invalid dispatch ID");
        native_state(state)?;
        let row = read_record(&state.join("dispatches").join(format!("{id}.json")))?;
        let scope = record_scope(&row).context("invalid dispatch scope")?;
        let authorize = || controller::Owners::read(state)?.require(scope, None, operation);
        authorize()?;
        let scope_lock = controller::lock_scope(state, scope)?;
        authorize()?;
        let unit = Self::lock(state, id, None)?;
        ensure!(
            record_scope(&unit.row) == Some(scope),
            "Herdr scope changed"
        );
        Ok((unit, scope_lock))
    }

    fn lock(state: &'a Path, id: &str, new: Option<Value>) -> Result<Self> {
        let create_lock = new.is_some();
        Self::lock_with(state, id, new, create_lock, |file| {
            file.lock()?;
            Ok(true)
        })?
        .context("missing locked Herdr assignment")
    }

    fn lock_with(
        state: &'a Path,
        id: &str,
        new: Option<Value>,
        create_lock: bool,
        take_lock: impl FnOnce(&File) -> Result<bool>,
    ) -> Result<Option<Self>> {
        ensure!(valid_id(id), "invalid dispatch ID");
        native_state(state)?;
        let path = state.join("dispatches").join(format!("{id}.json"));
        let lock_path = path.with_extension("herdr-lock");
        let lock = if create_lock {
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(lock_path)?
        } else {
            File::open(lock_path).context("missing native Herdr assignment lock")?
        };
        if !take_lock(&lock)? {
            return Ok(None);
        }
        let published = new.is_none();
        let row = match new {
            Some(row) => {
                ensure!(!path.try_exists()?, "dispatch already published");
                row
            }
            None => read_record(&path)?,
        };
        ensure!(
            row["backend"] == "herdr"
                && row["round"] == 1
                && row["attempt"]
                    .as_str()
                    .is_some_and(|a| valid_identity(a, "a-")),
            "current Herdr R1 assignment required"
        );
        ensure!(
            row["controllerOwner"].is_null(),
            "controller owner authorization required"
        );
        Ok(Some(Self {
            _lock: lock,
            state,
            path,
            row,
            published,
        }))
    }

    fn bound_record(&self) -> Result<&Value> {
        let h = &self.row["backendHandle"];
        ensure!(
            h["dispatch"] == self.row["dispatch"]
                && h["round"] == self.row["round"]
                && h["attempt"] == self.row["attempt"]
                && h["endpoint"] == self.row["endpoint"]
                && h["receipt"] == self.row["create"]["receipt"],
            "Herdr execution identity mismatch"
        );
        Ok(h)
    }

    fn handle(&self) -> Result<Handle> {
        Handle::deserialize(self.bound_record()?).context("incomplete native Herdr binding")
    }

    fn current(&self) -> Result<()> {
        native_state(self.state)?;
        ensure!(
            read_record(&self.path)? == self.row,
            "Herdr assignment changed during operation"
        );
        Ok(())
    }

    fn save(&mut self, next: Value) -> Result<()> {
        if !self.published {
            ensure!(
                !self.path.try_exists()?,
                "Herdr assignment already published"
            );
        } else {
            self.current()?;
        }
        write_json(&self.path, &next, None)?;
        File::open(self.path.parent().unwrap())?.sync_all()?;
        self.row = next;
        self.published = true;
        Ok(())
    }

    fn binding(&self, handle: &Handle) -> Value {
        let mut h = json!(handle);
        h["dispatch"] = self.row["dispatch"].clone();
        h["round"] = self.row["round"].clone();
        h["state"] = json!(if handle.stopped_evidence.is_some() {
            "not-started"
        } else if !handle.writers.is_empty() {
            "active"
        } else {
            "pending"
        });
        h["receipt"] = self.row["create"]["receipt"].clone();
        h
    }

    fn checkpoint(&mut self, h: &Handle) -> io::Result<()> {
        let mut next = self.row.clone();
        next["backendHandle"] = self.binding(h);
        self.save(next).map_err(io::Error::other)
    }

    // A started receipt identifies the actual child even when it is absent
    // from Herdr's foreground list. Never put it in the root-writer slot.
    fn launched_writer(&self) -> Result<Option<Process>> {
        if self.row["create"]["receipt"].is_null() {
            return Ok(None); // a reconciled replacement has no original launch receipt
        }
        let evidence = launch_receipt(self.state, &self.row["create"])?;
        match evidence["category"].as_str() {
            Some("started") => {
                let writer: Process = serde_json::from_value(evidence["writerProcess"].clone())
                    .context("unconfirmed launched worker identity")?;
                ensure!(evidence["pid"] == writer.pid, "launch receipt PID mismatch");
                Ok(Some(writer))
            }
            Some(
                "process_exited" | "executable_missing" | "permission_denied" | "spawn_failed",
            ) => Ok(None),
            _ => anyhow::bail!("worker startup is unconfirmed"),
        }
    }

    fn tracked_handle(&mut self, read_only: bool) -> Result<Handle> {
        let mut h = self.handle()?;
        if let Some(writer) = self.launched_writer()? {
            if !h.writers.is_empty() && !h.writers.contains(&writer) {
                h.writers.push(writer);
                if !read_only {
                    self.checkpoint(&h)?;
                }
            }
        }
        Ok(h)
    }

    fn inspect(&mut self, read_only: bool) -> Result<Value> {
        let mut h = self.tracked_handle(read_only)?;
        let observed = if read_only {
            h.resource_present()
        } else {
            h.inspect(|h| self.checkpoint(h))
        };
        Ok(match observed {
            Ok(alive) => json!({"result":"accepted","alive":alive,"owned":true}),
            Err(e) => json!(e),
        })
    }
}

enum InputAction<'a> {
    Submit(&'a str),
    Reply { text: &'a str, message_id: &'a str },
    ActivatePrefill(&'a str),
}

impl Assignment<'_> {
    fn clear_pending_input(&mut self) -> Result<()> {
        let mut next = self.row.clone();
        next["pendingInput"] = json!(false);
        next.as_object_mut()
            .unwrap()
            .remove("pendingReplyMessageId");
        self.save(next)
    }

    fn input(
        &mut self,
        action: InputAction<'_>,
        begin: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        let reply_id = match &action {
            InputAction::Reply { message_id, .. } => Some(*message_id),
            _ => None,
        };
        let released = match (reply_id, self.row["pendingReplyMessageId"].as_str()) {
            (Some(_), Some(previous)) if self.row["pendingInput"] == true => {
                super::messages::reply_was_not_sent(self.state, &self.row, previous)?
            }
            _ => false,
        };
        ensure!(
            self.row["pendingInput"] == false || released,
            "input still pending"
        );
        let mut h = self.tracked_handle(false)?;
        ensure!(h.stopped_evidence.is_none(), "Herdr writer stopped");
        let mut next = self.row.clone();
        next["pendingInput"] = json!(true);
        if let Some(message_id) = reply_id {
            next["pendingReplyMessageId"] = json!(message_id);
        }
        begin()?;
        // Replace a released checkpoint only after revalidation. Save
        // compares the current dispatch record under the assignment lock.
        if let Err(error) = self.save(next) {
            if reply_id.is_some() {
                return Ok(json!({"result":"definitely-not-sent",
                    "diagnostic":format!("could not persist Herdr pending input before submission: {error:#}")}));
            }
            return Err(error);
        }
        let result = match action {
            InputAction::ActivatePrefill(text) => {
                h.activate_opencode_prompt(text, |h| self.checkpoint(h))
            }
            InputAction::Submit(text) => {
                h.submit(text, self.row["host"] == "codex", |h| self.checkpoint(h))
            }
            InputAction::Reply { text, .. } => {
                let host = self.row["host"].as_str().unwrap_or_default().to_owned();
                h.submit_reply(text, &host, |h| self.checkpoint(h))
            }
        };
        if !matches!(result,Err(e) if e.kind == FailureKind::Uncertain) {
            if let Err(error) = self.clear_pending_input() {
                if let (Some(_), Err(failure)) = (reply_id, result) {
                    return Ok(json!({"result":failure.kind,
                        "diagnostic":format!("{}; could not clear Herdr pending input: {error:#}", failure.diagnostic)}));
                }
                return Err(error);
            }
        }
        Ok(match result {
            Ok(()) => json!({"result":"accepted"}),
            Err(e) => json!(e),
        })
    }
}

pub(super) fn create(state: &Path, row: &Value, launch: host::Launch) -> Result<Value> {
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    let unit = Assignment::lock(state, id, Some(row.clone()))?;
    let (unit, answer) = launch_assignment(unit, launch, None)?;
    ensure!(
        answer["result"] == "accepted",
        "backend create {} for {id}: {}",
        answer["result"],
        answer["diagnostic"]
    );
    Ok(unit.row)
}

pub(super) fn handoff_target(state: &Path, dispatch: &str, endpoint: &Path) -> Result<Process> {
    native_state(state)?;
    let lock = state
        .join("dispatches")
        .join(format!("{dispatch}.herdr-lock"));
    ensure!(
        std::fs::symlink_metadata(lock).is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "Herdr assignment lock is already present or inaccessible"
    );
    Handle::probe_service(endpoint).context("cannot verify the target Herdr service")
}

pub(super) fn create_for_handoff(
    state: &Path,
    row: &Value,
    audit: usize,
    launch: host::Launch,
    service: &Process,
) -> Result<Value> {
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    super::handoff::require_herdr_launch(state, row, audit, service)?;
    let unit = Assignment::lock_with(state, id, None, true, |file| {
        file.lock()?;
        Ok(true)
    })?
    .context("missing Herdr handoff assignment")?;
    ensure!(
        unit.row == *row,
        "Herdr handoff assignment changed before launch"
    );
    let (mut unit, answer) = launch_assignment(unit, launch, Some(service))?;
    let _inbox = super::messages::lock_for_publish(state)?;
    let mut next = unit.row.clone();
    super::handoff::settle_launch(&mut next, audit - 1, answer);
    unit.save(next)?;
    Ok(unit.row)
}

fn launch_assignment<'a>(
    mut unit: Assignment<'a>,
    launch: host::Launch,
    service: Option<&Process>,
) -> Result<(Assignment<'a>, Value)> {
    let state = unit.state;
    let row = &unit.row;
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    let cwd = Path::new(row["cwd"].as_str().context("missing cwd")?);
    let endpoint = PathBuf::from(row["endpoint"].as_str().context("missing endpoint")?);
    let attempt = row["attempt"]
        .as_str()
        .context("missing attempt")?
        .to_owned();
    let prefill = match launch.argv.as_slice() {
        [.., flag, brief] if row["host"] == "opencode" && flag == "--prompt" => Some(brief.clone()),
        _ => None,
    };
    let files = host_launch::prepare(
        state,
        id,
        cwd,
        launch.argv,
        serde_json::from_value(Value::Object(launch.env))?,
    )?;
    let launch = Launch {
        cwd: cwd.into(),
        label: row["label"].as_str().context("missing label")?.into(),
        command: vec![
            std::env::current_exe()?
                .to_str()
                .context("non-UTF8 launcher")?
                .into(),
            "--herdr-host".into(),
            state.to_str().context("non-UTF8 state")?.into(),
            files.id.clone(),
        ],
    };
    let mut prepared = unit.row.clone();
    prepared["executionId"] = json!(attempt);
    prepared["create"] = json!({"result":"uncertain","receipt":files.receipt});
    for key in ["dispatch", "round", "attempt"] {
        prepared["create"][key] = prepared[key].clone();
    }
    prepared["pendingInput"] = json!(false);
    if unit.published {
        unit.save(prepared)?;
    } else {
        prepared["audit"] = json!([]);
        unit.row = prepared;
    }
    // The provider checkpoints the nonce and service before its first external effect.
    let created = Handle::create_with_service(&endpoint, service, &attempt, &launch, |h| {
        unit.checkpoint(h)
    });
    let mut answer = match created {
        Ok(_) => {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let evidence = host_launch::read_receipt(&files.receipt).unwrap_or(Value::Null);
                if evidence["category"] == "started" {
                    break match unit.tracked_handle(false) {
                        Ok(_) => match &prefill {
                            Some(text) => {
                                unit.input(InputAction::ActivatePrefill(text), || Ok(()))?
                            }
                            None => json!({"result":"accepted"}),
                        },
                        Err(e) => json!({"result":"uncertain","diagnostic":e.to_string()}),
                    };
                }
                if evidence["category"] != "unknown" || Instant::now() >= deadline {
                    break json!({"result":"uncertain","category":evidence["category"],"diagnostic":"worker start not confirmed"});
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Err(e) => json!(e),
    };
    answer["receipt"] = json!(files.receipt);
    for key in ["dispatch", "round", "attempt"] {
        answer[key] = unit.row[key].clone();
    }
    if answer["result"] == "accepted" {
        answer["herdrBinding"] = launch_binding(&unit.row["backendHandle"]);
    }
    if !unit.row["backendHandle"].is_null() {
        let mut next = unit.row.clone();
        next["create"] = answer.clone();
        unit.save(next)?;
        if answer["result"] != "accepted" {
            if let Some(terminal) = host_launch::read_receipt(&files.receipt) {
                super::startup::unconfirmed_zero(state, &unit.row, &files.id, &terminal);
            }
        }
    } else if !unit.path.try_exists()? {
        // No checkpoint means the provider never issued workspace.create.
        std::fs::remove_file(&files.receipt)?;
        std::fs::remove_file(unit.path.with_extension("herdr-lock"))?;
    }
    Ok((unit, answer))
}

pub(super) fn observe(state: &Path, row: &mut Value, read_only: bool) -> Result<(Value, Value)> {
    let observed = (|| -> Result<_> {
        let id = row["dispatch"].as_str().context("missing dispatch")?;
        let mut unit = Assignment::lock(state, id, None)?;
        let bound = unit.bound_record().is_ok();
        let mut close_pending = unit.pending_close();
        let mut invalid_close_worker = false;
        let observed = (|| -> Result<_> {
            let handle = unit.handle().inspect_err(|_| {
                close_pending |= unit.completed_close().is_some();
            })?;
            let archive = unit
                .verified_archive(&handle)
                .inspect_err(|_| close_pending = true)?;
            let stopped = handle
                .verified_stopped()
                .inspect_err(|_| close_pending = true)?;
            if let Some(event) = backend::pending_herdr_close(&unit.row) {
                backend::verify_herdr_close_target(&unit.row, event)?;
                invalid_close_worker = !valid_close_worker(&event["workerOnClose"]);
            }
            let inspection = if archive.is_some() {
                json!({"result":"completed","alive":false})
            } else if stopped {
                json!({"result":"accepted","alive":false,"owned":true})
            } else {
                unit.inspect(read_only)?
            };
            Ok((archive, inspection))
        })();
        // Preserve the locked evidence before reading the independent launch receipt.
        *row = unit.row;
        row["closePending"] = json!(close_pending);
        row["launch"] = match launch_receipt(state, &row["create"]) {
            Ok(launch) => launch,
            Err(error) => {
                row["launchDiagnostic"] = json!(format!("{error:#}"));
                Value::Null
            }
        };
        let (archive, inspection) = observed.unwrap_or_else(|error| {
            (
                None,
                json!({"result":"uncertain","diagnostic":format!("{error:#}")}),
            )
        });
        invalid_close_worker |= archive.as_ref().is_some_and(|w| !valid_close_worker(w));
        if invalid_close_worker {
            row["closeWorkerDiagnostic"] = json!(INVALID_CLOSE_WORKER);
        }
        let worker = archive.filter(valid_close_worker).unwrap_or_else(|| {
            if bound {
                backend::herdr_worker(&row["launch"], &inspection)
            } else {
                json!({"state":"unknown"})
            }
        });
        Ok((worker, inspection))
    })();
    Ok(observed.unwrap_or_else(|e| {
        (
            json!({"state":"unknown"}),
            json!({"result":"uncertain","diagnostic":e.to_string()}),
        )
    }))
}

impl Assignment<'_> {
    fn pending_close(&self) -> bool {
        backend::pending_herdr_close(&self.row).is_some()
    }

    fn completed_close(&self) -> Option<&Value> {
        backend::herdr_closes(&self.row)
            .next_back()
            .filter(|event| event["result"] == "completed")
    }

    fn verified_archive(&self, handle: &Handle) -> Result<Option<Value>> {
        if let Some(event) = self.completed_close() {
            backend::verify_herdr_close_target(&self.row, event)?;
            return Ok(Some(event["workerOnClose"].clone()));
        }
        let archive = backend::archive(self.state, &self.row)?.map(|saved| saved["worker"].clone());
        if archive.is_some() {
            ensure!(
                handle
                    .verified_stopped()
                    .context("legacy archive stop evidence")?,
                "close intent target changed: legacy archive has no Herdr writer identity"
            );
        }
        Ok(archive)
    }

    fn close_receipt(&self, outcome: Value) -> Value {
        let mut receipt = outcome;
        for key in ["dispatch", "round", "attempt"] {
            receipt[key] = self.row[key].clone();
        }
        receipt["operation"] = json!("close");
        if self.row["create"]["herdrBinding"].is_object() {
            receipt["executionId"] = self.row["executionId"].clone();
            if let Some(attempt) = self.row["attempt"].as_str() {
                receipt["closeOperationId"] = json!(format!("close:{attempt}"));
            }
        }
        receipt
    }

    fn replay_close(&self, handle: &Handle) -> Result<Option<Value>> {
        let Some(worker) = self.verified_archive(handle)? else {
            return Ok(None);
        };
        let mut outcome = json!({"result":"completed","diagnostic":"session was already gone"});
        if !valid_close_worker(&worker) {
            outcome["closeWorkerDiagnostic"] = json!(INVALID_CLOSE_WORKER);
        }
        Ok(Some(self.close_receipt(outcome)))
    }

    fn finish_close(&mut self, mut outcome: Value) -> Result<Value> {
        let mut next = self.row.clone();
        let event = next["audit"]
            .as_array_mut()
            .and_then(|a| a.last_mut())
            .context("missing close intent")?;
        ensure!(
            event["operation"] == "close" && event["attempt"] == self.row["attempt"],
            "close intent changed"
        );
        if outcome["result"] == "completed" && !valid_close_worker(&event["workerOnClose"]) {
            outcome["closeWorkerDiagnostic"] = json!(INVALID_CLOSE_WORKER);
        }
        event["result"] = outcome["result"].clone();
        event["diagnostic"] = outcome["diagnostic"].clone();
        self.save(next)?;
        Ok(self.close_receipt(outcome))
    }
}

fn lock_close<'a>(state: &'a Path, row: &Value) -> Result<Assignment<'a>> {
    let unit = Assignment::lock(
        state,
        row["dispatch"].as_str().context("missing dispatch")?,
        None,
    )?;
    ensure!(
        unit.row["attempt"] == row["attempt"] && record_scope(&unit.row) == record_scope(row),
        "Herdr attempt or scope changed"
    );
    Ok(unit)
}

pub(super) fn lock_handoff_source(state: &Path, row: &Value) -> Result<File> {
    let unit = lock_close(state, row)?;
    ensure!(
        unit.row == *row,
        "Herdr assignment changed while preparing handoff"
    );
    Ok(unit._lock)
}

pub(super) fn read_only_close(state: &Path, row: &Value) -> Result<Option<Value>> {
    let unit = lock_close(state, row)?;
    unit.replay_close(&unit.handle()?)
}

pub(super) fn close(state: &Path, row: &Value) -> Result<Value> {
    let mut unit = lock_close(state, row)?;
    let mut handle = unit.handle()?;
    if let Some(receipt) = unit.replay_close(&handle)? {
        return Ok(receipt);
    }
    let stopped = match handle.verified_stopped() {
        Ok(stopped) => stopped,
        Err(error) => return Ok(unit.close_receipt(json!(error))),
    };
    if let Some(event) = backend::pending_herdr_close(&unit.row) {
        backend::verify_herdr_close_target(&unit.row, event)?;
        let outcome = match handle.confirm_closed(|h| unit.checkpoint(h)) {
            Ok(true) => return unit.finish_close(json!({"result":"completed"})),
            Ok(false) => {
                json!({"result":"uncertain","diagnostic":"pending close: pane or known writer still present"})
            }
            Err(e) => json!({"result":"uncertain","diagnostic":e.diagnostic}),
        };
        return Ok(unit.close_receipt(outcome));
    }
    if unit.row["create"]["result"] != "accepted" && !stopped {
        return Ok(unit.close_receipt(json!({"result":"uncertain","diagnostic":"unresolved Herdr create requires lifecycle resolution"})));
    }
    let prepared = if stopped {
        None
    } else {
        handle = match unit.tracked_handle(false) {
            Ok(handle) => handle,
            Err(error) => {
                return Ok(unit
                    .close_receipt(json!({"result":"uncertain","diagnostic":error.to_string()})));
            }
        };
        match handle.prepare_close(|h| unit.checkpoint(h)) {
            Ok(close) => Some(close),
            Err(e) => return Ok(unit.close_receipt(json!(e))),
        }
    };
    let mut next = unit.row.clone();
    let receipt =
        unit.close_receipt(json!({"result": if prepared.is_some() {"pending"} else {"completed"}}));
    let mut event = receipt.clone();
    event["at"] = json!(chrono::Utc::now().to_rfc3339());
    event["target"] = unit.row["backendHandle"].clone();
    let audit = next["audit"]
        .as_array_mut()
        .context("invalid lifecycle audit")?;
    event["id"] = json!(audit.len() + 1);
    {
        // done validates this assignment and publishes its receipt under the same lock.
        // Keep backend RPCs outside this short commit section.
        let _messages = super::messages::lock_for_publish(state)?;
        let launch = launch_receipt(state, &unit.row["create"])?;
        let worker = if launch["category"] == "process_exited" {
            backend::herdr_worker(&launch, &Value::Null)
        } else {
            json!({"state":"cancelled","exit_code":null,"signal":null})
        };
        event["workerOnClose"] = worker;
        audit.push(event);
        unit.save(next)?;
    }
    let Some(close) = prepared else {
        // Explicit stopped evidence already confirms this unit, even offline.
        let mut receipt = receipt;
        receipt["diagnostic"] = json!("session was already gone");
        return Ok(receipt);
    };
    let has_pane = close.has_pane();
    let outcome = match close.finish() {
        Ok(()) if !has_pane => {
            json!({"result":"completed","diagnostic":"session was already gone"})
        }
        Ok(()) => json!({"result":"completed"}),
        Err(e) => json!(e),
    };
    unit.finish_close(outcome)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    op: String,
    dispatch: String,
    round: u64,
    attempt: String,
    text: String,
}

pub fn submit(input: Input) -> Result<Value> {
    ensure!(input.op == "submit", "unsupported Herdr operation");
    controller::require_controller("submit")?;
    let state = state_dir()?;
    let (mut unit, _scope) = Assignment::lock_unowned(&state, &input.dispatch, "submit")?;
    writable_input(&unit, input.round, &input.attempt)?;
    let mut response = unit.input(InputAction::Submit(&input.text), || Ok(()))?;
    response["dispatch"] = json!(input.dispatch);
    response["round"] = json!(input.round);
    response["attempt"] = json!(input.attempt);
    Ok(response)
}

fn writable_input(unit: &Assignment<'_>, round: u64, attempt: &str) -> Result<()> {
    ensure!(
        unit.row["round"] == round && unit.row["attempt"] == attempt,
        "stale Herdr input assignment"
    );
    ensure!(
        unit.verified_archive(&unit.handle()?)?.is_none(),
        "Herdr dispatch closed"
    );
    ensure!(!unit.pending_close(), "Herdr close still pending");
    ensure!(
        super::assignment::unresolved_handoff(&unit.row).is_none(),
        "Herdr handoff still pending"
    );
    ensure!(
        unit.row["create"]["result"] == "accepted",
        "Herdr launch is not active"
    );
    Ok(())
}

// The caller holds the scope lock and must release it when the assignment
// lock is busy so close and takeover can proceed in the same lock order.
pub(super) fn try_submit_reply(
    state: &Path,
    row: &Value,
    message_id: &str,
    text: &str,
    begin: impl FnOnce() -> Result<()>,
) -> Result<Option<Value>> {
    let Some(mut unit) = Assignment::lock_with(
        state,
        row["dispatch"].as_str().context("missing dispatch")?,
        None,
        false,
        super::assignment::try_input_lock,
    )?
    else {
        return Ok(None);
    };
    writable_input(
        &unit,
        row["round"].as_u64().context("missing round")?,
        row["attempt"].as_str().context("missing attempt")?,
    )?;
    // The Herdr assignment lock covers the recheck and the existing input
    // checkpoint through the RPC result, just as it does for a private submit.
    unit.input(InputAction::Reply { text, message_id }, begin)
        .map(Some)
}

pub struct Reconcile {
    dispatch: String,
    mapping: Mapping,
    owner_token: bool,
}

impl Reconcile {
    pub fn parse(args: &[OsString]) -> Result<Self> {
        let (id, args) = args.split_first().context("missing dispatch")?;
        let dispatch = id
            .to_str()
            .filter(|id| valid_id(id))
            .context("invalid dispatch ID")?
            .into();
        let mut options = BTreeMap::new();
        let mut args = args.iter();
        while let Some(key) = args.next() {
            let key = key.to_str().context("invalid option")?;
            ensure!(
                matches!(
                    key,
                    "--pane"
                        | "--terminal"
                        | "--actor"
                        | "--evidence"
                        | "--workspace"
                        | "--old-writer-evidence"
                        | "--owner-token"
                ),
                "unknown Herdr option"
            );
            let value = args
                .next()
                .and_then(|v| v.to_str())
                .filter(|v| {
                    !v.starts_with("--")
                        && (matches!(key, "--actor" | "--evidence") || !v.trim().is_empty())
                })
                .context("missing option value")?;
            ensure!(
                options.insert(key, value.to_owned()).is_none(),
                "duplicate {key}"
            );
        }
        Ok(Self {
            dispatch,
            owner_token: options.contains_key("--owner-token"),
            mapping: Mapping {
                pane: options.remove("--pane").context("missing --pane")?,
                terminal: options.remove("--terminal").context("missing --terminal")?,
                actor: options.remove("--actor").context("missing --actor")?,
                evidence: options.remove("--evidence").context("missing --evidence")?,
                workspace: options.remove("--workspace"),
                old_writer_evidence: options.remove("--old-writer-evidence"),
            },
        })
    }

    pub fn run(self) -> Result<Value> {
        controller::require_controller("herdr-reconcile")?;
        ensure!(
            !self.mapping.actor.trim().is_empty() && !self.mapping.evidence.trim().is_empty(),
            "herdr-reconcile requires nonempty actor and evidence"
        );
        ensure!(
            !self.owner_token,
            "controller owner authorization is unavailable"
        );
        let state = state_dir()?;
        let (mut unit, _scope) =
            Assignment::lock_unowned(&state, &self.dispatch, "herdr-reconcile")?;
        ensure!(
            super::assignment::unresolved_handoff(&unit.row).is_none(),
            "Herdr handoff still pending"
        );
        ensure!(
            unit.verified_archive(&unit.handle()?)?.is_none(),
            "Herdr dispatch closed"
        );
        ensure!(!unit.pending_close(), "Herdr close still pending");
        let receipt = launch_receipt(&state, &unit.row["create"])?;
        let missing_launch = !unit.row["create"]["receipt"].is_null()
            && (receipt.is_null() || receipt["category"] == "unknown");
        let mut h = if missing_launch {
            ensure!(
                self.mapping
                    .old_writer_evidence
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty()),
                "unconfirmed launch requires old writer evidence"
            );
            unit.handle()?
        } else {
            unit.tracked_handle(false)?
        };
        let binding = h.reconcile(&self.mapping, |h| unit.checkpoint(h))?;
        let mut next = unit.row.clone();
        // An interrupted create may have a receipt but no verified pane root.
        if h.writers.is_empty() && !missing_launch {
            if let Some(child) = unit.launched_writer()? {
                ensure!(
                    binding.handle.writers.contains(&child) || !child.alive()?,
                    "old launched writer still alive outside proposed binding"
                );
            }
        }
        next["backendHandle"] = unit.binding(&binding.handle);
        if missing_launch || !binding.same_live_unit {
            next["backendHandle"]["receipt"] = Value::Null;
            next["create"]["receipt"] = Value::Null;
        }
        let round = next["round"].clone();
        let attempt = next["attempt"].clone();
        let audit = next["audit"]
            .as_array_mut()
            .context("invalid lifecycle audit")?;
        let event = audit.len() + 1;
        let at = chrono::Utc::now().to_rfc3339();
        let mut evidence = binding.audit;
        evidence["mapping"] = json!(self.mapping.evidence);
        evidence["oldWriter"] = json!(self.mapping.old_writer_evidence);
        audit.push(json!({"id":event,"dispatch":self.dispatch,"round":round,"attempt":attempt,
            "operation":"backend_reconcile","result":"accepted","at":at, "ownerGeneration":null,
            "fromAttempt":null,"toAttempt":null,"fromHost":null,"toHost":null,
            "diagnostic":"explicit Herdr unit binding",
            "resolution":{"actor":self.mapping.actor,"evidence":evidence.to_string(),"at":at,"ownerGeneration":null}}));
        unit.save(next)?;
        Ok(
            json!({"dispatch":self.dispatch,"round":round,"attempt":attempt,"result":"accepted","audit":event,"inputReplayed":false}),
        )
    }
}
