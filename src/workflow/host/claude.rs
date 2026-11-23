//! Launch-local Claude SessionStart evidence. The controller owns the expected
//! session and the bound callback; the hook reports what Claude used.
//! The local Claude runtime, its hooks, and WFC state share the Unix account.
use super::{Launch, Options};
use crate::workflow;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    env,
    ffi::OsStr,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const MAX_EVENT: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Source {
    Startup,
    Resume,
}

impl Source {
    fn matcher(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Startup => "--session-id",
            Self::Resume => "--resume",
        }
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Identity {
    dispatch: String,
    attempt: String,
    cwd: PathBuf,
    session: String,
    source: Source,
}

#[derive(Deserialize, Serialize)]
struct Expected {
    identity: Identity,
    profile: PathBuf,
    original_transcript: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Adoption {
    identity: Identity,
    transcript: PathBuf,
}

#[derive(Deserialize)]
struct HookEvent {
    session_id: String,
    cwd: PathBuf,
    source: Source,
    hook_event_name: String,
    transcript_path: PathBuf,
}

fn field<'a>(record: &'a Value, name: &str) -> Result<&'a str> {
    record[name]
        .as_str()
        .context("missing Claude assignment field")
}

fn attempt_dir(state: &Path, dispatch: &str, attempt: &str) -> Result<PathBuf> {
    ensure!(
        workflow::valid_id(dispatch) && workflow::valid_identity(attempt, "a-"),
        "invalid Claude assignment identity"
    );
    Ok(state
        .join("claude-evidence")
        .join(format!("{dispatch}.{attempt}")))
}

fn private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

fn effective_profile(cwd: &Path, launch: &Launch) -> Result<PathBuf> {
    let path_for = |key| {
        launch
            .env
            .get(key)
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| env::var_os(key).map(PathBuf::from))
            .filter(|value| !value.as_os_str().is_empty())
    };
    let path = match path_for("CLAUDE_CONFIG_DIR") {
        Some(path) => path,
        None => path_for("HOME")
            .or_else(env::home_dir)
            .context("missing Claude home directory")?
            .join(".claude"),
    };
    storage_path(&cwd.join(path)).context("resolve Claude configuration directory")
}

// The transcript and its project directory may not exist at SessionStart.
fn storage_path(path: &Path) -> io::Result<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(suffix.into_iter().rev());
                return Ok(resolved);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else {
                    return Err(err);
                };
                suffix.push(name);
                ancestor = parent;
            }
            Err(err) => return Err(err),
        }
    }
}

