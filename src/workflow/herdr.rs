//! Herdr assignments. A stable lock covers inspection, input claims, close
//! and reconciliation while atomic replacements publish complete record generations.
use super::{
    backend, controller, ensure_no_legacy_db, ensure_state_writable, host, read_record,
    record_scope, valid_id, valid_identity, writable_state_dir, write_json,
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
use wfc_herdr::{CodexInput, FailureKind, Handle, InputBoundary, Launch, Mapping, Process};

pub const USAGE: &str = "wfc herdr-reconcile DISPATCH --pane P --terminal T --actor NAME --evidence TEXT [--workspace W] [--old-writer-evidence TEXT] [--owner-token TOKEN]";
pub(super) const APPENDED_USER_ENTRY: &str = "herdr-appended-user-entry";

fn launch_receipt(state: &Path, create: &Value) -> Result<Value> {
    let launch = backend::herdr_receipt(state, create)?;
    ensure!(
        launch["category"] != "process_exited" || launch["exitCode"].is_i64(),
        "invalid host launch exit code"
    );
    Ok(launch)
}

fn receipt_writer(evidence: &Value) -> Result<Process> {
    let writer: Process = serde_json::from_value(evidence["writerProcess"].clone())
        .context("unconfirmed launched worker identity")?;
    ensure!(evidence["pid"] == writer.pid, "launch receipt PID mismatch");
    Ok(writer)
}

enum LaunchConfirmation {
    Pending,
    Started(Process),
    Terminal(Value),
}

fn confirm_launch(
    first: Value,
    recheck: impl FnOnce() -> Result<Value>,
) -> Result<LaunchConfirmation> {
    // The supervisor can replace the started receipt before we confirm it.
    let evidence = if first["category"] == "started" {
        recheck()?
    } else {
        first
    };
    Ok(match evidence["category"].as_str() {
        Some("unknown") => LaunchConfirmation::Pending,
        Some("started") => LaunchConfirmation::Started(receipt_writer(&evidence)?),
        Some("process_exited" | "executable_missing" | "permission_denied" | "spawn_failed") => {
            LaunchConfirmation::Terminal(evidence)
        }
        _ => anyhow::bail!("worker startup is unconfirmed"),
    })
}

fn launch_binding(handle: &Value) -> Value {
    json!({"endpoint":handle["endpoint"],"service":handle["service"],
        "workspace":handle["workspace"],"pane":handle["pane"],"terminal":handle["terminal"],
        "nonce":handle["nonce"],"receipt":handle["receipt"]})
}

pub(super) fn verify_handoff_source(state: &Path, row: &Value) -> Result<()> {
    let receipt = launch_receipt(state, &row["create"])?;
    verify_tree_launch(state, row, &receipt)
}

fn verify_tree_launch(state: &Path, row: &Value, receipt: &Value) -> Result<()> {
    let launch = backend::receipt_id(row)?.context("missing original Herdr launch")?;
    let binding = &row["create"]["herdrBinding"];
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

fn verify_offline_tree_stop(state: &Path, row: &Value, receipt: &Value) -> Result<()> {
    // Older Herdr launches have neither supervision marker. They still need
    // the existing exit, pane, service and recorded-writer checks.
    if receipt.get("treeTracked").is_none() && row["create"]["herdrBinding"].is_null() {
        return Ok(());
    }
    verify_tree_launch(state, row, receipt)?;
    ensure!(
        handoff_tree_stopped(state, row)?
            .is_some_and(|stopped| stopped["exitCode"] == receipt["exitCode"]),
        "original Herdr worker tree has no matching stop receipt"
    );
    Ok(())
}

const INVALID_CLOSE_WORKER: &str = "invalid closed worker outcome";

fn valid_close_worker(worker: &Value) -> bool {
    worker["state"] == "cancelled" || (worker["state"] == "exited" && worker["exit_code"].is_i64())
}

// Herdr's ownership tokens are attached to the original physical launch.
// Later logical attempts keep this binding and the same receipt intact.
pub(super) fn bound_record(row: &Value) -> Result<&Value> {
    let h = &row["backendHandle"];
    let original = row["round"] == 1 && h["attempt"] == row["attempt"];
    let continued = row["round"].as_u64().is_some_and(|round| round > 1)
        && h["attempt"]
            .as_str()
            .is_some_and(|id| valid_identity(id, "a-"))
        && h["attempt"] == row["executionId"]
        && h["receipt"].is_string()
        && row["create"]["result"] == "accepted"
        && row["rounds"].as_array().into_iter().flatten().any(|round| {
            round["round"] == 1 && round["attempt"] == h["attempt"] && round["state"] == "completed"
        })
        && row["audit"].as_array().into_iter().flatten().any(|event| {
            event["operation"] == "continue"
                && event["round"] == row["round"]
                && event["attempt"] == row["attempt"]
                && matches!(
                    event["result"].as_str(),
                    Some("pending" | "accepted" | "uncertain")
                )
        });
    ensure!(
        (original || continued)
            && h["round"] == 1
            && h["dispatch"] == row["dispatch"]
            && h["endpoint"] == row["endpoint"]
            && h["receipt"] == row["create"]["receipt"],
        "Herdr execution identity mismatch"
    );
    Ok(h)
}

pub(super) fn same_binding(before: &Value, after: &Value) -> bool {
    let (Ok(before), Ok(after)) = (bound_record(before), bound_record(after)) else {
        return false;
    };
    [
        "attempt",
        "endpoint",
        "receipt",
        "service",
        "workspace",
        "pane",
        "terminal",
        "nonce",
    ]
    .iter()
    .all(|&key| before[key] == after[key])
        && before["writerProcesses"].get(0) == after["writerProcesses"].get(0)
}

pub(super) fn unsettled_reply(row: &Value, history: &[Value]) -> bool {
    history.iter().any(|message| {
        message["type"] == "reply"
            && message["round"] == row["round"]
            && message["attempt"] == row["attempt"]
            && matches!(
                message["inputResult"].as_str(),
                Some("pending" | "uncertain")
            )
    })
}

fn open_assignment_lock(path: &Path) -> Result<File> {
    File::open(path.with_extension("herdr-lock")).context("missing native Herdr assignment lock")
}

pub(super) fn try_input_lock(state: &Path, row: &Value) -> Result<Option<File>> {
    ensure_state_writable(state)?;
    let id = row["dispatch"]
        .as_str()
        .filter(|id| valid_id(id))
        .context("invalid dispatch ID")?;
    let path = state.join("dispatches").join(format!("{id}.json"));
    let lock = open_assignment_lock(&path)?;
    if !super::assignment::try_input_lock(&lock)? {
        return Ok(None);
    }
    ensure_state_writable(state)?;
    Ok(Some(lock))
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
        ensure_no_legacy_db(state)?;
        let row = read_record(&state.join("dispatches").join(format!("{id}.json")))?;
        let scope = record_scope(&row).context("invalid dispatch scope")?;
        let authorize = || controller::Owners::read(state)?.require(scope, None, operation);
        authorize()?;
        let scope_lock = controller::lock_scope(state, scope)?;
        authorize()?;
        let unit = Self::lock(state, id, None, false)?;
        ensure!(
            record_scope(&unit.row) == Some(scope),
            "Herdr scope changed"
        );
        Ok((unit, scope_lock))
    }

    fn lock(state: &'a Path, id: &str, new: Option<Value>, read_only: bool) -> Result<Self> {
        let create_lock = new.is_some();
        Self::lock_with(state, id, new, read_only, create_lock, |file| {
            file.lock()?;
            Ok(true)
        })?
        .context("missing locked Herdr assignment")
    }

    fn lock_with(
        state: &'a Path,
        id: &str,
        new: Option<Value>,
        read_only: bool,
        create_lock: bool,
        take_lock: impl FnOnce(&File) -> Result<bool>,
    ) -> Result<Option<Self>> {
        ensure!(valid_id(id), "invalid dispatch ID");
        if read_only {
            ensure_no_legacy_db(state)?;
        } else {
            ensure_state_writable(state)?;
        }
        let path = state.join("dispatches").join(format!("{id}.json"));
        let lock_path = path.with_extension("herdr-lock");
        let lock = if create_lock {
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(lock_path)?
        } else {
            open_assignment_lock(&path)?
        };
        if !take_lock(&lock)? {
            return Ok(None);
        }
        if !read_only {
            ensure_state_writable(state)?;
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
                && (row["round"] == 1 || bound_record(&row).is_ok())
                && row["attempt"]
                    .as_str()
                    .is_some_and(|a| valid_identity(a, "a-")),
            "current Herdr R1 or verified continued assignment required"
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
        bound_record(&self.row)
    }

    fn handle(&self) -> Result<Handle> {
        Handle::deserialize(self.bound_record()?).context("incomplete native Herdr binding")
    }

    fn current(&self) -> Result<()> {
        ensure_state_writable(self.state)?;
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
        h["round"] = json!(1);
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
            Some("started") => receipt_writer(&evidence).map(Some),
            Some(
                "process_exited" | "executable_missing" | "permission_denied" | "spawn_failed",
            ) => Ok(None),
            _ => anyhow::bail!("worker startup is unconfirmed"),
        }
    }

    fn remember_launched_writer(
        &mut self,
        h: &mut Handle,
        writer: Process,
        read_only: bool,
    ) -> Result<()> {
        if !h.writers.is_empty() && !h.writers.contains(&writer) {
            h.writers.push(writer);
            if !read_only {
                self.checkpoint(h)?;
            }
        }
        Ok(())
    }

    fn tracked_handle(&mut self, read_only: bool) -> Result<Handle> {
        let mut h = self.handle()?;
        if let Some(writer) = self.launched_writer()? {
            self.remember_launched_writer(&mut h, writer, read_only)?;
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
        confirmed_handle: Option<Handle>,
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
        let mut h = match confirmed_handle {
            Some(handle) => handle,
            None => self.tracked_handle(false)?,
        };
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
    let mut unit = Assignment::lock(state, id, Some(row.clone()), false)?;
    let answer = launch_into(&mut unit, launch, None)?;
    ensure!(
        answer["result"] == "accepted",
        "backend create {} for {id}: {}",
        answer["result"],
        answer["diagnostic"]
    );
    Ok(unit.row)
}

pub(super) fn handoff_target(state: &Path, dispatch: &str, endpoint: &Path) -> Result<Process> {
    ensure_state_writable(state)?;
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
    let mut unit = Assignment::lock_with(state, id, None, false, true, |file| {
        file.lock()?;
        Ok(true)
    })?
    .context("missing Herdr handoff assignment")?;
    ensure!(
        unit.row == *row,
        "Herdr handoff assignment changed before launch"
    );
    let answer = launch_into(&mut unit, launch, Some(service))?;
    let _inbox = super::messages::lock_for_publish(state)?;
    let mut next = unit.row.clone();
    super::handoff::settle_launch(&mut next, audit - 1, answer);
    unit.save(next)?;
    Ok(unit.row)
}

fn launch_into(
    unit: &mut Assignment<'_>,
    launch: host::Launch,
    service: Option<&Process>,
) -> Result<Value> {
    let initial = !unit.published;
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
    let mut files = host_launch::prepare(
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
    if initial {
        prepared["audit"] = json!([]);
        unit.row = prepared;
    } else {
        unit.save(prepared)?;
    }
    // The provider checkpoints the nonce and service before its first external effect.
    let created = Handle::create_with_service(
        &endpoint,
        service,
        &attempt,
        &launch,
        |h| unit.checkpoint(h),
        || files.hand_off(),
    );
    let (mut answer, terminal) = match created {
        Ok(_) => {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let first = host_launch::read_receipt(&files.receipt).unwrap_or(Value::Null);
                let recheck =
                    confirm_launch(first.clone(), || launch_receipt(state, &unit.row["create"]));
                let (confirmation, activation) = match recheck {
                    Ok(LaunchConfirmation::Started(writer)) => {
                        let handle = match unit.handle().and_then(|mut handle| {
                            unit.remember_launched_writer(&mut handle, writer, false)?;
                            Ok(handle)
                        }) {
                            Ok(handle) => handle,
                            Err(e) => {
                                break (
                                    json!({"result":"uncertain","diagnostic":e.to_string()}),
                                    None,
                                );
                            }
                        };
                        let Some(text) = &prefill else {
                            break (json!({"result":"accepted"}), None);
                        };
                        let activation =
                            unit.input(InputAction::ActivatePrefill(text), Some(handle), || Ok(()));
                        // Startup activation can outlive the confirmed child.
                        // Keep its terminal evidence even if activation returned an error.
                        (
                            confirm_launch(first, || launch_receipt(state, &unit.row["create"])),
                            Some(activation),
                        )
                    }
                    confirmation => (confirmation, None),
                };
                match confirmation {
                    Ok(LaunchConfirmation::Started(_)) => {
                        break (
                            activation.context("missing Herdr activation result")??,
                            None,
                        );
                    }
                    Ok(LaunchConfirmation::Terminal(evidence)) => {
                        let answer = json!({"result":"uncertain","category":evidence["category"],"diagnostic":"worker start not confirmed"});
                        break (answer, Some(evidence));
                    }
                    Ok(LaunchConfirmation::Pending)
                        if activation.is_none() && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(LaunchConfirmation::Pending) => {
                        break (
                            json!({"result":"uncertain","category":"unknown","diagnostic":"worker start not confirmed"}),
                            None,
                        );
                    }
                    Err(e) => {
                        break (
                            json!({"result":"uncertain","diagnostic":e.to_string()}),
                            None,
                        );
                    }
                }
            }
        }
        Err(e) => (json!(e), None),
    };
    answer["receipt"] = json!(files.receipt);
    for key in ["dispatch", "round", "attempt"] {
        answer[key] = unit.row[key].clone();
    }
    if answer["result"] == "accepted" {
        answer["herdrBinding"] = launch_binding(&unit.row["backendHandle"]);
    }
    if !unit.row["backendHandle"].is_null() {
        // Both replacement paths publish the result with their own audit.
        if initial {
            let mut next = unit.row.clone();
            next["create"] = answer.clone();
            unit.save(next)?;
        }
        if answer["result"] != "accepted" {
            if let Some(terminal) = terminal.or_else(|| host_launch::read_receipt(&files.receipt)) {
                super::startup::unconfirmed_zero(state, &unit.row, &files.id, &terminal);
            }
        }
    } else if initial && !unit.path.try_exists()? {
        // No checkpoint means the provider never issued workspace.create.
        std::fs::remove_file(&files.receipt)?;
        std::fs::remove_file(unit.path.with_extension("herdr-lock"))?;
    }
    Ok(answer)
}

pub(super) struct OfflineResume<'a> {
    unit: Assignment<'a>,
    service: Process,
}

impl<'a> OfflineResume<'a> {
    pub(super) fn verify(state: &'a Path, row: &Value) -> Result<(Self, Value, Value)> {
        let mut unit = lock_close(state, row, false)?;
        ensure!(unit.row == *row, "Herdr assignment changed before resume");
        ensure!(
            row["create"]["result"] == "accepted" && row["pendingInput"] == false,
            "original Herdr creation or input is unresolved"
        );
        let evidence = launch_receipt(state, &row["create"])?;
        ensure!(
            evidence["category"] == "process_exited"
                && evidence["exitCode"]
                    .as_u64()
                    .is_some_and(|code| code <= 255),
            "original Herdr worker has no confirmed exit receipt"
        );
        let mut handle = unit.handle()?;
        unit.verified_archive(&handle)?;
        ensure!(
            handle.stopped_evidence.is_none(),
            "expected the original Herdr binding"
        );
        let service = handle
            .service
            .clone()
            .context("missing original Herdr service")?;
        ensure!(
            handle.confirm_closed(|h| unit.checkpoint(h))?,
            "original Herdr pane or writers are still present"
        );
        verify_offline_tree_stop(state, row, &evidence)?;
        let inspection = json!({"result":"accepted","alive":false,"owned":true});
        let worker = backend::herdr_worker(&evidence, &inspection);
        Ok((Self { unit, service }, worker, inspection))
    }

    pub(super) fn launch(&mut self, next: &mut Value, launch: host::Launch) -> Result<Value> {
        // The caller wrote the new attempt under the Inbox lock. Keep the
        // stable Herdr assignment lock through the replacement checkpoints.
        self.unit.row = next.clone();
        self.unit.current()?;
        let answer = launch_into(&mut self.unit, launch, Some(&self.service))?;
        *next = self.unit.row.clone();
        Ok(answer)
    }
}

pub(super) fn observe(state: &Path, row: &mut Value, read_only: bool) -> Result<(Value, Value)> {
    let observed = (|| -> Result<_> {
        let id = row["dispatch"].as_str().context("missing dispatch")?;
        let mut unit = Assignment::lock(state, id, None, read_only)?;
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

fn lock_close<'a>(state: &'a Path, row: &Value, read_only: bool) -> Result<Assignment<'a>> {
    let unit = Assignment::lock(
        state,
        row["dispatch"].as_str().context("missing dispatch")?,
        None,
        read_only,
    )?;
    ensure!(
        unit.row["attempt"] == row["attempt"] && record_scope(&unit.row) == record_scope(row),
        "Herdr attempt or scope changed"
    );
    Ok(unit)
}

pub(super) fn lock_handoff_source(state: &Path, row: &Value) -> Result<File> {
    let unit = lock_close(state, row, false)?;
    ensure!(
        unit.row == *row,
        "Herdr assignment changed while preparing handoff"
    );
    Ok(unit._lock)
}

pub(super) fn read_only_close(state: &Path, row: &Value) -> Result<Option<Value>> {
    let unit = lock_close(state, row, true)?;
    unit.replay_close(&unit.handle()?)
}

pub(super) fn close(state: &Path, row: &Value) -> Result<Value> {
    let mut unit = lock_close(state, row, false)?;
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
    let state = writable_state_dir()?;
    let (mut unit, _scope) = Assignment::lock_unowned(&state, &input.dispatch, "submit")?;
    writable_input(&unit, input.round, &input.attempt)?;
    let mut response = unit.input(InputAction::Submit(&input.text), None, || Ok(()))?;
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
    ensure!(
        !super::assignment::input_unresolved(&unit.row),
        "Herdr input is pending or uncertain"
    );
    Ok(())
}

pub(super) struct InputTarget {
    handle: Handle,
    writer: Process,
}

impl InputTarget {
    // The caller holds the scope and the stable physical assignment lock.
    pub(super) fn verify(state: &Path, row: &Value) -> Result<Self> {
        ensure!(
            backend::herdr_closes(row)
                .next_back()
                .is_none_or(|event| !matches!(
                    event["result"].as_str(),
                    Some("pending" | "uncertain" | "completed")
                )),
            "Herdr execution resources are closing or closed"
        );
        ensure!(row["pendingInput"] == false, "Herdr input is still pending");
        let receipt = launch_receipt(state, &row["create"])?;
        ensure!(
            receipt["category"] == "started",
            "Herdr launch is not running"
        );
        let writer = receipt_writer(&receipt)?;
        let mut handle = Handle::deserialize(bound_record(row)?)?;
        ensure!(
            handle.inspect(|_| Ok(()))? && writer.alive()?,
            "Herdr worker is not running"
        );
        if !handle.writers.contains(&writer) {
            handle.writers.push(writer.clone());
        }
        Ok(Self { handle, writer })
    }
}

impl super::input::HerdrInputTarget for InputTarget {
    fn bind_continued_execution(&self, next: &mut Value) {
        next["executionId"] = json!(self.handle.attempt);
    }

    fn save_writer_evidence(&self, next: &mut Value) {
        next["backendHandle"]["writerProcesses"] = json!(self.handle.writers);
    }

    fn submit(
        &mut self,
        action: CodexInput,
        text: &str,
        mut check: impl FnMut(InputBoundary) -> io::Result<()>,
    ) -> Value {
        let writer = &self.writer;
        let mut late_writers = None;
        let result = self.handle.submit_codex_input(
            text,
            writer,
            action,
            // Writers observed before staging were persisted with the audit.
            // A new writer during preparation needs a fresh preflight.
            |observed| {
                late_writers = Some(observed.writers.clone());
                Err(io::Error::other("Herdr pane writers changed before input"))
            },
            |boundary| {
                if boundary == InputBoundary::Prepare
                    && !writer.alive().map_err(io::Error::other)?
                {
                    return Err(io::Error::other("original Herdr worker stopped"));
                }
                check(boundary)
            },
        );
        if let Some(writers) = late_writers {
            self.handle.writers = writers;
        }
        match result {
            Ok(()) => json!({"result":"accepted", "inputEvidence":APPENDED_USER_ENTRY}),
            Err(error) => json!(error),
        }
    }
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
    unit.input(InputAction::Reply { text, message_id }, None, begin)
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
        let state = writable_state_dir()?;
        let (mut unit, _scope) =
            Assignment::lock_unowned(&state, &self.dispatch, "herdr-reconcile")?;
        ensure!(
            super::assignment::unresolved_handoff(&unit.row).is_none(),
            "Herdr handoff still pending"
        );
        ensure!(
            unit.row["round"] == 1,
            "Herdr pane reconciliation requires the original task round"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn started_receipt() -> Value {
        json!({"category":"started","pid":42,
            "writerProcess":{"pid":42,"creation":"linux:fixture:125"}})
    }

    #[test]
    fn herdr_confirmation_keeps_terminal_receipt_and_one_notice() -> Result<()> {
        let state =
            std::env::temp_dir().join(format!("wfc-herdr-confirm-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let dispatch = "d-aabbccdd";
            let attempt = "a-123456789abc";
            let launch = "11111111111111111111111111111111";
            let receipt = state.join("launches").join(format!("{launch}.json"));
            let assignment = state.join("dispatches").join(format!("{dispatch}.json"));
            std::fs::create_dir_all(receipt.parent().unwrap())?;
            std::fs::create_dir_all(assignment.parent().unwrap())?;
            let row = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"role":"worker",
                "scope":"fixture-scope","cwd":state,"report":state.join("report"),"host":"codex","backend":"herdr",
                "create":{"result":"uncertain","receipt":receipt}});
            write_json(&assignment, &row, None)?;
            let original_assignment = std::fs::read(&assignment)?;
            write_json(&receipt, &started_receipt(), None)?;
            let first = host_launch::read_receipt(&receipt).context("missing fixture receipt")?;
            let terminal = json!({"category":"process_exited","exitCode":0,
                "startupZero":{"pid":42,"processStart":"linux:fixture:125"}});
            let confirmation = confirm_launch(first.clone(), || {
                write_json(&receipt, &terminal, None)?;
                launch_receipt(&state, &row["create"])
            })?;
            let LaunchConfirmation::Terminal(observed) = confirmation else {
                anyhow::bail!("a terminal receipt must not confirm startup");
            };
            assert_eq!(observed, terminal);
            super::super::startup::unconfirmed_zero(&state, &row, launch, &observed);
            let notices = super::super::messages::history(&state, dispatch)?;
            let [notice] = notices.as_slice() else {
                anyhow::bail!("expected a single notice for the current attempt");
            };
            assert_eq!(notice["attempt"], attempt);
            assert_eq!(notice["type"], "exited");
            assert_eq!(notice["state"], "unsent");
            assert_eq!(notice["fault"]["launchId"], launch);
            assert_eq!(notice["fault"]["exitCode"], 0);
            assert_eq!(notice["fault"]["processStart"], "linux:fixture:125");
            // Model an OpenCode prefill that starts with a confirmed writer
            // and sees that same worker's terminal receipt at settlement.
            write_json(&receipt, &started_receipt(), None)?;
            assert!(matches!(
                confirm_launch(first.clone(), || launch_receipt(&state, &row["create"]))?,
                LaunchConfirmation::Started(_)
            ));
            write_json(&receipt, &terminal, None)?;
            let LaunchConfirmation::Terminal(after_prefill) =
                confirm_launch(first, || launch_receipt(&state, &row["create"]))?
            else {
                anyhow::bail!("activation cannot settle a terminal launch");
            };
            assert_eq!(after_prefill, observed);
            super::super::startup::unconfirmed_zero(&state, &row, launch, &after_prefill);
            assert_eq!(super::super::messages::history(&state, dispatch)?, notices);
            assert_eq!(read_record(&assignment)?, row);
            assert_eq!(std::fs::read(&assignment)?, original_assignment);
            assert_eq!(host_launch::read_receipt(&receipt), Some(terminal));
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(state);
        result
    }

    #[test]
    fn herdr_confirmation_requires_the_verified_started_writer() -> Result<()> {
        let reading = confirm_launch(started_receipt(), || Ok(started_receipt()))?;
        let LaunchConfirmation::Started(writer) = reading else {
            anyhow::bail!("expected a verified started writer");
        };
        assert_eq!(writer.pid, 42);
        assert_eq!(writer.creation, "linux:fixture:125");
        let mut wrong = started_receipt();
        wrong["pid"] = json!(17);
        assert!(confirm_launch(started_receipt(), || Ok(wrong)).is_err());
        assert!(matches!(
            confirm_launch(started_receipt(), || Ok(json!({"category":"unknown"})))?,
            LaunchConfirmation::Pending
        ));
        Ok(())
    }

    #[test]
    fn herdr_offline_resume_requires_the_same_tracked_tree_stop() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("wfc-herdr-offline-proof-{}", uuid::Uuid::new_v4()));
        let state = root.as_path();
        let result = (|| -> Result<()> {
            let launch = "11111111111111111111111111111111";
            let attempt = "a-123456789abc";
            let dispatch = "d-aabbccdd";
            let launches = state.join("launches");
            std::fs::create_dir_all(&launches)?;
            let receipt = launches.join(format!("{launch}.json"));
            let binding = json!({"endpoint":state.join("herdr.sock"),
                "service":{"pid":42,"creation":"linux:fixture:125"},
                "workspace":"w-1","pane":"p-1","terminal":"t-1",
                "nonce":"fixture","receipt":receipt});
            let row = json!({"dispatch":dispatch,"attempt":attempt,"executionId":attempt,"round":1,
                "backend":"herdr","backendHandle":binding,
                "create":{"result":"accepted","dispatch":dispatch,"attempt":attempt,"round":1,
                          "receipt":receipt,"herdrBinding":binding}});
            let terminal = json!({"category":"process_exited","exitCode":17,"backend":"herdr",
                "launchId":launch,"dispatch":dispatch,"attempt":attempt,"round":"1","treeTracked":true});
            write_json(&receipt, &terminal, None)?;
            let observed = launch_receipt(state, &row["create"])?;
            let stop = receipt.with_extension("stopped");
            let error = verify_offline_tree_stop(state, &row, &observed).unwrap_err();
            assert_eq!(
                error.to_string(),
                "original Herdr worker tree has no matching stop receipt"
            );

            let mut stopped = terminal.clone();
            stopped["writerTreeStopped"] = json!(true);
            for (field, wrong) in [
                ("launchId", json!("22222222222222222222222222222222")),
                ("dispatch", json!("d-deadbeef")),
                ("attempt", json!("a-123456789abd")),
                ("round", json!("2")),
                ("backend", json!("tmux")),
                ("category", json!("started")),
                ("treeTracked", json!(false)),
                ("writerTreeStopped", json!(false)),
                ("exitCode", json!(18)),
            ] {
                let mut mismatched = stopped.clone();
                mismatched[field] = wrong;
                write_json(&stop, &mismatched, None)?;
                assert!(
                    verify_offline_tree_stop(state, &row, &observed).is_err(),
                    "{field}"
                );
            }
            write_json(&stop, &stopped, None)?;
            verify_offline_tree_stop(state, &row, &observed)?;

            let legacy_exit = json!({"category":"process_exited","exitCode":17});
            assert!(verify_offline_tree_stop(state, &row, &legacy_exit).is_err());
            let mut legacy = row;
            legacy["create"]
                .as_object_mut()
                .unwrap()
                .remove("herdrBinding");
            std::fs::remove_file(&stop)?;
            write_json(&receipt, &legacy_exit, None)?;
            verify_offline_tree_stop(state, &legacy, &legacy_exit)?;
            assert!(verify_handoff_source(state, &legacy).is_err());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(root);
        result
    }
}
