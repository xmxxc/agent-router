//! Replace an offline execution with a new attempt in its original host conversation.
#[cfg(test)]
mod tests;

use super::{assignment, backend, controller, host, messages, view};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const USAGE: &str = "wfc resume DISPATCH [--task FILE --report FILE] [--owner-token TOKEN]";

pub struct Resume<'a> {
    dispatch: &'a str,
    next: Option<(&'a Path, &'a Path)>,
    owner_token: Option<&'a str>,
}

impl<'a> Resume<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut dispatch = None;
        let [task, report, owner_token] = messages::options(
            args,
            ["--task", "--report", "--owner-token"],
            Some((&mut dispatch, "dispatch ID")),
        )?;
        ensure!(
            task.is_some() == report.is_some(),
            "give both --task and --report, or neither"
        );
        Ok(Self {
            dispatch: dispatch
                .filter(|id| super::valid_id(id))
                .context("missing or invalid dispatch ID")?,
            next: task
                .zip(report)
                .map(|(task, report)| (Path::new(task), Path::new(report))),
            owner_token,
        })
    }

    pub fn run(self) -> Result<Value> {
        controller::require_controller("resume")?;
        self.run_in(super::writable_state_dir()?, backend::resume_bundled)
    }

    fn run_in(
        self,
        state: PathBuf,
        start_bundled: impl FnOnce(&Path, &Value, &Value, host::Launch, Option<&str>) -> Result<Value>,
    ) -> Result<Value> {
        super::ensure_state_writable(&state)?;
        let dir = state.join("dispatches");
        let read = || -> Result<Value> {
            let row = assignment::public_record(&dir, self.dispatch)?
                .with_context(|| format!("unknown native dispatch {}", self.dispatch))?;
            if row["backend"] == "bundled" {
                return messages::read_callback_record(&dir, self.dispatch);
            }
            Ok(row)
        };
        let initial = read()?;
        assignment::require_not_tmux_history(&initial)?;
        ensure!(initial["legacyArchived"] != true, "offline legacy assignment has no resumable worker");
        let scope = super::record_scope(&initial).context("invalid dispatch scope")?;
        let token = controller::token(self.owner_token);
        let authorize = || -> Result<Value> {
            let owners = controller::Owners::read(&state)?;
            owners.require(scope, token.as_deref(), "resume")?;
            Ok(owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone())
        };
        authorize()?;
        let _scope = controller::lock_scope(&state, scope)?;
        let generation = authorize()?;
        let row = read()?;
        ensure!(
            row == initial,
            "dispatch changed while preparing resume; inspect status and retry"
        );
        let claude = row["host"] == "claude";
        let opencode = row["host"] == "opencode";
        let herdr = row["backend"] == "herdr";
        let bundled = row["backend"] == "bundled";
        ensure!(
            (herdr || bundled) && row["host"] == "codex" && row["round"] == 1,
            "resume requires an original Codex task using Herdr or bundled"
        );
        ensure!(
            assignment::identified(&row) && row["controllerOwner"].is_null(),
            "invalid or unverified assignment for resume"
        );
        if bundled {
            ensure!(
                self.next.is_none(),
                "bundled resume requires the original task and report"
            );
            ensure!(
                row["publicationDiagnostic"].is_null(),
                "cannot verify the original bundled publication"
            );
            if let Some(event) = assignment::resume_audit(&row) {
                if event["result"] == "accepted" && backend::bundled_input_ticket(&row).is_ok() {
                    return Ok(
                        json!({"operation":"resume","backend":"bundled","result":"accepted",
                        "dispatch":row["dispatch"],"attempt":row["attempt"],"round":row["round"],
                        "audit":event["id"],"session":event["session"],"resumed":row["dispatch"],
                        "launchResult":"accepted","duplicate":true}),
                    );
                }
                return Ok(reconcile(
                    &row,
                    event["id"].clone(),
                    "previous bundled resume is unresolved",
                ));
            }
            if !row["codexHome"].is_string()
                || !row["codexBriefHash"].as_str().is_some_and(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Ok(reconcile(
                    &row,
                    Value::Null,
                    "missing original bundled Codex conversation binding",
                ));
            }
            if !host::worker_brief(&row, host::FIRST_BRIEF)
                .is_ok_and(|brief| row["codexBriefHash"] == messages::body_hash(&brief))
            {
                return Ok(reconcile(
                    &row,
                    Value::Null,
                    "original bundled task and report do not match the Codex brief",
                ));
            }
        }
        if let Some(event) = assignment::unresolved_lifecycle(&row) {
            return Ok(reconcile(
                &row,
                event["id"].clone(),
                "previous lifecycle operation is unresolved",
            ));
        }
        if row["createPending"] == true
            || (row["create"]["result"] == "uncertain" && assignment::resume_audit(&row).is_none())
        {
            return Ok(reconcile(
                &row,
                Value::Null,
                "original creation is unresolved",
            ));
        }
        if herdr {
            ensure!(
                self.next.is_none() && generation.is_null(),
                "Herdr resume requires the original task and report in an unowned scope"
            );
            ensure!(
                assignment::resume_audit(&row).is_none(),
                "Herdr resume already recorded for this task"
            );
            if !row["codexHome"].is_string() {
                return Ok(reconcile(&row, Value::Null, "missing original Codex home"));
            }
        }
        let _execution = (!herdr)
            .then(|| assignment::close_lock(&state, &row))
            .transpose()?;
        let mut herdr_resume = None;
        let observation = if herdr {
            super::herdr::OfflineResume::verify(&state, &row).map(|(unit, worker, inspected)| {
                herdr_resume = Some(unit);
                (worker, inspected)
            })
        } else if bundled {
            backend::observe_bundled_for_resume(&state, &row)
        } else {
            backend::observe_for_resume(&state, &row)
        };
        let (worker, inspection) = match observation {
            Ok(observation) => observation,
            Err(e) => {
                return Ok(reconcile(
                    &row,
                    Value::Null,
                    &format!("cannot inspect the original execution: {e}"),
                ));
            }
        };
        if inspection["alive"] == true && inspection["owned"] == true {
            anyhow::bail!("original tmux session is still running; inspect status or attach to it");
        }
        if inspection["alive"] != false
            || !matches!(
                inspection["result"].as_str(),
                Some("accepted" | "completed")
            )
            || !(matches!(worker["state"].as_str(), Some("exited" | "not_started"))
                || bundled && worker["state"] == "cancelled")
        {
            return Ok(reconcile(
                &row,
                Value::Null,
                "cannot verify that the original execution is offline",
            ));
        }

        let history = || view::merged_history(&row, messages::history(&state, self.dispatch)?);
        let snapshot = |history: &[Value]| -> Result<Value> {
            let mut old = row.clone();
            old["worker"] = worker.clone();
            view::project(&mut old, history, Some(false), false)?;
            Ok(old)
        };
        let old = snapshot(&history()?)?;
        let completed = old["state"] == "done";
        ensure!(
            !bundled || !completed,
            "the original bundled task is already complete"
        );
        ensure!(
            completed == self.next.is_some(),
            "{}",
            if completed {
                "the current task is complete; give the next --task and --report"
            } else {
                "the current task is unfinished; resume without --task and --report"
            }
        );
        let mut next = row.clone();
        next["scope"] = json!(scope);
        let cwd = Path::new(row["cwd"].as_str().context("missing original cwd")?);
        ensure!(
            cwd.canonicalize()?.as_path() == cwd && cwd.is_dir(),
            "the original worker directory changed"
        );
        let bound = Path::new(row["report"].as_str().context("missing bound report")?);
        let report_dir = bound
            .parent()
            .context("missing original report directory")?;
        ensure!(
            report_dir.canonicalize()?.as_path() == report_dir && report_dir.is_dir(),
            "the original report directory changed"
        );
        if bundled {
            let task = Path::new(row["task"].as_str().context("missing original task")?);
            ensure!(
                task.canonicalize()?.as_path() == task
                    && task.is_file()
                    && super::resolve_report_path(bound)?.as_path() == bound
                    && match fs::symlink_metadata(bound) {
                        Ok(metadata) => metadata.is_file(),
                        Err(error) if error.kind() == ErrorKind::NotFound => true,
                        Err(error) => return Err(error).context("verify original report"),
                    },
                "the original task or report path changed"
            );
        }
        if let Some((task, report)) = self.next {
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
            let task = task.canonicalize().context("resolve next task file")?;
            ensure!(task.is_file(), "--task must name a file");
            next["task"] = json!(task.to_str().context("task path must be UTF-8")?);
            next["report"] = json!(report.to_str().context("report path must be UTF-8")?);
            next["round"] = json!(row["round"]
                .as_u64()
                .unwrap()
                .checked_add(1)
                .context("round limit reached")?);
        }
        let attempt = format!("a-{}", Uuid::new_v4().simple());
        next["attempt"] = json!(attempt);
        next["executionId"] = json!(attempt);
        if herdr || bundled {
            for field in ["backendHandle", "pendingReplyMessageId"] {
                next.as_object_mut().unwrap().remove(field);
            }
        } else {
            next["tmuxLaunchProtocol"] = json!(backend::LAUNCH_PROTOCOL);
        }
        for field in [
            "create",
            controller::CALLBACK_REF_HASH,
            "legacyExecutionId",
            "legacyLaunchId",
            "legacyWorkerPane",
            "worker",
        ] {
            next.as_object_mut().unwrap().remove(field);
        }
        let options = host::Options {
            name: row["host"].as_str().context("missing host")?,
            backend: if bundled {
                "bundled"
            } else if herdr {
                "herdr"
            } else {
                "tmux"
            },
            config: row["launchConfig"].as_str(),
            model: row["model"].as_str(),
            effort: row["effort"].as_str(),
            git: row["git"] == true,
            network: row["network"] == true,
            ..host::Options::default()
        };
        let opening = if completed {
            "[router] Resumed with a new task for you"
        } else {
            "[router] Your session was interrupted and has been resumed. Check the worktree and what you already did, then continue"
        };
        let (mut launch, claude_proof, mut opencode_plan, session) = if claude {
            match options.prepare_claude_resume(&state, &row, &mut next, opening) {
                Ok((launch, proof)) => (
                    launch,
                    Some(proof),
                    None,
                    next["hostSession"]
                        .as_str()
                        .context("missing Claude session")?
                        .to_owned(),
                ),
                Err(e) => {
                    return Ok(reconcile(
                        &row,
                        Value::Null,
                        &format!("cannot verify the original Claude conversation: {e}"),
                    ));
                }
            }
        } else if opencode {
            let prepared = (|| -> Result<_> {
                let (mut launch, brief) =
                    host::Options::prepare_opencode_resume(&state, &row, &mut next, opening)?;
                let plan =
                    host::opencode::resume::Prepared::new(&state, &next, &mut launch, brief)?;
                let session = next["hostSession"]
                    .as_str()
                    .context("missing OpenCode session")?;
                Ok((launch, None, Some(plan), session.to_owned()))
            })();
            match prepared {
                Ok(prepared) => prepared,
                Err(e) => {
                    return Ok(reconcile(
                        &row,
                        Value::Null,
                        &format!("cannot verify the original OpenCode conversation: {e}"),
                    ));
                }
            }
        } else {
            let mut launch = match options.prepare_worker(&state, &mut next, Some(opening)) {
                Ok(launch) => launch,
                Err(e) if bundled => {
                    return Ok(reconcile(
                        &row,
                        Value::Null,
                        &format!("cannot prepare the original Codex conversation: {e}"),
                    ))
                }
                Err(e) => return Err(e),
            };
            if bundled && row["launchConfig"] != next["launchConfig"] {
                return Ok(reconcile(
                    &row,
                    Value::Null,
                    "configured Codex launch path differs from the original dispatch",
                ));
            }
            if !row["codexHome"].is_null()
                && row["codexHome"] != next["codexHome"]
                && !row["codexHome"]
                    .as_str()
                    .zip(next["codexHome"].as_str())
                    .is_some_and(|(saved, current)| {
                        host::matches_unresolved_codex_home(Path::new(saved), Path::new(current))
                    })
            {
                return Ok(reconcile(
                    &row,
                    Value::Null,
                    "configured Codex home differs from the original dispatch",
                ));
            }
            let session = match conversation(&old, &next) {
                Ok(session) => session.to_string(),
                Err(e) => {
                    return Ok(reconcile(
                        &row,
                        Value::Null,
                        &format!("cannot verify the original Codex conversation: {e}"),
                    ))
                }
            };
            launch.argv.insert(launch.argv.len() - 1, session.clone());
            next["hostSession"] = json!(session);
            (launch, None, None, session)
        };
        super::bind_worker_callback(&mut next, Some(&mut launch));

        let path = dir.join(format!("{}.json", self.dispatch));
        let lock_for_publication = || {
            if opencode {
                messages::lock_for_private_publish(&state)
            } else {
                messages::lock_for_publish(&state)
            }
        };
        let event = {
            let ledger = lock_for_publication()?;
            ensure!(
                authorize()? == generation && read()? == row,
                "assignment or owner changed while preparing resume"
            );
            if bundled {
                let pending = dir.join(format!("{}.pending", self.dispatch));
                ensure!(
                    fs::symlink_metadata(pending)
                        .is_err_and(|error| error.kind() == ErrorKind::NotFound),
                    "cannot verify a settled bundled publication before resume"
                );
                if ledger.unresolved_resume_reply(&row)? {
                    return Ok(reconcile(
                        &row,
                        Value::Null,
                        "original bundled reply is unresolved",
                    ));
                }
            }
            let history = history()?;
            let current = snapshot(&history)?;
            ensure!(
                (current["state"] == "done") == completed,
                "the worker completed while preparing resume; give the next --task and --report"
            );
            let mut rounds = current["rounds"]
                .as_array()
                .cloned()
                .context("missing round history")?;
            if completed {
                ensure!(
                    !rounds
                        .iter()
                        .chain(&history)
                        .any(|r| r["report"] == next["report"]),
                    "the next report was already used by this dispatch"
                );
            }
            let now = chrono::Utc::now();
            rounds.push(
                json!({"round":next["round"],"attempt":attempt,"task":next["task"],
                "report":next["report"],"created":now.timestamp_micros() as f64 / 1_000_000.0,
                "completed":null,"state":"unknown","result":null}),
            );
            next["rounds"] = json!(rounds);
            if next["audit"].is_null() {
                next["audit"] = json!([]);
            }
            let round = next["round"].clone();
            let audit = next["audit"]
                .as_array_mut()
                .context("invalid lifecycle audit")?;
            let event = audit.len() + 1;
            audit.push(
                json!({"id":event,"operation":"resume","dispatch":self.dispatch,
                "round":round,"attempt":attempt,"fromAttempt":row["attempt"],"toAttempt":attempt,
                "fromHost":row["host"],"toHost":row["host"],"session":session,
                "at":now.to_rfc3339(),"ownerGeneration":generation,"result":"pending",
                "priorInspection":inspection,"priorWorker":worker,"priorCreate":row["create"]}),
            );
            if herdr || bundled {
                audit[event - 1]["priorBackendHandle"] = row["backendHandle"].clone();
            }
            if let Some(proof) = claude_proof {
                if let Err(error) = proof.publish(&ledger, &row, &mut next, &launch) {
                    let current = read()?;
                    let audit = if current["attempt"] == next["attempt"] {
                        json!(event)
                    } else {
                        Value::Null
                    };
                    return Ok(reconcile(
                        &current,
                        audit,
                        &format!("cannot publish Claude resume: {error}"),
                    ));
                }
            } else {
                if let Some(plan) = &opencode_plan {
                    if let Err(error) = plan.recheck(&state, &row, &next) {
                        return Ok(reconcile(
                            &row,
                            Value::Null,
                            &format!("cannot publish OpenCode resume: {error}"),
                        ));
                    }
                }
                super::write_json(&path, &next, None)?;
                if let Some(plan) = &mut opencode_plan {
                    plan.published();
                }
                messages::sync_directory(&dir)?;
            }
            event
        };

        // From here a lost response is indistinguishable from a running worker.
        // The scope lock still fences takeover, and callbacks can use the new assignment.
        let mut answer = if let Some(unit) = &mut herdr_resume {
            match unit.launch(&mut next, launch) {
                Ok(answer) => answer,
                Err(e) => {
                    return Ok(reconcile(
                        &read()?,
                        json!(event),
                        &format!("cannot confirm Herdr restart: {e}"),
                    ))
                }
            }
        } else if bundled {
            start_bundled(&state, &next, &inspection, launch, token.as_deref()).unwrap_or_else(|e| {
                json!({"result":"uncertain","diagnostic":format!("cannot confirm bundled resume: {e}")})
            })
        } else {
            backend::resume(&state, &next, launch).unwrap_or_else(|e| {
                json!({"result":"definitely-not-sent","diagnostic":format!("could not call tmux backend: {e}")})
            })
        };
        if bundled {
            let ticket = &answer["ticket"];
            let matching_ticket = ticket["version"] == 2
                && ticket["unit"] == next["dispatch"]
                && ticket["attempt"] == next["attempt"]
                && (ticket["startupProtocol"] == 1) == (next["bundledStartupCommit"] == true);
            let ready_confirmed = answer["result"] != "accepted"
                || (matching_ticket
                    && crate::bundled::confirm_resume_start(&state, ticket, &answer["identity"])
                        .unwrap_or(false));
            if !matching_ticket || !ready_confirmed {
                answer
                    .as_object_mut()
                    .context("invalid bundled resume response")?
                    .remove("ticket");
            }
            if !ready_confirmed {
                answer["result"] = json!("uncertain");
                answer["diagnostic"] =
                    json!("bundled resume did not confirm the replacement's signed ready identity");
            }
        } else if herdr && answer["result"] != "accepted" {
            answer["result"] = json!("uncertain");
            answer["error"] = json!("lifecycle_reconcile_required");
        }
        let launch_result = match answer["result"].as_str() {
            _ if !bundled
                && (answer["launchResult"] == "accepted"
                    || answer["category"] == "process_exited") =>
            {
                "accepted"
            }
            Some("accepted") => "accepted",
            Some("refused" | "definitely-not-sent") => "definitely-not-sent",
            _ => "uncertain",
        };
        answer["launchResult"] = json!(launch_result);
        if bundled && answer["result"] != "accepted" {
            answer["result"] = json!("uncertain");
            answer["error"] = json!("lifecycle_reconcile_required");
        }
        if claude && answer["result"] == "accepted" {
            host::claude::wait_for_resume_adoption(&state, &next);
        }
        if let Some(plan) = &opencode_plan {
            answer["inputResult"] = json!("definitely-not-sent");
            if answer["result"] == "accepted" {
                let permitted = {
                    let _ledger = lock_for_publication()?;
                    ensure!(
                        read()? == next,
                        "assignment changed during OpenCode resume; inspect its lifecycle audit"
                    );
                    plan.authorize_input()
                };
                if let Ok(expires_at) = permitted {
                    plan.wait_for_delivery(&state, &next, expires_at);
                }
            } else if launch_result != "definitely-not-sent" {
                answer["adoptionResult"] = json!("uncertain");
            }
        }
        {
            let ledger = match lock_for_publication() {
                Ok(ledger) => ledger,
                Err(error)
                    if bundled
                        && answer["result"] == "uncertain"
                        && answer["ticket"].is_object() =>
                {
                    ensure!(
                        read()? == next,
                        "assignment changed during resume; inspect its lifecycle audit"
                    );
                    let mut pending = reconcile(
                        &next,
                        json!(event),
                        &format!("cannot settle the bundled resume: {error}"),
                    );
                    pending["backend"] = json!("bundled");
                    pending["ticket"] = answer["ticket"].clone();
                    return Ok(pending);
                }
                Err(error) => return Err(error),
            };
            ensure!(
                read()? == next,
                "assignment changed during resume; inspect its lifecycle audit"
            );
            if claude && answer["result"] == "accepted" {
                if host::claude::resume_adopted(&ledger, &next).is_ok() {
                    answer["adoptionResult"] = json!("accepted");
                } else {
                    answer["result"] = json!("uncertain");
                    answer["adoptionResult"] = json!("uncertain");
                    answer["error"] = json!("lifecycle_reconcile_required");
                    answer["category"] = json!("host_adoption_unconfirmed");
                    answer["diagnostic"] =
                        json!("Claude did not confirm adoption of the resumed session");
                }
            }
            if let Some(plan) = opencode_plan.filter(|_| answer["result"] == "accepted") {
                let adopted = plan.adopted();
                let delivered = adopted && plan.delivered(&state, &next).is_ok_and(|done| done);
                answer["adoptionResult"] = json!(if adopted { "accepted" } else { "uncertain" });
                if delivered {
                    answer["inputResult"] = json!("accepted");
                } else {
                    if adopted {
                        answer["inputResult"] = json!("uncertain");
                    }
                    answer["result"] = json!("uncertain");
                    answer["error"] = json!("lifecycle_reconcile_required");
                    answer["category"] = json!(if adopted {
                        "input_uncertain"
                    } else {
                        "host_adoption_unconfirmed"
                    });
                    answer["diagnostic"] = json!(if adopted {
                        "OpenCode did not confirm one committed resume brief"
                    } else {
                        "OpenCode did not confirm the original session and a focused composer"
                    });
                }
            }
            for field in ["dispatch", "round", "attempt"] {
                answer[field] = next[field].clone();
            }
            if bundled && answer["ticket"].is_object() {
                next["backendHandle"] = answer["ticket"].clone();
            }
            next["create"] = answer.clone();
            let audit = &mut next["audit"][event - 1];
            for field in [
                "result",
                "diagnostic",
                "launchResult",
                "inputResult",
                "adoptionResult",
                "category",
                "receipt",
                "exitCode",
                "cancellationEvidence",
            ] {
                if let Some(value) = answer.get(field) {
                    audit[field] = value.clone();
                }
            }
            audit["settledAt"] = json!(chrono::Utc::now().to_rfc3339());
            if answer["result"] == "accepted" {
                next["rounds"].as_array_mut().unwrap().last_mut().unwrap()["state"] =
                    json!("running");
                if bundled {
                    next["worker"] = json!({"state":"running"});
                }
            }
            super::write_json(&path, &next, None)?;
            messages::sync_directory(&dir)?;
        }
        answer["operation"] = json!("resume");
        answer["audit"] = json!(event);
        answer["session"] = json!(session);
        if bundled {
            answer["backend"] = json!("bundled");
        } else if herdr {
            answer["backend"] = json!("herdr");
        } else {
            answer["tmux"] = next["tmux"].clone();
        }
        if answer["result"] == "accepted" {
            answer["resumed"] = json!(self.dispatch);
        }
        Ok(answer)
    }
}

