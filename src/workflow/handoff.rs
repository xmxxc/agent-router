//! Close a verified native tmux writer before assigning a fresh execution.
use super::{assignment, backend, controller, herdr, host, messages, view, worker_owner};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::ErrorKind,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
use wfc_herdr::Process;

pub const USAGE: &str = "wfc handoff DISPATCH --host HOST [--backend tmux|herdr] [--herdr-endpoint PATH] [--task PATH --report PATH] [--model MODEL] [--owner-token TOKEN]";
pub(super) const PENDING_CALLBACK_HASH: &str = "handoffCallbackHash";

pub struct Handoff<'a> {
    dispatch: &'a str,
    host: &'a str,
    backend: &'a str,
    endpoint: Option<&'a str>,
    next: Option<(&'a Path, &'a Path)>,
    model: Option<&'a str>,
    owner_token: Option<&'a str>,
}

impl<'a> Handoff<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut dispatch = None;
        let [host, backend, endpoint, task, report, model, owner_token] = messages::options(
            args,
            [
                "--host",
                "--backend",
                "--herdr-endpoint",
                "--task",
                "--report",
                "--model",
                "--owner-token",
            ],
            Some((&mut dispatch, "dispatch ID")),
        )?;
        ensure!(
            task.is_some() == report.is_some(),
            "give both --task and --report, or neither"
        );
        let backend = backend.unwrap_or("tmux");
        ensure!(
            matches!(backend, "tmux" | "herdr"),
            "--backend must be tmux or herdr"
        );
        ensure!(
            if backend == "herdr" {
                endpoint.is_some_and(|socket| Path::new(socket).is_absolute())
            } else {
                endpoint.is_none()
            },
            "an absolute --herdr-endpoint is required for a Herdr target only"
        );
        Ok(Self {
            dispatch: dispatch
                .filter(|id| super::valid_id(id))
                .context("missing or invalid dispatch ID")?,
            host: host
                .filter(|name| matches!(*name, "codex" | "opencode" | "claude"))
                .context("--host must be codex, opencode or claude")?,
            backend,
            endpoint,
            next: task
                .zip(report)
                .map(|(task, report)| (Path::new(task), Path::new(report))),
            model,
            owner_token,
        })
    }

    pub fn run(self) -> Result<Value> {
        controller::require_controller("handoff")?;
        let state = super::state_dir()?;
        let dir = state.join("dispatches");
        let path = dir.join(format!("{}.json", self.dispatch));
        let read = || {
            assignment::public_record(&dir, self.dispatch)?
                .with_context(|| format!("unknown native dispatch {}", self.dispatch))
        };
        let initial = read()?;
        let scope = super::record_scope(&initial).context("invalid dispatch scope")?;
        let token = controller::token(self.owner_token);
        let authorize = || -> Result<Value> {
            let owners = controller::Owners::read(&state)?;
            owners.require(scope, token.as_deref(), "handoff")?;
            Ok(owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone())
        };
        authorize()?;
        let _scope = controller::lock_scope(&state, scope)?;
        let generation = authorize()?;
        let row = read()?;
        ensure!(
            row == initial,
            "dispatch changed while preparing handoff; inspect status and retry"
        );
        worker_owner::validate_close(&row)?;
        if let Some(event) = assignment::unresolved_lifecycle(&row) {
            return Ok(unresolved(
                &row,
                event,
                "previous lifecycle operation is unresolved",
            ));
        }
        if let Some(event) = self.duplicate(&row)? {
            let mut result = response(&row, event, "accepted");
            result["duplicate"] = json!(true);
            return Ok(result);
        }
        ensure!(
            row["backend"] != "herdr",
            "Herdr source handoff requires verified stopping of the original worker tree"
        );
        ensure!(
            row["backend"] == "tmux"
                && row["host"].is_string()
                && assignment::identified(&row)
                && row["executionId"] == row["attempt"]
                && row["create"]["result"] == "accepted"
                && row["tmuxSocket"]
                    .as_str()
                    .is_some_and(|socket| Path::new(socket).is_absolute()),
            "handoff requires an original native tmux execution with a confirmed launch"
        );
        ensure!(
            row["host"] != self.host || row["backend"] != self.backend,
            "handoff requires a different host or backend"
        );
        require_scope_settled(&state, scope)?;

        let model = self.model.or_else(|| row["model"].as_str());
        let history = || view::merged_history(&row, messages::history(&state, self.dispatch)?);
        let execution_lock = assignment::close_lock(&state, &row)?;
        let record_preflight = |result: &str, diagnostic: &str| -> Result<Value> {
            let _inbox = messages::lock_for_publish(&state)?;
            ensure!(
                authorize()? == generation && read()? == row,
                "assignment or owner changed while inspecting handoff"
            );
            let mut next = row.clone();
            if next["audit"].is_null() {
                next["audit"] = json!([]);
            }
            let audit = next["audit"]
                .as_array_mut()
                .context("invalid lifecycle audit")?;
            let id = audit.len() + 1;
            audit.push(json!({
                "id":id, "operation":"handoff","phase":"preflight","result":result,"dispatch":self.dispatch,
                "round":row["round"],"attempt":row["attempt"],"fromRound":row["round"],"fromAttempt":row["attempt"],
                "fromHost":row["host"],"toHost":self.host,"ownerGeneration":generation,"diagnostic":diagnostic,
                "at":chrono::Utc::now().to_rfc3339()
            }));
            super::write_json(&path, &next, None)?;
            messages::sync_directory(&dir)?;
            let mut reply = response(&row, &next["audit"][id - 1], result);
            reply["diagnostic"] = json!(diagnostic);
            Ok(reply)
        };
        let mut previous = row.clone();
        let (worker, inspection) = match backend::observe_with_mode(&state, &mut previous, true) {
            Ok(observed) => observed,
            Err(error) => {
                return record_preflight(
                    "uncertain",
                    &format!("cannot verify the original execution: {error}"),
                )
            }
        };
        let bound = inspection["result"] == "accepted"
            && inspection["executionId"] == row["executionId"]
            && inspection["receipt"] == row["create"]["receipt"]
            && (inspection["alive"] == false
                || (inspection["alive"] == true
                    && inspection["owned"] == true
                    && (inspection["workerBound"] == true || worker["state"] == "exited")));
        let archived = inspection["result"] == "completed"
            && inspection["alive"] == false
            && inspection["resourceCleanup"] == "confirmed";
        if !bound && !archived {
            if inspection["result"] == "refused"
                || (inspection["alive"] == true && inspection["owned"] == false)
            {
                return record_preflight(
                    "refused",
                    "original tmux execution is not owned by this dispatch",
                );
            }
            return record_preflight("uncertain", "original tmux execution is unverified");
        }
        let old_writer = match backend::handoff_writer(&state, &row) {
            Ok(writer) => writer,
            Err(error) => {
                return record_preflight(
                    "uncertain",
                    &format!("cannot verify the original writer: {error}"),
                )
            }
        };

        let completion = |history: &[Value]| -> Result<Value> {
            let mut old = row.clone();
            old["worker"] = worker.clone();
            view::project(&mut old, history, inspection["alive"].as_bool(), false)?;
            Ok(old)
        };
        let completed = completion(&history()?)?["state"] == "done";
        ensure!(
            completed == self.next.is_some(),
            "{}",
            if completed {
                "the current task is complete; give the next --task and --report"
            } else {
                "the current task is unfinished; handoff without --task and --report"
            }
        );

        let mut next = self.prepare_record(&row, scope, model)?;
        let target_service = if let Some(socket) = self.endpoint {
            ensure!(
                generation.is_null() && next["round"] == 1,
                "Herdr handoff requires an unowned scope and a first-round target"
            );
            Some(herdr::handoff_target(
                &state,
                self.dispatch,
                Path::new(socket),
            )?)
        } else {
            None
        };
        let options = host::Options {
            name: self.host,
            backend: self.backend,
            config: row["launchConfig"].as_str(),
            model,
            effort: row["effort"].as_str(),
            git: row["git"] == true,
            network: row["network"] == true,
            ..host::Options::default()
        };
        let mut launch = options.prepare_handoff(&state, &mut next)?;
        super::bind_worker_callback(&mut next, Some(&mut launch));

        let mut closing = row.clone();
        let index = {
            let _inbox = messages::lock_for_publish(&state)?;
            ensure!(
                authorize()? == generation && read()? == row,
                "assignment or owner changed while preparing handoff"
            );
            let history = history()?;
            let old = completion(&history)?;
            ensure!(
                (old["state"] == "done") == completed,
                "worker completion changed while preparing handoff"
            );
            let mut rounds = old["rounds"]
                .as_array()
                .cloned()
                .context("missing round history")?;
            if completed {
                ensure!(
                    !rounds
                        .iter()
                        .chain(&history)
                        .any(|old| old["report"] == next["report"]),
                    "the next report was already used by this dispatch"
                );
            } else if let Some(last) = rounds
                .iter_mut()
                .rev()
                .find(|round| round["round"] == row["round"] && round["attempt"] == row["attempt"])
            {
                if last["state"] == "running" {
                    last["state"] = json!("unknown");
                }
            }
            let now = chrono::Utc::now();
            closing["rounds"] = json!(rounds);
            rounds.push(
                json!({"round":next["round"],"attempt":next["attempt"],"task":next["task"],
                "report":next["report"],"created":now.timestamp_micros() as f64 / 1_000_000.0,
                "completed":null,"state":"unknown","result":null}),
            );
            next["rounds"] = json!(rounds);
            closing[PENDING_CALLBACK_HASH] = next[controller::CALLBACK_HASH].clone();
            if closing["audit"].is_null() {
                closing["audit"] = json!([]);
            }
            let audit = closing["audit"]
                .as_array_mut()
                .context("invalid lifecycle audit")?;
            let index = audit.len();
            let mut event = json!({
                "id":index + 1,"operation":"handoff","dispatch":self.dispatch,
                "round":next["round"],"attempt":next["attempt"],"fromRound":row["round"],
                "fromAttempt":row["attempt"],"toAttempt":next["attempt"],
                "fromHost":row["host"],"toHost":next["host"],"fromModel":row["model"],"toModel":next["model"],
                "fromTask":row["task"],"fromReport":row["report"],"toTask":next["task"],"toReport":next["report"],
                "ownerGeneration":generation,"at":now.to_rfc3339(),"result":"pending","phase":"closing",
                "priorCreate":row["create"],"oldWriter":old_writer
            });
            if let Some(service) = &target_service {
                event["fromBackend"] = json!("tmux");
                event["target"] =
                    json!({"backend":"herdr","endpoint":self.endpoint,"service":service});
            }
            audit.push(event);
            super::write_json(
                &scope_path(&state, scope),
                &scope_binding(&closing, &closing["audit"][index]),
                None,
            )?;
            messages::sync_directory(&state)?;
            super::write_json(&path, &closing, None)?;
            messages::sync_directory(&dir)?;
            index
        };

        // Shared close takes the original execution lock itself. The scope
        // remains held, and the durable intent now fences old callbacks.
        drop(execution_lock);
        let event = &closing["audit"][index];
        authorize().and_then(|owner| {
            ensure!(owner == generation, "handoff owner generation changed");
            Ok(())
        })?;
        let closed = match worker_owner::close_for_handoff_locked(&state, &closing, index + 1) {
            Ok(closed) if verified_close(&row, &closed) => closed,
            Ok(_) => {
                return Ok(unresolved(
                    &closing,
                    event,
                    "original close did not confirm the same execution",
                ))
            }
            Err(error) => {
                return Ok(unresolved(
                    &closing,
                    event,
                    &format!("original close is unconfirmed: {error}"),
                ))
            }
        };
        let stop_evidence = match writer_stopped(&state, &row, old_writer.as_ref()) {
            Ok(Some(evidence)) => evidence,
            Ok(None) => {
                return Ok(unresolved(
                    &closing,
                    event,
                    "original writer has not stopped",
                ))
            }
            Err(error) => {
                return Ok(unresolved(
                    &closing,
                    event,
                    &format!("cannot verify that the original writer stopped: {error}"),
                ))
            }
        };

        // Only a recorded stop and a matching close permit the new callback
        // and attempt to become current. No retry uses this path again.
        let _new_execution = assignment::close_lock(&state, &next)?;
        {
            let _inbox = messages::lock_for_publish(&state)?;
            ensure!(
                authorize()? == generation && read()? == closing,
                "assignment or owner changed before handoff launch"
            );
            next["audit"] = closing["audit"].clone();
            next["audit"][index]["close"] = closed;
            next["audit"][index]["oldWriterStopped"] = json!(true);
            next["audit"][index]["writerStopEvidence"] = stop_evidence;
            next["audit"][index]["phase"] = json!("starting");
            super::write_json(&path, &next, None)?;
            messages::sync_directory(&dir)?;
        }
        ensure!(
            authorize()? == generation,
            "handoff owner changed before launch"
        );
        if let Some(service) = target_service {
            return match herdr::create_for_handoff(&state, &next, index + 1, launch, &service) {
                Ok(settled) => Ok(launch_response(&settled, index)),
                Err(error) => Ok(unresolved(
                    &next,
                    &next["audit"][index],
                    &format!("could not confirm Herdr handoff launch: {error:#}"),
                )),
            };
        }
        let reply = backend::create_for_handoff(&state, &next, index + 1, launch).unwrap_or_else(|error| {
            json!({"result":"uncertain","diagnostic":format!("could not confirm handoff launch: {error:#}")})
        });
        {
            let inbox = messages::lock_for_publish(&state)?;
            ensure!(
                authorize()? == generation && read()? == next,
                "assignment or owner changed before handoff settlement"
            );
            settle_launch(&mut next, index, reply);
            super::write_json(&path, &next, None)?;
            messages::sync_directory(&dir)?;
            // The verified receipt can report a startup fault for this attempt
            // while the transfer itself still needs reconciliation.
            if backend::publish_unconfirmed_tmux_exit(&state, &next, Some(inbox)).is_err() {
                eprintln!("wfc: could not publish confirmed startup failure");
            }
        }
        Ok(launch_response(&next, index))
    }

    fn duplicate<'r>(&self, row: &'r Value) -> Result<Option<&'r Value>> {
        if row["host"] != self.host
            || row["backend"] != self.backend
            || row["create"]["result"] != "accepted"
            || self
                .endpoint
                .is_some_and(|endpoint| row["endpoint"] != endpoint)
            || self.model.is_some_and(|model| row["model"] != model)
        {
            return Ok(None);
        }
        if let Some((task, report)) = self.next {
            let task = task.canonicalize()?;
            let report = super::resolve_report_path(report)?;
            if row["task"].as_str().map(Path::new) != Some(task.as_path())
                || row["report"].as_str().map(Path::new) != Some(report.as_path())
            {
                return Ok(None);
            }
        }
        Ok(row["audit"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|event| {
                event["operation"] == "handoff"
                    && event["result"] == "accepted"
                    && event["phase"] == "complete"
                    && event["toAttempt"] == row["attempt"]
                    && event["toHost"] == self.host
                    && event["launch"]["receipt"].is_string()
                    && event["launch"]["receipt"] == row["create"]["receipt"]
                    && match self.endpoint {
                        Some(endpoint) => {
                            event["target"]["backend"] == "herdr"
                                && event["target"]["endpoint"] == endpoint
                        }
                        None => event["target"].is_null(),
                    }
            }))
    }

    fn prepare_record(&self, row: &Value, scope: &str, model: Option<&str>) -> Result<Value> {
        let mut next = row.clone();
        let cwd = Path::new(row["cwd"].as_str().context("missing original cwd")?);
        ensure!(
            cwd.is_dir() && cwd.canonicalize()? == cwd,
            "the original worker directory changed"
        );
        let report = Path::new(row["report"].as_str().context("missing original report")?);
        let report_dir = report
            .parent()
            .context("missing original report directory")?;
        ensure!(
            report_dir.is_dir() && report_dir.canonicalize()? == report_dir,
            "the original report directory changed"
        );
        if let Some((task, report)) = self.next {
            let task = task.canonicalize().context("resolve next task file")?;
            ensure!(task.is_file(), "--task must name a file");
            let report = super::resolve_report_path(report)?;
            ensure!(
                report.parent() == Some(report_dir),
                "the next report must be in the original report directory"
            );
            let report = super::report_destination(&report)?;
            ensure!(
                report.parent() == Some(report_dir),
                "the next report must be in the original report directory"
            );
            next["task"] = json!(task.to_str().context("task path must be UTF-8")?);
            next["report"] = json!(report.to_str().context("report path must be UTF-8")?);
            next["round"] = json!(row["round"]
                .as_u64()
                .and_then(|round| round.checked_add(1))
                .context("round limit reached")?);
        }
        let attempt = format!("a-{}", Uuid::new_v4().simple());
        next["scope"] = json!(scope);
        next["attempt"] = json!(attempt);
        next["executionId"] = json!(attempt);
        next["host"] = json!(self.host);
        next["backend"] = json!(self.backend);
        next["model"] = json!(model);
        for field in [
            "create",
            "worker",
            "hostSession",
            "opencodeOpening",
            "opencodeSession",
            "codexHome",
            "codexBriefHash",
            "trustedBy",
            "ignored",
            "legacyExecutionId",
            "legacyLaunchId",
            "legacyWorkerPane",
            "endpoint",
            "backendHandle",
            "pendingInput",
            "pendingReplyMessageId",
        ] {
            next.as_object_mut().unwrap().remove(field);
        }
        if let Some(endpoint) = self.endpoint {
            next["endpoint"] = json!(endpoint);
            let record = next.as_object_mut().unwrap();
            record.remove("tmuxSocket");
            record.remove("tmuxLaunchProtocol");
            record.remove("tmux");
        } else {
            next["tmuxLaunchProtocol"] = json!(backend::LAUNCH_PROTOCOL);
        }
        Ok(next)
    }
}

