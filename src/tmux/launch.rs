use super::{failure, Reply, Target, Tmux};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Deserialize)]
pub(super) struct Launch {
    cwd: PathBuf,
    argv: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    label: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Spec {
    #[serde(default, rename = "launchId")]
    launch_id: Option<String>,
    cwd: PathBuf,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
}

fn private_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(&mut f, value)?;
    f.flush()?;
    Ok(())
}

fn read_private_json(path: &Path) -> Option<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    serde_json::from_reader(file).ok()
}

pub(crate) fn read_receipt(path: &Path) -> Option<Value> {
    let record = read_private_json(path)?;
    if record
        .get("launchId")
        .is_some_and(|value| value.as_str() != path.file_stem().and_then(|s| s.to_str()))
    {
        return None;
    }
    Some(record)
}

pub(crate) const UNKNOWN_CLOSE_IDENTITY: &str =
    "original worker identity is unknown and its stop is unconfirmed";

pub(crate) fn remember_unknown_close_identity(
    state: &Path,
    launch: &str,
    dispatch: &str,
) -> Result<()> {
    let path = state.join("launches").join(launch);
    let marker = path.with_extension("identity-unknown");
    if missing(&marker)
        && read_receipt(&path.with_extension("json"))
            .is_some_and(|receipt| receipt.get("processStart") == Some(&Value::Null))
    {
        // An older live supervisor can replace its started receipt with an
        // exit receipt that omits the field while descendants are still alive.
        super::binding::save(&marker, &json!({"launchId":launch,"dispatch":dispatch}))?;
    }
    Ok(())
}

pub(crate) fn close_worker_tree_ready(
    state: &Path,
    launch: &str,
    dispatch: &str,
    identity: impl FnOnce() -> Option<(String, u64)>,
) -> bool {
    let path = state.join("launches").join(launch);
    let Some(receipt) = read_receipt(&path.with_extension("json")) else {
        return false;
    };
    let marker = path.with_extension("identity-unknown");
    let previously_unknown = !missing(&marker);
    if !receipt.is_object()
        || (previously_unknown
            && read_receipt(&marker) != Some(json!({"launchId":launch,"dispatch":dispatch})))
    {
        return false;
    }
    let process_start = receipt.get("processStart");
    let explicitly_unknown = process_start == Some(&Value::Null);
    if !explicitly_unknown && !previously_unknown {
        return process_start.is_none_or(Value::is_string);
    }
    let Some((attempt, round)) = identity() else {
        return false;
    };
    let round = round.to_string();
    receipt["category"] == "process_exited"
        && receipt["treeTracked"] == true
        && receipt["launchId"] == launch
        && receipt["exitCode"].as_u64().is_some_and(|code| code <= 255)
        && (!explicitly_unknown || (receipt["attempt"] == attempt && receipt["round"] == round))
        && read_receipt(&path.with_extension("stopped")).is_some_and(|stop| {
            stop["writerTreeStopped"] == true
                && stop["dispatch"] == dispatch
                && stop["launchId"] == launch
                && stop["attempt"] == attempt
                && stop["round"] == round
        })
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
pub(super) enum SubmissionPhase {
    Prepared,
    InFlight,
    Returned,
    LegacyAbsent,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Submission {
    pub(super) launch_id: String,
    dispatch: String,
    execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    round: Option<u64>,
    pub(super) socket: Option<PathBuf>,
    pub(super) origin: Option<super::binding::ServerSession>,
    pub(super) phase: SubmissionPhase,
}

fn save_cancellation(spec: &Path, receipt: &Path, proof: &Value) -> Reply<PathBuf> {
    if super::binding::save(receipt, proof).is_ok() {
        return Ok(receipt.to_owned());
    }
    let marker = spec.with_extension("revoked");
    super::binding::save(&marker, proof).map_err(|_| super::binding::unknown())?;
    Ok(marker)
}

// Only the winner of removing the one-shot descriptor can start or cancel.
fn revoke_spec(spec: &Path, receipt: &Path, proof: &Value) -> Option<Value> {
    fs::remove_file(spec).ok()?;
    let mut reply = failure(
        "definitely-not-sent",
        "worker launch was cancelled before startup",
    );
    reply["category"] = json!("not_started");
    match save_cancellation(spec, receipt, proof) {
        Ok(path) if path == receipt => reply["receipt"] = json!(path),
        Ok(path) => reply["cancellationEvidence"] = json!(path),
        Err(_) => {
            reply["diagnostic"] =
                json!("worker launch was cancelled but its proof could not be written");
        }
    }
    Some(reply)
}

pub(super) fn missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|e| e.kind() == ErrorKind::NotFound)
}

