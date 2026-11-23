use super::{failure, Reply, Target, Tmux};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
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
    cwd: PathBuf,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
}

pub(super) fn router_credential(key: &str) -> bool {
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

pub(super) fn read_receipt(path: &Path) -> Option<Value> {
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
    prefix.with_extension("revoked").is_file()
        || read_receipt(&prefix.with_extension("json"))
            .is_some_and(|r| r["category"] == "not_started")
}

impl Tmux<'_> {
    pub(super) fn revoke_pending_launch(&self) -> Reply<()> {
        use super::binding::Claim;
        if let Some(Claim::Launch(id) | Claim::Registered { id, .. }) = self.claim()? {
            let id =
                Uuid::parse_str(&id).map_err(|_| failure("uncertain", "invalid tmux launch ID"))?;
            let prefix = self.state.join("launches").join(id.simple().to_string());
            revoke_spec(
                &prefix.with_extension("spec"),
                &prefix.with_extension("json"),
            );
        }
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
        let spec = Spec {
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
        let id = Uuid::new_v4().simple().to_string();
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
            private_file(&receipt, &json!({"category":"unknown"}))
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
    let dir =
        PathBuf::from(env::var_os("WFC_STATE").context("missing tmux state")?).join("launches");
    let spec_path = dir.join(format!("{id}.spec"));
    let receipt = dir.join(format!("{id}.json"));
    let spec: Spec = serde_json::from_reader(File::open(&spec_path)?)?;
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
    let started = super::binding::save(&receipt, &json!({"category":"started","pid":child.id()}));
    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    super::binding::save(
        &receipt,
        &json!({"category":"process_exited","exitCode":code}),
    )?;
    started?;
    Ok(ExitCode::from(code as u8))
}