fn create_plugin(state: &Path, expected: &Expected) -> Result<PathBuf> {
    let id = &expected.identity;
    let dir = attempt_dir(state, &id.dispatch, &id.attempt)?;
    let root = dir.parent().context("missing Claude evidence parent")?;
    DirBuilder::new().mode(0o700).recursive(true).create(root)?;
    let root_metadata = root.symlink_metadata()?;
    ensure!(
        root_metadata.is_dir()
            && root_metadata.mode() & 0o077 == 0
            && root_metadata.uid() == unsafe { libc::geteuid() },
        "Claude evidence root must be private"
    );
    private_directory(&dir).context("reserve private Claude evidence directory")?;
    let result = (|| {
        let plugin = dir.join("plugin");
        private_directory(&plugin)?;
        private_directory(&plugin.join(".claude-plugin"))?;
        private_directory(&plugin.join("hooks"))?;
        workflow::write_json(
            &plugin.join(".claude-plugin/plugin.json"),
            &json!({"name": format!("wfc-session-evidence-{}", id.attempt)}),
            None,
        )?;
        // A normal Claude command hook is a shell command. Quote only the WFC
        // executable; the runtime data arrives as environment and JSON stdin.
        let bin = env::current_exe()?;
        let bin = bin.to_str().context("WFC executable path must be UTF-8")?;
        let command = format!(
            "exec '{}' --claude-session-start",
            bin.replace('\'', "'\\''")
        );
        workflow::write_json(
            &plugin.join("hooks/hooks.json"),
            &json!({"hooks": {"SessionStart": [{
                "matcher": id.source.matcher(),
                "hooks": [{"type": "command", "command": command}]
            }]}}),
            None,
        )?;
        workflow::write_json(
            &dir.join("expected.json"),
            &serde_json::to_value(expected)?,
            None,
        )?;
        workflow::messages::sync_directory(&dir)?;
        workflow::messages::sync_directory(root)?;
        Ok(plugin)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

fn prepare(
    state: &Path,
    record: &mut Value,
    launch: &mut Launch,
    brief: String,
    writable: &[PathBuf],
    session: String,
    original_transcript: Option<PathBuf>,
) -> Result<()> {
    let cwd = PathBuf::from(field(record, "cwd")?);
    let label = field(record, "label")?;
    let source = if original_transcript.is_some() {
        Source::Resume
    } else {
        Source::Startup
    };
    let expected = Expected {
        identity: Identity {
            dispatch: field(record, "dispatch")?.to_owned(),
            attempt: field(record, "attempt")?.to_owned(),
            cwd: cwd.clone(),
            session: session.clone(),
            source,
        },
        profile: effective_profile(&cwd, launch)?,
        original_transcript,
    };
    let dirs = writable
        .iter()
        .map(|p| {
            p.to_str()
                .map(String::from)
                .context("writable path must be UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    let plugin = create_plugin(state, &expected)?;
    launch
        .argv
        .extend([brief, "--permission-mode".into(), "auto".into()]);
    for (flag, key) in [("--model", "model"), ("--effort", "effort")] {
        if let Some(value) = record[key].as_str() {
            launch.argv.extend([flag.into(), value.into()]);
        }
    }
    launch.argv.extend([
        "--name".into(),
        label.into(),
        source.flag().into(),
        session.clone(),
        "--plugin-dir".into(),
        plugin
            .to_str()
            .context("Claude plugin path must be UTF-8")?
            .into(),
        "--add-dir".into(),
    ]);
    launch.argv.extend(dirs);
    record["hostSession"] = json!(session);
    Ok(())
}

pub(super) fn prepare_initial(
    state: &Path,
    options: &Options<'_>,
    record: &mut Value,
    launch: &mut Launch,
    brief: String,
    writable: &[PathBuf],
) -> Result<()> {
    record["model"] = json!(options.model);
    record["effort"] = json!(options.effort);
    record["network"] = json!(options.network);
    prepare(
        state,
        record,
        launch,
        brief,
        writable,
        Uuid::new_v4().to_string(),
        None,
    )
}

pub(super) fn apply_existing_trust(record: &mut Value) -> Result<()> {
    match trust(Path::new(field(record, "cwd")?)) {
        Ok(Some(ancestor)) => record["trustedBy"] = json!(ancestor),
        Ok(None) => {}
        Err(_) => eprintln!("wfc: could not confirm Claude worktree trust"),
    }
    Ok(())
}

/// The shared resume entry calls this after fencing and checking the old worker.
/// It supplies the launch built from the stored launch config, then publishes
/// the new assignment and callback before passing the returned argv to tmux.
#[allow(dead_code, reason = "shared resume admission is integrated separately")]
pub(in crate::workflow) fn prepare_resume(
    state: &Path,
    previous: &Value,
    next: &mut Value,
    launch: &mut Launch,
    brief: String,
    writable: &[PathBuf],
) -> Result<()> {
    ensure!(
        previous["host"] == "claude"
            && next["host"] == "claude"
            && previous["dispatch"] == next["dispatch"]
            && previous["attempt"] != next["attempt"]
            && previous["cwd"] == next["cwd"],
        "Claude resume assignment mismatch"
    );
    let cwd = Path::new(field(previous, "cwd")?);
    ensure!(
        cwd.canonicalize()? == cwd,
        "Claude resume directory changed"
    );
    let (expected, saved) = adoption(state, previous)?;
    ensure!(
        expected.profile == effective_profile(cwd, launch)?,
        "Claude resume profile changed"
    );
    let transcript = saved
        .transcript
        .canonicalize()
        .context("Claude session is not persisted")?;
    ensure!(
        transcript == saved.transcript,
        "Claude persisted session path changed"
    );
    let metadata = transcript
        .metadata()
        .context("Claude session is not persisted")?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "Claude session is not persisted"
    );
    for key in [
        "label",
        "model",
        "effort",
        "network",
        "git",
        "ignored",
        "launchConfig",
    ] {
        if let Some(value) = previous.get(key) {
            next[key] = value.clone();
        }
    }
    prepare(
        state,
        next,
        launch,
        brief,
        writable,
        saved.identity.session,
        Some(saved.transcript),
    )
}

// This can also be checked immediately after startup. The transcript can be
// created later, so only resume preparation requires a regular file on disk.
fn adoption(state: &Path, row: &Value) -> Result<(Expected, Adoption)> {
    let dispatch = field(row, "dispatch")?;
    let attempt = field(row, "attempt")?;
    let dir = attempt_dir(state, dispatch, attempt)?;
    let error = "Claude session adoption not verified";
    let binding = open_binding(&dir).context(error)?;
    binding.lock_shared()?;
    let expected: Expected = serde_json::from_reader(&binding).context(error)?;
    let saved: Adoption =
        serde_json::from_reader(File::open(dir.join("adopted.json")).context(error)?)
            .context(error)?;
    ensure!(
        expected.identity == saved.identity
            && saved.identity.dispatch == dispatch
            && saved.identity.attempt == attempt
            && saved.identity.cwd.as_path() == Path::new(field(row, "cwd")?)
            && saved.identity.session == field(row, "hostSession")?,
        "{error}"
    );
    check_transcript(&saved.transcript, &expected)?;
    Ok((expected, saved))
}

fn check_transcript(path: &Path, expected: &Expected) -> Result<()> {
    let filename = format!("{}.jsonl", expected.identity.session);
    let mut parts = path
        .strip_prefix(expected.profile.join("projects"))
        .context("Claude transcript is outside the expected profile")?
        .components();
    ensure!(
        matches!(parts.next(), Some(Component::Normal(_)))
            && parts.next() == Some(Component::Normal(OsStr::new(&filename)))
            && parts.next().is_none()
            && expected
                .original_transcript
                .as_deref()
                .is_none_or(|old| old == path),
        "Claude transcript does not name the expected session"
    );
    Ok(())
}

fn open_binding(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("expected.json"))?;
    ensure!(file.metadata()?.is_file(), "invalid Claude launch binding");
    Ok(file)
}

fn record_start(
    state: &Path,
    dispatch: &str,
    attempt: &str,
    callback: &str,
    stdin: impl Read,
) -> Result<()> {
    let dir = attempt_dir(state, dispatch, attempt)?;
    let mut data = Vec::new();
    stdin.take(MAX_EVENT + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_EVENT,
        "Claude SessionStart event too large"
    );
    let event: HookEvent = serde_json::from_slice(&data)?;

    let file = open_binding(&dir)?;
    file.lock()?;
    let expected: Expected = serde_json::from_reader(&file)?;
    let (row, path) = workflow::assignment::open_assignment(&state.join("dispatches"), dispatch)?;
    let row = workflow::decode_record(row, &path)?;
    ensure!(
        !callback.is_empty()
            && row[workflow::controller::CALLBACK_HASH] == workflow::messages::body_hash(callback)
            && row["host"] == "claude"
            && row["dispatch"] == dispatch
            && expected.identity.dispatch == dispatch
            && row["attempt"] == attempt
            && expected.identity.attempt == attempt
            && row["hostSession"] == expected.identity.session
            && Path::new(field(&row, "cwd")?) == expected.identity.cwd.as_path(),
        "invalid Claude attempt capability"
    );
    ensure!(
        event.hook_event_name == "SessionStart"
            && event.source == expected.identity.source
            && event.session_id == expected.identity.session
            && event.cwd.canonicalize()? == expected.identity.cwd,
        "Claude SessionStart identity mismatch"
    );
    ensure!(
        event.transcript_path.is_absolute(),
        "invalid Claude transcript path"
    );
    let transcript = storage_path(&event.transcript_path)?;
    check_transcript(&transcript, &expected)?;
    let receipt = Adoption {
        identity: expected.identity,
        transcript,
    };
    let path = dir.join("adopted.json");
    match File::open(&path) {
        Ok(existing) => {
            let previous: Adoption = serde_json::from_reader(existing)?;
            if previous != receipt {
                workflow::write_json(&path, &json!({"error": "conflicting-session-start"}), None)?;
                workflow::messages::sync_directory(&dir)?;
                bail!("conflicting Claude SessionStart receipt");
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            workflow::write_json(&path, &serde_json::to_value(receipt)?, None)?;
            workflow::messages::sync_directory(&dir)?;
        }
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

pub(crate) fn session_start() -> Result<()> {
    let record = || -> Result<()> {
        record_start(
            &workflow::state_dir()?,
            &env::var("WFC_DISPATCH")?,
            &env::var("WFC_ATTEMPT")?,
            &env::var(workflow::controller::WORKER_CALLBACK)?,
            io::stdin().lock(),
        )
    };
    // Do not print hook input, paths or the attempt capability.
    record().map_err(|_| anyhow::anyhow!("Claude SessionStart evidence rejected"))
}

// Match the host's existing trust decision for this directory or a real
// ancestor. Serialize router updates to the config, including symlinked files.
fn trust(cwd: &Path) -> Result<Option<PathBuf>> {
    let Some(config) = env::var_os("WFC_CLAUDE_CONFIG")
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|p| p.join(".claude.json")))
    else {
        return Ok(None);
    };
    trust_file(&config, cwd)
}

fn trust_file(config: &Path, cwd: &Path) -> Result<Option<PathBuf>> {
    let path = match config.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let name = path
        .file_name()
        .context("missing Claude config filename")?
        .to_string_lossy();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path.with_file_name(format!(".{name}.wfc.lock")))?;
    lock.lock()?;
    let f = fs::File::open(&path)?;
    let permissions = f.metadata()?.permissions();
    let mut config: Value = serde_json::from_reader(f)?;
    let Some(projects) = config.get_mut("projects").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let key = cwd.to_str().context("cwd must be UTF-8")?;
    if projects
        .get(key)
        .is_some_and(|p| p["hasTrustDialogAccepted"] == true)
    {
        return Ok(Some(cwd.to_owned()));
    }
    let ancestor = projects
        .iter()
        .filter(|(_, v)| v["hasTrustDialogAccepted"] == true)
        .filter_map(|(k, _)| Path::new(k).canonicalize().ok())
        .filter(|p| p != cwd && cwd.starts_with(p))
        .max_by_key(|p| p.components().count());
    if ancestor.is_some() {
        let project = projects.entry(key.to_owned()).or_insert_with(|| json!({}));
        ensure!(project.is_object(), "invalid Claude project");
        project["hasTrustDialogAccepted"] = json!(true);
        workflow::write_json(&path, &config, Some(permissions))?;
    }
    Ok(ancestor)
}

#[cfg(test)]
mod tests;