// Old launches have no embedded ID on either file. New launches tag both.
fn pending_spec(path: &Path, receipt: &Value, state: &Path, dispatch: &str) -> Option<Spec> {
    let value = read_receipt(path)?;
    if receipt["category"] != "unknown" || value.get("launchId") != receipt.get("launchId") {
        return None;
    }
    let spec: Spec = serde_json::from_value(value).ok()?;
    (spec.env.get("WFC_STATE")?.as_str() == state.to_str()?
        && spec.env.get("WFC_DISPATCH")? == dispatch)
        .then_some(spec)
}

impl Tmux<'_> {
    pub(super) fn launch_path(&self, id: &str, extension: &str) -> PathBuf {
        self.state
            .join("launches")
            .join(id)
            .with_extension(extension)
    }

    pub(super) fn require_close_worker_tree(&self, execution: Option<&str>) -> Reply<()> {
        use super::binding::{valid_launch_id, Claim};
        let selected = match (execution, self.legacy_execution_id) {
            (Some(execution), Some(launch)) => {
                Some((launch.to_owned(), Some(execution.to_owned())))
            }
            (_, Some(selector)) => self
                .observed_launch(selector, false)?
                .or_else(|| valid_launch_id(selector).then(|| selector.to_owned()))
                .map(|id| (id, None)),
            _ => match self.claim()? {
                Some(Claim::Registered { id, execution }) if valid_launch_id(&id) => {
                    Some((id, Some(execution)))
                }
                Some(
                    Claim::Launch(id)
                    | Claim::Released(Some(id))
                    | Claim::Observed {
                        receipt: Some(id), ..
                    },
                ) if valid_launch_id(&id) => Some((id, None)),
                _ => None,
            },
        };
        if let Some((id, execution)) = selected {
            remember_unknown_close_identity(self.state, &id, self.dispatch)
                .map_err(|_| failure("uncertain", UNKNOWN_CLOSE_IDENTITY))?;
            let ready = close_worker_tree_ready(self.state, &id, self.dispatch, || {
                let attempt = execution.as_deref()?;
                let submission = self.submission(&id, Some(attempt)).ok()?;
                let round = submission.round.or_else(|| {
                    crate::workflow::registered_tmux_round(self.state, self.dispatch, attempt, &id)
                })?;
                Some((attempt.to_owned(), round))
            });
            if !ready
                && !self.legacy_worker_cancelled(&id)
                && self.cancellation(&id, execution.as_deref())?.is_none()
            {
                return Err(failure("uncertain", UNKNOWN_CLOSE_IDENTITY));
            }
        }
        Ok(())
    }

    pub(super) fn submission(&self, id: &str, execution: Option<&str>) -> Reply<Submission> {
        let unknown = super::binding::unknown;
        let value = read_receipt(&self.launch_path(id, "submission")).ok_or_else(unknown)?;
        let submission: Submission = serde_json::from_value(value).map_err(|_| unknown())?;
        if submission.launch_id != id
            || submission.dispatch != self.dispatch
            || submission.execution_id.as_deref() != execution
            || !super::binding::same_selection(submission.socket.as_deref(), self.socket.as_deref())
        {
            return Err(unknown());
        }
        Ok(submission)
    }

    pub(super) fn legacy_worker_cancelled(&self, id: &str) -> bool {
        if !super::binding::valid_launch_id(id)
            || ["spec", "held", "intent", "server", "session", "cleanup"]
                .iter()
                .any(|ext| !missing(&self.launch_path(id, ext)))
        {
            return false;
        }
        let receipt_path = self.launch_path(id, "json");
        let marker = self.launch_path(id, "revoked");
        match read_private_json(&receipt_path) {
            Some(record) if record == json!({"category":"not_started"}) => missing(&marker),
            Some(record) if record == json!({"category":"unknown"}) => {
                read_receipt(&marker) == Some(Value::Null)
            }
            None if missing(&receipt_path)
                || fs::symlink_metadata(&receipt_path).is_ok_and(|m| m.is_dir()) =>
            {
                read_receipt(&marker) == Some(Value::Null)
            }
            _ => false,
        }
    }

    pub(super) fn confirm_legacy_absence(
        &self,
        id: &str,
        execution: Option<&str>,
        reachable: bool,
    ) -> Reply<()> {
        let unknown = super::binding::unknown;
        let path = self.launch_path(id, "submission");
        if !missing(&path) {
            return if self.submission(id, execution)?.phase == SubmissionPhase::LegacyAbsent {
                Ok(())
            } else {
                Err(unknown())
            };
        }
        if reachable || !self.legacy_endpoint_is_empty(id)? {
            return Err(unknown());
        }
        // Store the independently checked absence as history. The legacy
        // worker receipt is left intact and supplies no close-operation fact.
        let submission = Submission {
            launch_id: id.to_owned(),
            dispatch: self.dispatch.to_owned(),
            execution_id: execution.map(str::to_owned),
            round: None,
            socket: self.socket.clone(),
            origin: None,
            phase: SubmissionPhase::LegacyAbsent,
        };
        super::binding::save(&path, &submission).map_err(|_| unknown())
    }

    fn cancellation_record(
        &self,
        id: &str,
        execution: Option<&str>,
        operation: Option<&str>,
    ) -> Value {
        json!({"category":"not_started", "dispatch":self.dispatch, "executionId":execution,
            "closeOperationId":operation, "launchId":id})
    }

    // A host server record means it acquired the descriptor. It can never
    // coexist with a valid pre-start cancellation for that launch.
    pub(super) fn cancellation(
        &self,
        id: &str,
        execution: Option<&str>,
    ) -> Reply<Option<(Value, PathBuf)>> {
        if !super::binding::valid_launch_id(id) {
            return Err(super::binding::unknown());
        }
        let receipt_path = self.launch_path(id, "json");
        let marker_path = self.launch_path(id, "revoked");
        let receipt = read_private_json(&receipt_path);
        let marker = read_receipt(&marker_path);
        let unknown = super::binding::unknown;
        let (record, path) = match receipt {
            Some(record) if record["category"] == "not_started" && missing(&marker_path) => {
                (record, receipt_path)
            }
            Some(record)
                if record["category"] == "unknown"
                    && !missing(&marker_path)
                    && record.get("launchId").is_none_or(|tag| tag == id) =>
            {
                (marker.ok_or_else(unknown)?, marker_path)
            }
            None if !missing(&marker_path)
                && (missing(&receipt_path)
                    || fs::symlink_metadata(&receipt_path).is_ok_and(|m| m.is_dir())) =>
            {
                (marker.ok_or_else(unknown)?, marker_path)
            }
            Some(record) if record["category"] == "not_started" || !missing(&marker_path) => {
                return Err(unknown())
            }
            None if !missing(&marker_path) => return Err(unknown()),
            _ => return Ok(None),
        };
        let operation = record["closeOperationId"].as_str();
        if !missing(&self.launch_path(id, "spec"))
            || !missing(&self.launch_path(id, "server"))
            || record != self.cancellation_record(id, execution, operation)
        {
            return Err(unknown());
        }
        if let Some(operation) = operation {
            if execution.is_none()
                || !matches!(
                    self.previous_close(id, operation)?,
                    Some(super::CloseOperation::Pending | super::CloseOperation::Confirmed(_))
                )
            {
                return Err(unknown());
            }
        }
        Ok(Some((record, path)))
    }

    pub(super) fn unverified_released_cancellation(
        &self,
        id: &str,
        execution: Option<&str>,
    ) -> Reply<Option<(Value, PathBuf)>> {
        // R7 could release solely on worker cancellation, before recording
        // any evidence about the session. Its worker fact is still valid.
        if !super::binding::valid_launch_id(id) || self.submission(id, execution).is_ok() {
            return Ok(None);
        }
        let fact = self.cancellation(id, execution)?;
        // A closed native R7 row with neither its worker fact nor its server
        // record has no independent proof of how its session ended.
        if fact.is_none() && execution.is_some() && !self.has_original_server_record(id) {
            return Err(super::binding::unknown());
        }
        Ok(fact)
    }

    pub(super) fn cancellation_proof(&self, id: &str) -> Reply<Option<(Value, PathBuf)>> {
        let (Some(execution), Some(operation)) = (self.close_execution, self.close_operation_id)
        else {
            return Ok(None);
        };
        if self.legacy_worker_cancelled(id) {
            return Ok(None);
        }
        Ok(self
            .cancellation(id, Some(execution))?
            .filter(|(record, _)| record["closeOperationId"] == operation))
    }

    pub(super) fn revoke_pending_launch(&self) -> Reply<()> {
        use super::binding::Claim;
        let (id, execution) = match self.claim()? {
            Some(Claim::Registered { id, execution }) => (id, Some(execution)),
            Some(Claim::Launch(id)) => (id, None),
            _ => return Ok(()),
        };
        let unknown = super::binding::unknown;
        if !super::binding::valid_launch_id(&id) {
            return Err(unknown());
        }
        let execution = execution.as_deref();
        let operation = if self.close_execution.is_some() {
            if self.close_execution != execution || self.legacy_execution_id != Some(&id) {
                return Ok(());
            }
            self.close_operation_id
        } else {
            None
        };
        let spec = self.launch_path(&id, "spec");
        let receipt = self.launch_path(&id, "json");
        let held = self.launch_path(&id, "held");
        let intent = self.launch_path(&id, "intent");
        let record = self.cancellation_record(&id, execution, operation);
        if operation.is_none() {
            if !missing(&held) {
                return Err(unknown());
            }
            if !missing(&spec) {
                let initial = read_receipt(&receipt).ok_or_else(unknown)?;
                if !missing(&self.launch_path(&id, "server"))
                    || pending_spec(&spec, &initial, self.state, self.dispatch).is_none()
                {
                    return Err(unknown());
                }
                revoke_spec(&spec, &receipt, &record);
            }
            return Ok(());
        }
        let owner = json!({"launchId":id,"executionId":execution,
            "closeOperationId":operation,"dispatch": self.dispatch});
        if missing(&held) {
            if missing(&spec) {
                return Ok(());
            }
            let initial = read_receipt(&receipt).ok_or_else(unknown)?;
            if !missing(&self.launch_path(&id, "server"))
                || pending_spec(&spec, &initial, self.state, self.dispatch).is_none()
            {
                return Err(unknown());
            }
            self.record_close(super::CloseOperation::Pending)?;
            super::binding::save(&intent, &owner).map_err(|_| unknown())?;
            // The host must unlink spec before it can start. Winning this
            // rename also leaves a private, recoverable witness of the win.
            match fs::rename(&spec, &held) {
                Ok(()) => (),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    self.record_close(super::CloseOperation::NotExecuted)?;
                    return Ok(());
                }
                Err(_) => return Err(unknown()),
            }
        }
        if read_receipt(&intent).as_ref() != Some(&owner)
            || !missing(&spec)
            || !missing(&self.launch_path(&id, "server"))
        {
            return Err(unknown());
        }
        if self.cancellation_proof(&id)?.is_none() {
            let initial = read_receipt(&receipt).ok_or_else(unknown)?;
            if pending_spec(&held, &initial, self.state, self.dispatch).is_none() {
                return Err(unknown());
            }
            save_cancellation(&spec, &receipt, &record)?;
        }
        fs::remove_file(&held).map_err(|_| unknown())?;
        fs::remove_file(&intent).map_err(|_| unknown())?;
        Ok(())
    }

    fn creation_hook(&self, launcher: &Path, id: &str) -> Reply<String> {
        // The two path arguments pass through tmux formats and then a shell.
        let quote = |path: &Path| {
            path.to_str()
                .map(|path| format!("'{}'", path.replace('#', "##").replace('\'', "'\"'\"'")))
                .ok_or_else(super::binding::unknown)
        };
        Ok(format!("{} --tmux-record-session {} {id} '#{{pid}}' '#{{session_id}}' '#{{pane_id}}' #{{q:socket_path}} '#{{m:*--tmux-host {id},#{{pane_start_command}}}}' #{{q:WFC_TMUX_LAUNCH}}",
            quote(launcher)?, quote(self.state)?))
    }

    pub(super) fn create(&self, launch: Launch) -> Reply<(Target, PathBuf)> {
        if self.target()?.is_some() {
            return Err(failure("refused", "named tmux session already exists"));
        }
        let bad = || {
            failure(
                "definitely-not-sent",
                "invalid worker cwd, argv or environment",
            )
        };
        let cwd = fs::canonicalize(launch.cwd).map_err(|_| bad())?;
        if !cwd.is_dir()
            || launch.argv.first().is_none_or(String::is_empty)
            || launch.argv.iter().any(|s| s.contains('\0'))
            || launch
                .env
                .iter()
                .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
        {
            return Err(bad());
        }
        let mut environment = BTreeMap::new();
        for (k, v) in env::vars_os() {
            let k = k.into_string().map_err(|_| bad())?;
            if !crate::workflow::controller::is_credential(&k) {
                environment.insert(k, v.into_string().map_err(|_| bad())?);
            }
        }
        for (k, v) in launch.env {
            if !crate::workflow::controller::is_credential(&k) || k == "WFC_WORKER_CALLBACK" {
                environment.insert(k, v);
            }
        }
        let state = self.state.to_str().ok_or_else(bad)?;
        environment.insert("WFC_STATE".into(), state.into());
        environment.insert("WFC_DISPATCH".into(), self.dispatch.into());
        let id = Uuid::new_v4().simple().to_string();
        let spec = Spec {
            launch_id: Some(id.clone()),
            cwd: cwd.clone(),
            argv: launch.argv,
            env: environment,
        };
        let dir = self.state.join("launches");
        let io_error = || {
            failure(
                "definitely-not-sent",
                "could not prepare private tmux launch",
            )
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|_| io_error())?;
        let spec_path = dir.join(format!("{id}.spec"));
        let receipt = dir.join(format!("{id}.json"));
        let launcher = env::current_exe().map_err(|_| io_error())?;
        let state_env = format!("WFC_STATE={state}");
        let launch_env = format!("WFC_TMUX_LAUNCH={id}");
        let dispatch_env = format!("WFC_DISPATCH={}", self.dispatch);
        let mut args = vec![
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_id} #{pane_id} #{pid}",
            "-s",
            &self.session,
        ];
        if let Some(label) = launch.label.as_deref().filter(|s| !s.is_empty()) {
            args.extend(["-n", label]);
        }
        args.extend([
            "-c",
            cwd.to_str().ok_or_else(bad)?,
            "-e",
            &state_env,
            "-e",
            &dispatch_env,
            "-e",
            &launch_env,
            "--",
            launcher.to_str().ok_or_else(bad)?,
            "--tmux-host",
            &id,
        ]);
        let unknown = || failure("uncertain", "tmux host startup not confirmed");
        let hook_target = format!("={}:", self.session);
        let hook = self.creation_hook(&launcher, &id)?;
        let tail = ["run-shell", "-t", &hook_target, &hook];
        let started = (|| {
            private_file(&receipt, &json!({"category":"unknown","launchId":id}))
                .and_then(|_| private_file(&spec_path, &spec))
                .map_err(|_| {
                    failure(
                        "definitely-not-sent",
                        "could not write private launch files",
                    )
                })?;
            let path = self.launch_path(&id, "submission");
            let mut submission = Submission {
                launch_id: id.clone(),
                dispatch: self.dispatch.to_owned(),
                execution_id: self.new_execution_id.map(str::to_owned),
                round: spec
                    .env
                    .get("WFC_ROUND")
                    .and_then(|round| round.parse::<u64>().ok())
                    .filter(|round| *round > 0),
                socket: self.socket.clone(),
                origin: self.launch_server(),
                phase: SubmissionPhase::Prepared,
            };
            super::binding::save(&path, &submission).map_err(|_| io_error())?;
            self.reserve(&id)?;
            if self
                .socket
                .as_deref()
                .and_then(Path::parent)
                .is_some_and(missing)
            {
                return Err(failure(
                    "definitely-not-sent",
                    "tmux socket directory does not exist",
                ));
            }
            // Confirm the socket before marking submission in flight. A
            // failed preflight cannot have issued new-session.
            let pinned_socket = submission
                .origin
                .as_ref()
                .map(|origin| self.pin_submission(origin))
                .transpose()?;
            if let Some(audit) = self.handoff_audit {
                let execution = self.new_execution_id.ok_or_else(bad)?;
                let previous = crate::workflow::handoff::require_launch(
                    self.state,
                    self.dispatch,
                    execution,
                    audit,
                )
                .map_err(|error| failure("definitely-not-sent", &format!("{error:#}")))?;
                self.require_handoff_server(&previous, submission.origin.as_ref())?;
            }
            submission.phase = SubmissionPhase::InFlight;
            super::binding::save(&path, &submission).map_err(|_| io_error())?;
            let out = if let Some(socket) = &pinned_socket {
                let pinned_path = super::binding::fd_path(socket);
                let mut pinned = vec!["-N", "-S", &pinned_path];
                pinned.extend_from_slice(&args);
                self.run_with_tail(&pinned, &tail)
            } else {
                self.run_with_tail(&args, &tail)
            };
            submission.phase = match &out {
                Err(e) if e["result"] == "definitely-not-sent" => SubmissionPhase::Prepared,
                Err(_) => SubmissionPhase::InFlight,
                Ok(_) => SubmissionPhase::Returned,
            };
            super::binding::save(&path, &submission).map_err(|_| unknown())?;
            let out = out?;
            if !out.status.success() {
                return Err(unknown());
            }
            let binding = String::from_utf8_lossy(&out.stdout);
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let evidence: Value = read_receipt(&receipt).unwrap_or(Value::Null);
                match evidence["category"].as_str() {
                    Some("started") => {
                        // Give an immediately exiting process a chance to publish its exit status.
                        sleep(Duration::from_millis(300));
                        let evidence: Value = read_receipt(&receipt).ok_or_else(unknown)?;
                        if evidence["category"] != "started" {
                            return Err(launch_failure(evidence));
                        }
                        let started_unverified = || {
                            let mut e = unknown();
                            e["launchResult"] = json!("accepted");
                            e
                        };
                        let target = self.owned().map_err(|_| started_unverified())?;
                        if binding.trim_end()
                            != format!(
                                "{} {} {}",
                                target.session,
                                target
                                    .worker_pane
                                    .as_deref()
                                    .ok_or_else(started_unverified)?,
                                target.server_pid
                            )
                        {
                            return Err(started_unverified());
                        }
                        let latest = read_receipt(&receipt).ok_or_else(started_unverified)?;
                        if latest["category"] != "started" {
                            return Err(launch_failure(latest));
                        }
                        return Ok(target);
                    }
                    Some("unknown") | None if Instant::now() < deadline => {
                        sleep(Duration::from_millis(20))
                    }
                    Some("unknown") | None => return Err(unknown()),
                    _ => return Err(launch_failure(evidence)),
                }
            }
        })();
        started.map(|t| (t, receipt.clone())).map_err(|mut e| {
            if let Some(cancelled) = revoke_spec(
                &spec_path,
                &receipt,
                &self.cancellation_record(&id, self.new_execution_id, None),
            ) {
                return cancelled;
            }
            // The worker can finish after the started check but before tmux
            // confirms its pane. Prefer this launch's published terminal facts.
            if let Some(final_receipt) = read_receipt(&receipt).filter(|r| {
                matches!(
                    r["category"].as_str(),
                    Some(
                        "process_exited"
                            | "executable_missing"
                            | "permission_denied"
                            | "spawn_failed"
                    )
                )
            }) {
                let mut terminal = launch_failure(final_receipt);
                if let Some(started) = e.get("launchResult") {
                    terminal["launchResult"] = started.clone();
                }
                e = terminal;
            }
            e["receipt"] = json!(receipt);
            e
        })
    }
}