fn reconcile(row: &Value, audit: Value, diagnostic: &str) -> Value {
    json!({"error":"lifecycle_reconcile_required","operation":"resume","dispatch":row["dispatch"],
        "attempt":row["attempt"],"round":row["round"],"audit":audit,"result":"uncertain",
        "diagnostic":diagnostic})
}

fn conversation(old: &Value, next: &Value) -> Result<Uuid> {
    let first = old["rounds"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["round"] == 1)
        .context("missing original round")?;
    let original =
        json!({"dispatch":old["dispatch"],"task":first["task"],"report":first["report"]});
    let original_hash = match old["codexBriefHash"].as_str() {
        Some(hash) => hash.to_owned(),
        None => messages::body_hash(&host::worker_brief(&original, host::FIRST_BRIEF)?),
    };
    let known = if old["hostSession"].is_null() {
        None
    } else {
        Some(Uuid::parse_str(
            old["hostSession"]
                .as_str()
                .context("invalid saved session")?,
        )?)
    };
    let home = Path::new(next["codexHome"].as_str().context("missing Codex home")?);
    ensure!(
        home.canonicalize()?.as_path() == home && home.is_dir(),
        "configured Codex home is not a verified directory"
    );
    let cwd = old["cwd"].as_str().context("missing original cwd")?;
    let root = home.join("sessions");
    let mut dirs = vec![root];
    let mut files = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if let Some(id) = rollout_id(&entry.path()) {
                files.push((id, entry.path(), kind.is_file()));
            }
        }
    }
    let mut found = None;
    for (id, path, regular) in &files {
        if !regular || known.is_some_and(|known| known != *id) {
            continue;
        }
        if matches_rollout(path, *id, cwd, &original_hash)? {
            ensure!(
                found.replace(*id).is_none(),
                "more than one rollout matches the original assignment"
            );
        }
    }
    let id = found.context("no rollout matches the original assignment")?;
    ensure!(
        files
            .iter()
            .filter(|(candidate, _, _)| *candidate == id)
            .count()
            == 1,
        "more than one rollout uses the original session ID"
    );
    Ok(id)
}