pub(super) fn settle_launch(row: &mut Value, index: usize, mut reply: Value) {
    let accepted = reply["result"] == "accepted" && reply["receipt"].is_string();
    for key in ["dispatch", "round", "attempt"] {
        reply[key] = row[key].clone();
    }
    row["create"] = reply.clone();
    let audit = &mut row["audit"][index];
    audit["result"] = json!(if accepted { "accepted" } else { "uncertain" });
    audit["launch"] = reply;
    audit["settledAt"] = json!(chrono::Utc::now().to_rfc3339());
    if accepted {
        audit["phase"] = json!("complete");
        row["rounds"].as_array_mut().unwrap().last_mut().unwrap()["state"] = json!("running");
    }
}

fn launch_response(row: &Value, index: usize) -> Value {
    let event = &row["audit"][index];
    let result = event["result"].as_str().unwrap_or("uncertain");
    let mut reply = response(row, event, result);
    if result != "accepted" {
        reply["diagnostic"] = event["launch"]["diagnostic"].clone();
    }
    reply
}

fn response(row: &Value, event: &Value, result: &str) -> Value {
    let attempt = event.get("toAttempt").unwrap_or(&row["attempt"]);
    let mut reply = json!({"dispatch":row["dispatch"],"operation":"handoff","result":result,
        "audit":event["id"],"fromAttempt":event["fromAttempt"],"attempt":attempt,
        "host":event["toHost"],"round":event["round"]});
    if event["target"]["backend"] == "herdr" {
        reply["backend"] = json!("herdr");
        reply["endpoint"] = event["target"]["endpoint"].clone();
    }
    reply
}