fn launch_failure(evidence: Value) -> Value {
    let mut outcome = match evidence["category"].as_str() {
        Some("executable_missing" | "permission_denied" | "spawn_failed") => failure(
            "definitely-not-sent",
            "could not start the worker executable",
        ),
        _ => failure("uncertain", "tmux host startup not confirmed"),
    };
    outcome["category"] = evidence["category"].clone();
    if let Some(code) = evidence.get("exitCode") {
        outcome["exitCode"] = code.clone();
    }
    outcome
}

/// Private entry point executed directly by tmux. The only argv payload is the
/// one-shot launch ID. Worker argv and credentials travel in a mode-0600 file.
pub(crate) fn run_host(id: &Path) -> Result<ExitCode> {
    let id = id.to_str().context("invalid launch ID")?;
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid launch ID"
    );
    let state = PathBuf::from(env::var_os("WFC_STATE").context("missing tmux state")?);
    let dir = state.join("launches");
    let spec_path = dir.join(format!("{id}.spec"));
    let receipt = dir.join(format!("{id}.json"));
    let initial = read_receipt(&receipt).context("missing initial launch receipt")?;
    let dispatch = env::var("WFC_DISPATCH").context("missing dispatch")?;
    let spec = pending_spec(&spec_path, &initial, &state, &dispatch)
        .context("unverified launch specification")?;
    let submission = dir.join(format!("{id}.submission"));
    if !missing(&submission) {
        // Let the tmux create queue identify its session before a fast worker
        // can exit and remove the target of that queue.
        let session = dir.join(format!("{id}.session"));
        let host = super::binding::host_session()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if super::binding::host_matches_created_session(&session, id, &host) {
                break;
            }
            ensure!(
                !missing(&spec_path) && Instant::now() < deadline,
                "tmux session creation was not recorded"
            );
            sleep(Duration::from_millis(10));
        }
    }
    let startup = crate::workflow::startup::Startup::hosted(&spec.env, id);
    fs::remove_file(spec_path)?;
    super::binding::record_server(&dir, id)?;
    let mut cmd = Command::new(spec.argv.first().context("empty launch argv")?);
    // Track orphans before starting the worker. A direct child's exit alone
    // cannot authorize a cross-host replacement.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0
            && unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } != libc::SIG_ERR,
        "cannot supervise the tmux worker process tree"
    );
    cmd.args(&spec.argv[1..])
        .current_dir(spec.cwd)
        .env_clear()
        .envs(&spec.env);
    for key in ["TMUX", "TMUX_PANE", "TERM"] {
        if let Some(value) = env::var_os(key) {
            cmd.env(key, value);
        }
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            if let Some(startup) = &startup {
                startup.spawn_failed(err.kind());
            }
            let category = match err.kind() {
                ErrorKind::NotFound => "executable_missing",
                ErrorKind::PermissionDenied => "permission_denied",
                _ => "spawn_failed",
            };
            super::binding::save(&receipt, &json!({"category":category}))?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let process_start = wfc_herdr::Process::observe_unreaped_child(&child)
        .ok()
        .flatten()
        .map(|process| process.creation);
    let identity_unknown = process_start.is_none();
    let watching = startup.as_ref().map(|startup| startup.watch(&child));
    // Ctrl-C addresses the terminal foreground group. Let the worker handle it
    // and keep its receipt writer alive to record the actual exit.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        // Keep observing descendants when tmux closes the old terminal.
        // Set this after spawn so the worker retains its own HUP behavior.
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let started_receipt = json!({"category":"started","pid":child.id(),"processStart":process_start,
        "launchId":id,"treeTracked":true});
    let (status, _, started) = crate::workflow::startup::wait_host(&mut child, || {
        super::binding::save(&receipt, &started_receipt)
    })?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    let mut terminal =
        json!({"category":"process_exited","exitCode":code,"launchId":id,"treeTracked":true});
    if identity_unknown {
        // Direct exit can be published before adopted descendants have stopped.
        terminal["processStart"] = Value::Null;
        terminal["attempt"] = json!(spec.env.get("WFC_ATTEMPT"));
        terminal["round"] = json!(spec.env.get("WFC_ROUND"));
    }
    if let Some(watching) = watching {
        terminal["startupExit"] = watching.tmux_exit_evidence(status);
    }
    super::binding::save(&receipt, &terminal)?;
    started?;
    wait_for_worker_tree()?;
    super::binding::save(
        &dir.join(format!("{id}.stopped")),
        &json!({"launchId":id,"dispatch":dispatch,"attempt":spec.env.get("WFC_ATTEMPT"),
            "round":spec.env.get("WFC_ROUND"),"writerTreeStopped":true}),
    )?;
    Ok(ExitCode::from(code as u8))
}

fn wait_for_worker_tree() -> Result<()> {
    loop {
        if unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::__WALL) } > 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(()),
            Some(libc::EINTR) => (),
            _ => return Err(error).context("wait for tmux worker descendants"),
        }
    }
}
