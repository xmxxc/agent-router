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

pub(super) fn read_receipt(path: &Path) -> Option<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let record: Value = serde_json::from_reader(file).ok()?;
    if record
        .get("launchId")
        .is_some_and(|value| value.as_str() != path.file_stem().and_then(|s| s.to_str()))
    {
        return None;
    }
    Some(record)
}

// Both parties must unlink the same one-shot spec before proceeding. Only
// the winner can start or cancel. A recovery marker never contains the spec.
fn revoke_spec(spec: &Path, receipt: &Path) -> Option<Value> {
    fs::remove_file(spec).ok()?;
    let mut reply = failure(
        "definitely-not-sent",
        "worker launch was cancelled before startup",
    );
    reply["category"] = json!("not_started");
    if super::binding::save(receipt, &json!({"category":"not_started"})).is_ok() {
        reply["receipt"] = json!(receipt);
    } else {
        reply["diagnostic"] =
            json!("worker launch was cancelled but the receipt could not be written");
        let marker = spec.with_extension("revoked");
        if private_file(&marker, &()).is_ok() {
            reply["cancellationEvidence"] = json!(marker);
        }
    }
    Some(reply)
}

pub(super) fn cancelled(state: &Path, id: &str) -> bool {
    let Ok(id) = Uuid::parse_str(id) else {
        return false;
    };
    let prefix = state.join("launches").join(id.simple().to_string());
    if !missing(&prefix.with_extension("held")) {
        return false;
    }
    prefix.with_extension("revoked").is_file()
        || read_receipt(&prefix.with_extension("json"))
            .is_some_and(|r| r["category"] == "not_started")
}

fn missing(path: &Path) -> bool {
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
    fn cancellation_record(&self, id: &str) -> Option<Value> {
        Some(
            json!({"category":"not_started","dispatch":self.dispatch,"executionId":self.close_execution?,
            "closeOperationId":self.close_operation_id?, "launchId":id}),
        )
    }

    pub(super) fn cancellation_proof(&self, id: &str) -> Option<(Value, PathBuf)> {
        let record = self.cancellation_record(id)?;
        let prefix = self.state.join("launches").join(id);
        if !missing(&prefix.with_extension("spec")) || !missing(&prefix.with_extension("server")) {
            return None;
        }
        let path = prefix.with_extension("json");
        match read_receipt(&path) {
            Some(receipt) if receipt == record => return Some((record, path)),
            Some(receipt) if receipt["category"] == "unknown" => (),
            None if missing(&path) => (),
            _ => return None,
        }
        let marker = prefix.with_extension("revoked");
        (read_receipt(&marker).as_ref() == Some(&record)).then_some((record, marker))
    }

    pub(super) fn revoke_pending_launch(&self) -> Reply<()> {
        use super::binding::Claim;
        let (id, proof) = match self.claim()? {
            Some(Claim::Registered { id, execution }) => {
                let proof = if self.close_execution == Some(&execution)
                    && self.legacy_execution_id == Some(&id)
                {
                    self.cancellation_record(&id)
                } else {
                    None
                };
                (id, proof)
            }
            Some(Claim::Launch(id)) => (id, None),
            _ => return Ok(()),
        };
        if !super::binding::valid_launch_id(&id) {
            return Err(super::binding::unknown());
        }
        let prefix = self.state.join("launches").join(&id);
        let spec = prefix.with_extension("spec");
        let receipt = prefix.with_extension("json");
        let held = prefix.with_extension("held");
        let Some(record) = proof else {
            if !missing(&held) {
                return Err(super::binding::unknown());
            }
            revoke_spec(&spec, &receipt);
            return Ok(());
        };
        let intent = prefix.with_extension("intent");
        let marker = prefix.with_extension("revoked");
        let owner = json!({"launchId":id,"executionId":self.close_execution,
            "closeOperationId":self.close_operation_id,"dispatch": self.dispatch});
        let unknown = super::binding::unknown;
        if missing(&held) {
            if missing(&spec) {
                return Ok(());
            }
            let initial = read_receipt(&receipt).ok_or_else(unknown)?;
            if !missing(&prefix.with_extension("server"))
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
            || !missing(&prefix.with_extension("server"))
        {
            return Err(unknown());
        }
        if self.cancellation_proof(&id).is_none() {
            let initial = read_receipt(&receipt).ok_or_else(unknown)?;
            if pending_spec(&held, &initial, self.state, self.dispatch).is_none() {
                return Err(unknown());
            }
            super::binding::save(&receipt, &record)
                .or_else(|_| super::binding::save(&marker, &record))
                .map_err(|_| unknown())?;
        }
        fs::remove_file(&held).map_err(|_| unknown())?;
        fs::remove_file(&intent).map_err(|_| unknown())?;
        Ok(())
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
        let io_error = |_| {
            failure(
                "definitely-not-sent",
                "could not prepare private tmux launch",
            )
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(io_error)?;
        let spec_path = dir.join(format!("{id}.spec"));
        let receipt = dir.join(format!("{id}.json"));
        let launcher = env::current_exe().map_err(io_error)?;
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
        let started = (|| {
            private_file(&receipt, &json!({"category":"unknown","launchId":id}))
                .and_then(|_| private_file(&spec_path, &spec))
                .map_err(|_| {
                    failure(
                        "definitely-not-sent",
                        "could not write private launch files",
                    )
                })?;
            self.reserve(&id)?;
            let out = self.run(&args)?;
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
            if let Some(cancelled) = revoke_spec(&spec_path, &receipt) {
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
