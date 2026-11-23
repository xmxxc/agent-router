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

fn native_state(state: &Path) -> Result<()> {
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

const INVALID_CLOSE_WORKER: &str = "invalid closed worker outcome";

fn valid_close_worker(worker: &Value) -> bool {
    worker["state"] == "cancelled" || (worker["state"] == "exited" && worker["exit_code"].is_i64())
}

struct Assignment<'a> {
    state: &'a Path,
    _lock: File,
    path: PathBuf,
    row: Value,
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
        ensure!(valid_id(id), "invalid dispatch ID");
        native_state(state)?;
        let path = state.join("dispatches").join(format!("{id}.json"));
        let lock_path = path.with_extension("herdr-lock");
        let lock = if new.is_some() {
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(lock_path)?
        } else {
            File::open(lock_path).context("missing native Herdr assignment lock")?
        };
        lock.lock()?;
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
        Ok(Self {
            _lock: lock,
            state,
            path,
            row,
        })
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
        if self.row["backendHandle"].is_null() {
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

    fn tracked_handle(&mut self) -> Result<Handle> {
        let mut h = self.handle()?;
        if let Some(writer) = self.launched_writer()? {
            if !h.writers.is_empty() && !h.writers.contains(&writer) {
                h.writers.push(writer);
                self.checkpoint(&h)?;
            }
        }
        Ok(h)
    }

    fn inspect(&mut self) -> Result<Value> {
        let mut h = self.tracked_handle()?;
        Ok(match h.inspect(|h| self.checkpoint(h)) {
            Ok(alive) => json!({"result":"accepted","alive":alive,"owned":true}),
            Err(e) => json!(e),
        })
    }
}

enum InputAction<'a> {
    Submit(&'a str),
    ActivatePrefill(&'a str),
}

impl Assignment<'_> {
    fn input(&mut self, action: InputAction<'_>) -> Result<Value> {
        ensure!(self.row["pendingInput"] == false, "input still pending");
        let mut h = self.tracked_handle()?;
        ensure!(h.stopped_evidence.is_none(), "Herdr writer stopped");
        let mut next = self.row.clone();
        next["pendingInput"] = json!(true);
        self.save(next)?;
        let result = match action {
            InputAction::ActivatePrefill(text) => {
                h.activate_opencode_prompt(text, |h| self.checkpoint(h))
            }
            InputAction::Submit(text) => {
                h.submit(text, self.row["host"] == "codex", |h| self.checkpoint(h))
            }
        };
        if !matches!(result,Err(e) if e.kind == FailureKind::Uncertain) {
            let mut next = self.row.clone();
            next["pendingInput"] = json!(false);
            self.save(next)?;
        }
        Ok(match result {
            Ok(()) => json!({"result":"accepted"}),
            Err(e) => json!(e),
        })
    }
}

pub(super) fn create(state: &Path, row: &Value, launch: host::Launch) -> Result<Value> {
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    let cwd = Path::new(row["cwd"].as_str().context("missing cwd")?);
    let prefill = match launch.argv.as_slice() {
        [.., flag, brief] if row["host"] == "opencode" && flag == "--prompt" => Some(brief.clone()),
        _ => None,
    };
    let mut unit = Assignment::lock(state, id, Some(row.clone()))?;
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
    unit.row["create"] = json!({"result":"uncertain","receipt":files.receipt});
    unit.row["pendingInput"] = json!(false);
    unit.row["audit"] = json!([]);
    // The provider checkpoints the nonce and service before its first external effect.
    let created = Handle::create(
        Path::new(row["endpoint"].as_str().context("missing endpoint")?),
        row["attempt"].as_str().context("missing attempt")?,
        &launch,
        |h| unit.checkpoint(h),
    );
    let mut answer = match created {
        Ok(_) => {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let evidence = host_launch::read_receipt(&files.receipt).unwrap_or(Value::Null);
                if evidence["category"] == "started" {
                    break match unit.tracked_handle() {
                        Ok(_) => match &prefill {
                            Some(text) => unit.input(InputAction::ActivatePrefill(text))?,
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
    if !unit.row["backendHandle"].is_null() {
        let mut next = unit.row.clone();
        next["create"] = answer.clone();
        unit.save(next)?;
    } else if !unit.path.try_exists()? {
        // No checkpoint means the provider never issued workspace.create.
        std::fs::remove_file(&files.receipt)?;
        std::fs::remove_file(unit.path.with_extension("herdr-lock"))?;
    }
    ensure!(
        answer["result"] == "accepted",
        "backend create {} for {id}: {}",
        answer["result"],
        answer["diagnostic"]
    );
    Ok(unit.row)
}

pub(super) fn observe(state: &Path, row: &mut Value) -> Result<(Value, Value)> {
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
                unit.inspect()?
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
        handle = match unit.tracked_handle() {
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
        return Ok(receipt);
    };
    let outcome = match close.finish() {
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
    ensure!(
        unit.row["round"] == input.round && unit.row["attempt"] == input.attempt,
        "stale Herdr input assignment"
    );
    ensure!(
        unit.verified_archive(&unit.handle()?)?.is_none(),
        "Herdr dispatch closed"
    );
    ensure!(!unit.pending_close(), "Herdr close still pending");
    ensure!(
        unit.row["create"]["result"] == "accepted",
        "Herdr launch is not active"
    );
    let mut response = unit.input(InputAction::Submit(&input.text))?;
    response["dispatch"] = json!(input.dispatch);
    response["round"] = json!(input.round);
    response["attempt"] = json!(input.attempt);
    Ok(response)
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
            unit.tracked_handle()?
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