fn unresolved(row: &Value, event: &Value, diagnostic: &str) -> Value {
    let mut result = response(row, event, "uncertain");
    result["error"] = json!("lifecycle_reconcile_required");
    result["diagnostic"] = json!(diagnostic);
    if result["fromAttempt"].is_null() {
        result["fromAttempt"] = row["attempt"].clone();
    }
    result
}

fn verified_close(row: &Value, closed: &Value) -> bool {
    closed["result"] == "completed"
        && closed["resourceCleanup"] == "confirmed"
        && ["dispatch", "round", "attempt"]
            .iter()
            .all(|key| closed[*key] == row[*key])
        && row["attempt"]
            .as_str()
            .is_some_and(|attempt| closed["closeOperationId"] == format!("close:{attempt}"))
}

fn writer_stopped(
    state: &Path,
    row: &Value,
    writer: Option<&wfc_herdr::Process>,
) -> Result<Option<Value>> {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let old_pid_stopped = writer
            .map(wfc_herdr::Process::alive)
            .transpose()?
            .is_none_or(|live| !live);
        if old_pid_stopped {
            if let Some(evidence) = backend::handoff_tree_stopped(state, row)? {
                return Ok(Some(evidence));
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn scope_path(state: &Path, scope: &str) -> PathBuf {
    state.join(format!("handoff-order-{}.json", messages::body_hash(scope)))
}

fn scope_binding(row: &Value, audit: &Value) -> Value {
    let mut binding = json!({"scope":super::record_scope(row),"dispatch":row["dispatch"],"audit":audit["id"],
        "fromAttempt":audit["fromAttempt"],"toAttempt":audit["toAttempt"],"ownerGeneration":audit["ownerGeneration"]});
    if let Some(target) = audit.get("target") {
        binding["target"] = target.clone();
    }
    binding
}

pub(crate) fn require_launch(
    state: &Path,
    dispatch: &str,
    attempt: &str,
    audit: usize,
) -> Result<String> {
    let (event, previous) = verify_launch(state, dispatch, attempt, audit, "tmux")?;
    ensure!(event["target"].is_null(), "tmux handoff target changed");
    Ok(previous)
}

pub(super) fn require_herdr_launch(
    state: &Path,
    row: &Value,
    audit: usize,
    service: &Process,
) -> Result<()> {
    let dispatch = row["dispatch"].as_str().context("missing dispatch")?;
    let attempt = row["attempt"].as_str().context("missing attempt")?;
    let (event, _) = verify_launch(state, dispatch, attempt, audit, "herdr")?;
    ensure!(
        event["ownerGeneration"].is_null()
            && event["target"]
                == json!({"backend":"herdr","endpoint":row["endpoint"],"service":service}),
        "Herdr handoff target changed"
    );
    Ok(())
}

fn verify_launch(
    state: &Path,
    dispatch: &str,
    attempt: &str,
    audit: usize,
    target: &str,
) -> Result<(Value, String)> {
    ensure!(super::valid_id(dispatch), "invalid handoff dispatch");
    let row = assignment::public_record(&state.join("dispatches"), dispatch)?
        .context("missing handoff launch assignment")?;
    let event = assignment::unresolved_handoff(&row).context("missing pending handoff launch")?;
    let scope = super::record_scope(&row).context("invalid handoff scope")?;
    let binding = read_scope_binding(state, scope)?;
    ensure!(
        row["backend"] == target
            && row["executionId"] == attempt
            && row["attempt"] == attempt
            && event["phase"] == "starting"
            && event["result"] == "pending"
            && event["id"] == audit
            && event["toAttempt"] == attempt
            && event["oldWriterStopped"] == true
            && event["writerStopEvidence"]["writerTreeStopped"] == true
            && event["writerStopEvidence"]["attempt"] == event["fromAttempt"]
            && event["writerStopEvidence"]["dispatch"] == dispatch
            && event["close"]["result"] == "completed"
            && event["close"]["resourceCleanup"] == "confirmed"
            && event["close"]["attempt"] == event["fromAttempt"]
            && event["close"]["round"] == event["fromRound"]
            && event["close"]["dispatch"] == row["dispatch"]
            && binding.as_ref() == Some(&scope_binding(&row, event)),
        "handoff launch does not match the original closure and pending assignment"
    );
    let owner = controller::Owners::read(state)?
        .public(scope)?
        .unwrap_or(Value::Null);
    ensure!(
        owner["generation"] == event["ownerGeneration"]
            && (owner.is_null() || owner["active"] == true),
        "handoff no longer has the original owner"
    );
    let previous = json!({"dispatch":dispatch,"attempt":event["fromAttempt"],
        "round":event["fromRound"],"executionId":event["fromAttempt"],"create":event["priorCreate"]});
    let launch = backend::receipt_id(&previous)?.context("missing original handoff launch")?;
    ensure!(
        event["writerStopEvidence"]["launchId"] == launch
            && event["fromRound"]
                .as_u64()
                .is_some_and(|round| { event["writerStopEvidence"]["round"] == round.to_string() })
            && verified_close(&previous, &event["close"]),
        "original handoff stop or close does not identify this launch"
    );
    Ok((event.clone(), launch.to_owned()))
}

fn read_scope_binding(state: &Path, scope: &str) -> Result<Option<Value>> {
    read_order(&scope_path(state, scope))
}

pub(super) fn read_order(path: &Path) -> Result<Option<Value>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => {
            ensure!(
                file.metadata()?.is_file(),
                "invalid lifecycle evidence file {}",
                path.display()
            );
            Ok(Some(
                serde_json::from_reader(file)
                    .with_context(|| format!("read {}", path.display()))?,
            ))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("open {}", path.display())),
    }
}

// Use the interrupt candidate's same-audit evidence. An accepted C-c only
// settles its ordering fence. Handoff still verifies the close and old writer.
fn require_interrupt_settled(state: &Path, scope: &str) -> Result<()> {
    let path = state.join(format!(
        "interrupt-order-{}.json",
        messages::body_hash(scope)
    ));
    let Some(order) = read_order(&path)? else {
        return Ok(());
    };
    ensure!(order["scope"] == scope, "interrupt order scope mismatch");
    let dispatch = order["dispatch"]
        .as_str()
        .filter(|id| super::valid_id(id))
        .context("invalid interrupt order dispatch")?;
    let attempt = order["attempt"]
        .as_str()
        .filter(|id| super::valid_identity(id, "a-"))
        .context("invalid interrupt order attempt")?;
    let audit = order["audit"]
        .as_u64()
        .filter(|&id| id > 0)
        .context("invalid interrupt order audit")?;
    let receipt = state
        .join("dispatches")
        .join(format!("{dispatch}.{attempt}.{audit}.interrupt-order"));
    ensure!(
        read_order(&receipt)?.is_some_and(|receipt| receipt["order"] == order
            && matches!(
                receipt["result"].as_str(),
                Some("accepted" | "refused" | "definitely-not-sent")
            )),
        "interrupt audit {audit} is unresolved; reconcile the original execution first"
    );
    Ok(())
}

// Called with the scope lock. An abandoned handoff is not a new launch
// opportunity, and an old backend could still be working on its request.
pub(super) fn require_scope_settled(state: &Path, scope: &str) -> Result<()> {
    require_interrupt_settled(state, scope)?;
    let Some(binding) = read_scope_binding(state, scope)? else {
        return Ok(());
    };
    ensure!(binding["scope"] == scope, "handoff order scope mismatch");
    let dispatch = binding["dispatch"]
        .as_str()
        .filter(|id| super::valid_id(id))
        .context("invalid handoff order dispatch")?;
    let row = assignment::public_record(&state.join("dispatches"), dispatch)?
        .context("missing handoff order assignment")?;
    let event = row["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|event| event["operation"] == "handoff" && scope_binding(&row, event) == binding)
        .context("handoff order identity mismatch")?;
    ensure!(
        event["result"] == "accepted"
            && event["phase"] == "complete"
            && event["launch"]["receipt"].is_string(),
        "handoff audit {} is unresolved; reconcile the original execution before changing owner",
        event["id"]
    );
    Ok(())
}