pub(super) fn rollout_id(path: &Path) -> Option<Uuid> {
    let filename = path
        .file_name()?
        .to_str()?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let id = filename.get(filename.len().checked_sub(36)?..)?;
    let prefix = &filename[..filename.len() - 36];
    (prefix.is_empty() || prefix.ends_with('-')).then_some(())?;
    let uuid = Uuid::parse_str(id).ok()?;
    (uuid.to_string() == id).then_some(uuid)
}

fn matches_rollout(path: &Path, id: Uuid, cwd: &str, brief_hash: &str) -> Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "invalid rollout file");
    let mut lines = BufReader::new(file).take(4 * 1024 * 1024).lines();
    let Some(Ok(meta)) = lines
        .next()
        .transpose()?
        .map(|line| serde_json::from_str::<Value>(&line))
    else {
        return Ok(false);
    };
    let payload = &meta["payload"];
    if meta["type"] != "session_meta"
        || payload["id"] != id.to_string()
        || payload["cwd"] != cwd
        || !matches!(payload["source"].as_str(), Some("cli" | "vscode"))
        || !payload["forked_from_id"].is_null()
        || !payload["parent_thread_id"].is_null()
    {
        return Ok(false);
    }
    let matches_brief = |text: &str| messages::body_hash(text) == brief_hash;
    for line in lines {
        let Ok(record) = serde_json::from_str::<Value>(&line?) else {
            return Ok(false);
        };
        let item = &record["payload"];
        if record["type"] == "event_msg" && item["type"] == "user_message" {
            return Ok(item["message"].as_str().is_some_and(matches_brief));
        }
        if record["type"] == "response_item" && item["type"] == "message" && item["role"] == "user"
        {
            if let Some(text) = item["content"][0]["text"].as_str() {
                if text.starts_with("<environment_context>")
                    || text.starts_with("# AGENTS.md instructions")
                    || text.starts_with("<user_instructions>")
                {
                    continue;
                }
                // Older rollouts can have only the response item.
                return Ok(matches_brief(text));
            }
        }
    }
    Ok(false)
}
