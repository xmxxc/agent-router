//! Private launch files and worker process receipts for Herdr on Linux.
use crate::tmux::launch::router_credential;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt},
    process::ExitStatusExt,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{collections::BTreeMap, env, fs};
use uuid::Uuid;
#[derive(Serialize, Deserialize)]
struct Spec {
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

pub(crate) fn read_receipt(path: &Path) -> Option<Value> {
    serde_json::from_reader(File::open(path).ok()?).ok()
}

pub(crate) struct Prepared {
    pub id: String,
    pub spec_path: PathBuf,
    pub receipt: PathBuf,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.spec_path);
    }
}

pub(crate) fn prepare(
    state: &Path,
    dispatch: &str,
    cwd: &Path,
    argv: Vec<String>,
    extra_env: BTreeMap<String, String>,
) -> Result<Prepared> {
    let cwd = fs::canonicalize(cwd).map_err(|_| anyhow::anyhow!("invalid worker launch"))?;
    if !cwd.is_dir()
        || argv.first().is_none_or(String::is_empty)
        || argv.iter().any(|s| s.contains('\0'))
        || extra_env
            .iter()
            .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
    {
        anyhow::bail!("invalid worker cwd, argv or environment");
    }
    let mut environment = BTreeMap::new();
    for (k, v) in env::vars_os() {
        let k = k
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid worker launch"))?;
        if !router_credential(&k) {
            environment.insert(
                k,
                v.into_string()
                    .map_err(|_| anyhow::anyhow!("invalid worker launch"))?,
            );
        }
    }
    for (k, v) in extra_env {
        if !router_credential(&k) || k == "WFC_WORKER_CALLBACK" {
            environment.insert(k, v);
        }
    }
    let state_name = state.to_str().context("invalid state path")?;
    environment.insert("WFC_STATE".into(), state_name.into());
    environment.insert("WFC_DISPATCH".into(), dispatch.into());
    let spec = Spec {
        cwd,
        argv,
        env: environment,
    };
    let dir = state.join("launches");
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let id = Uuid::new_v4().simple().to_string();
    let spec_path = dir.join(format!("{id}.spec"));
    let receipt = dir.join(format!("{id}.json"));

    let files = Prepared {
        id,
        spec_path,
        receipt,
    };
    private_file(&files.receipt, &json!({"category":"unknown"}))?;
    private_file(&files.spec_path, &spec)?;
    Ok(files)
}

pub(crate) fn spec_path(state: &Path, id: &Path) -> Result<PathBuf> {
    let name = id.to_str().context("invalid launch ID")?;
    anyhow::ensure!(
        name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid launch ID"
    );
    Ok(state.join("launches").join(format!("{name}.spec")))
}

pub(crate) fn supervise(spec_path: &Path) -> Result<ExitCode> {
    let receipt = spec_path.with_extension("json");
    let spec: Spec = serde_json::from_reader(File::open(spec_path)?)?;
    let startup = crate::workflow::startup::Startup::hosted(
        &spec.env,
        spec_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("invalid launch ID")?,
    );
    fs::remove_file(spec_path)?;
    let mut cmd = Command::new(spec.argv.first().context("empty launch argv")?);
    cmd.args(&spec.argv[1..])
        .current_dir(spec.cwd)
        .env_clear()
        .envs(spec.env);
    cmd.env_remove("TMUX").env_remove("TMUX_PANE");
    for key in [
        "TERM",
        "HERDR_SOCKET_PATH",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_PANE_ID",
    ] {
        if let Some(value) = env::var_os(key) {
            cmd.env(key, value);
        } else {
            cmd.env_remove(key);
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
            crate::workflow::write_json(&receipt, &json!({"category":category}), None)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let watching = startup.as_ref().map(|startup| startup.watch(child.id()));
    // Ctrl-C addresses the terminal foreground group. Let the worker handle it
    // and keep its receipt writer alive to record the actual exit.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    // The child can leave the terminal foreground before the backend's first
    // process snapshot. Its unreaped PID still belongs to this launch.
    let mut started_receipt = json!({"category":"started","pid":child.id()});
    if let Ok(Some(writer)) = wfc_herdr::Process::observe(child.id()) {
        started_receipt["writerProcess"] = json!(writer);
    }
    let started = crate::workflow::write_json(&receipt, &started_receipt, None);
    let status = child.wait()?;
    if let Some(watching) = watching {
        watching.exited(status);
    }
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    crate::workflow::write_json(
        &receipt,
        &json!({"category":"process_exited","exitCode":code}),
        None,
    )?;
    started?;
    Ok(ExitCode::from(code as u8))
}
