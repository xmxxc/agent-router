//! Private launch files and worker process receipts for Herdr on Linux.
use crate::workflow::controller::is_credential;
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
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, env, fs};
use uuid::Uuid;
#[derive(Serialize, Deserialize)]
struct Spec {
    cwd: PathBuf,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
}

fn consume_spec(path: &Path) -> Result<Spec> {
    let spec = serde_json::from_reader(File::open(path)?)?;
    fs::remove_file(path)?;
    Ok(spec)
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
    handed_off: bool,
}

impl Prepared {
    pub(crate) fn hand_off(&mut self) {
        self.handed_off = true;
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = fs::remove_file(&self.spec_path);
        }
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
        if !is_credential(&k) {
            environment.insert(
                k,
                v.into_string()
                    .map_err(|_| anyhow::anyhow!("invalid worker launch"))?,
            );
        }
    }
    for (k, v) in extra_env {
        if !is_credential(&k) || k == "WFC_WORKER_CALLBACK" {
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
        handed_off: false,
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

pub(crate) fn track_worker_tree() -> Result<()> {
    // An orphan stays with this supervisor even when it leaves the terminal session.
    anyhow::ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0
            && unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } != libc::SIG_ERR,
        "cannot supervise the worker process tree"
    );
    Ok(())
}

pub(crate) fn wait_for_worker_tree(grace: Option<Duration>) -> Result<bool> {
    let deadline = grace.map(|grace| Instant::now() + grace);
    loop {
        let flags = libc::__WALL | if deadline.is_some() { libc::WNOHANG } else { 0 };
        let child = unsafe { libc::waitpid(-1, std::ptr::null_mut(), flags) };
        if child > 0 {
            continue;
        }
        if child == 0 {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(true),
            Some(libc::EINTR) => (),
            _ => return Err(error).context("wait for worker descendants"),
        }
    }
}

extern "C" fn finish_watching(_: libc::c_int) {}

pub(crate) fn supervise(spec_path: &Path) -> Result<ExitCode> {
    let receipt = spec_path.with_extension("json");
    let id = spec_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("invalid launch ID")?;
    let spec = consume_spec(spec_path)?;
    let startup = crate::workflow::startup::Startup::hosted(&spec.env, id);
    let binding = json!({"backend":"herdr", "launchId":id,
        "dispatch":spec.env.get("WFC_DISPATCH"), "attempt":spec.env.get("WFC_ATTEMPT"),
        "round":spec.env.get("WFC_ROUND"), "treeTracked":true});
    track_worker_tree()?;
    // Herdr signals the terminal session on close. Keep the receipt writer
    // alive to reap the tree. Exec resets these caught signals in the worker.
    let handler = finish_watching as *const () as libc::sighandler_t;
    anyhow::ensure!(
        unsafe { libc::signal(libc::SIGHUP, handler) } != libc::SIG_ERR
            && unsafe { libc::signal(libc::SIGTERM, handler) } != libc::SIG_ERR,
        "cannot preserve Herdr stop receipt writer"
    );
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
    let watching = startup.as_ref().map(|startup| startup.watch(&child));
    // Ctrl-C addresses the terminal foreground group. Let the worker handle it
    // and keep its receipt writer alive to record the actual exit.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    // The child can leave the terminal foreground before the backend's first
    // process snapshot. Its unreaped PID still belongs to this launch.
    let mut started_receipt = binding.clone();
    started_receipt["category"] = json!("started");
    started_receipt["pid"] = json!(child.id());
    if let Ok(Some(writer)) = wfc_herdr::Process::observe(child.id()) {
        started_receipt["writerProcess"] = json!(writer);
    }
    let (status, observed, started) = crate::workflow::startup::wait_host(&mut child, || {
        crate::workflow::write_json(&receipt, &started_receipt, None)
    })?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    let mut terminal = binding;
    terminal["category"] = json!("process_exited");
    terminal["exitCode"] = json!(code);
    if let Some(zero) = watching.and_then(|watching| watching.host_exited(status, observed)) {
        terminal["startupZero"] = zero;
    }
    crate::workflow::write_json(&receipt, &terminal, None)?;
    started?;
    // Keep ordinary pane exit bounded when a detached descendant survives.
    // Only ECHILD permits a receipt. Herdr can also kill this supervisor
    // during escalation, which likewise leaves the stop unconfirmed.
    if wait_for_worker_tree(Some(Duration::from_millis(150)))? {
        terminal["writerTreeStopped"] = json!(true);
        crate::workflow::write_json(&spec_path.with_extension("stopped"), &terminal, None)?;
    }
    Ok(ExitCode::from(code as u8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn herdr_launch_spec_lifetime_follows_delivery() -> Result<()> {
        let root = std::env::temp_dir().join(format!("wfc-herdr-spec-{}", uuid::Uuid::new_v4()));
        let state = root.as_path();
        let result = (|| -> Result<()> {
            let spec = Spec {
                cwd: state.into(),
                argv: vec!["fixture".into()],
                env: BTreeMap::from([("WFC_ATTEMPT".into(), "a-000000000031".into())]),
            };
            for (id, sent, consume_before_drop) in [
                ("01111111111111111111111111111111", false, false),
                ("02222222222222222222222222222222", true, false),
                ("03333333333333333333333333333333", true, true),
            ] {
                let path = spec_path(state, Path::new(id))?;
                fs::create_dir_all(path.parent().unwrap())?;
                private_file(&path, &spec)?;
                let mut files = Prepared {
                    id: id.into(),
                    spec_path: path.clone(),
                    receipt: path.with_extension("json"),
                    handed_off: false,
                };
                assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
                if sent {
                    files.hand_off();
                }
                if consume_before_drop {
                    assert_eq!(consume_spec(&path)?.env, spec.env);
                }
                drop(files);
                assert_eq!(path.try_exists()?, sent && !consume_before_drop);
                if sent && !consume_before_drop {
                    let consumed = consume_spec(&path)?;
                    assert_eq!(consumed.env, spec.env);
                    assert_eq!(consumed.argv, spec.argv);
                    assert_eq!(consumed.cwd, spec.cwd);
                    assert!(!path.try_exists()?);
                    assert!(consume_spec(&path).is_err());
                }
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(root);
        result
    }
}
