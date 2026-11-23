use super::{failure, Reply, Target, Tmux};
use crate::host_launch::{self, read_receipt};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
pub(super) struct Launch {
    cwd: PathBuf,
    argv: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    label: Option<String>,
}

impl Tmux<'_> {
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
        let files = host_launch::prepare(self.state, self.dispatch, &cwd, launch.argv, launch.env)
            .map_err(|_| bad())?;
        let id = &files.id;
        let receipt = &files.receipt;
        let state = self.state.to_str().ok_or_else(bad)?;
        let io_error = |_| {
            failure(
                "definitely-not-sent",
                "could not prepare private tmux launch",
            )
        };
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
            id,
        ]);
        let unknown = || failure("uncertain", "tmux host startup not confirmed");
        let started = (|| {
            self.reserve(id)?;
            let out = match self.run(&args) {
                Err(e) if e["result"] == "definitely-not-sent" => {
                    self.forget()?;
                    return Err(e);
                }
                result => result?,
            };
            if !out.status.success() {
                return Err(unknown());
            }
            let binding = String::from_utf8_lossy(&out.stdout);
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let evidence: Value = read_receipt(receipt).unwrap_or(Value::Null);
                match evidence["category"].as_str() {
                    Some("started") => {
                        // Give an immediately exiting process a chance to publish its exit status.
                        sleep(Duration::from_millis(300));
                        let evidence: Value = read_receipt(receipt).ok_or_else(unknown)?;
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
                            != format!("{} {} {}", target.session, target.pane, target.server_pid)
                        {
                            return Err(started_unverified());
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
        // The launcher also unlinks this one-shot environment file before spawning.
        started.map(|t| (t, receipt.clone())).map_err(|mut e| {
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
    let state = PathBuf::from(env::var_os("WFC_STATE").context("missing tmux state")?);
    let spec_path = host_launch::spec_path(&state, id)?;
    host_launch::supervise(&spec_path, host_launch::Terminal::Tmux, || {
        super::binding::record_server(
            &state.join("launches"),
            id.to_str().context("invalid launch ID")?,
        )
    })
}
