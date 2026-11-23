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

pub(crate) fn router_credential(key: &str) -> bool {
    [
        "WFC_CONTROLLER_TOKEN",
        "WFC_OWNER_TOKEN",
        "WFC_WORKER_CALLBACK",
    ]
    .iter()
    .any(|name| key.eq_ignore_ascii_case(name))
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

pub(super) fn read_receipt(path: &Path) -> Option<Value> {
    let record = read_private_json(path)?;
    if record
        .get("launchId")
        .is_some_and(|value| value.as_str() != path.file_stem().and_then(|s| s.to_str()))
    {
        return None;
    }
    Some(record)
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
pub(super) enum SubmissionPhase {
    Prepared,
    InFlight,
    Returned,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Submission {
    pub(super) launch_id: String,
    dispatch: String,
    execution_id: Option<String>,
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

    pub(super) fn submission(&self, id: &str, execution: Option<&str>) -> Reply<Submission> {
        let unknown = super::binding::unknown;
        let value = read_receipt(&self.launch_path(id, "submission")).ok_or_else(unknown)?;
        let submission: Submission = serde_json::from_value(value).map_err(|_| unknown())?;
        if submission.launch_id != id
            || submission.dispatch != self.dispatch
            || submission.execution_id.as_deref() != execution
            || submission.socket != self.socket
        {
            return Err(unknown());
        }
        Ok(submission)
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
            if !router_credential(&k) {
                environment.insert(k, v.into_string().map_err(|_| bad())?);
            }
        }
        for (k, v) in launch.env {
            if !router_credential(&k) || k == "WFC_WORKER_CALLBACK" {
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
            submission.phase = SubmissionPhase::InFlight;
            super::binding::save(&path, &submission).map_err(|_| io_error())?;
            let out = if let Some(origin) = &submission.origin {
                // A launch sent to a pinned server cannot be redirected to a replacement.
                let socket = self.pin_submission(origin)?;
                let pinned_path = super::binding::fd_path(&socket);
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
    fs::remove_file(spec_path)?;
    super::binding::record_server(&dir, id)?;
    let mut cmd = Command::new(spec.argv.first().context("empty launch argv")?);
    cmd.args(&spec.argv[1..])
        .current_dir(spec.cwd)
        .env_clear()
        .envs(spec.env);
    for key in ["TMUX", "TMUX_PANE", "TERM"] {
        if let Some(value) = env::var_os(key) {
            cmd.env(key, value);
        }
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let category = match err.kind() {
                ErrorKind::NotFound => "executable_missing",
                ErrorKind::PermissionDenied => "permission_denied",
                _ => "spawn_failed",
            };
            super::binding::save(&receipt, &json!({"category":category}))?;
            return Ok(ExitCode::FAILURE);
        }
    };
    // Ctrl-C addresses the terminal foreground group. Let the worker handle it
    // and keep its receipt writer alive to record the actual exit.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    let started = super::binding::save(
        &receipt,
        &json!({"category":"started","pid":child.id(),"launchId":id}),
    );
    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    super::binding::save(
        &receipt,
        &json!({"category":"process_exited","exitCode":code,"launchId":id}),
    )?;
    started?;
    Ok(ExitCode::from(code as u8))
}
