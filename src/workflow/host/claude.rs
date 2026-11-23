//! Launch-local Claude SessionStart evidence. The controller owns the expected
//! session and the bound callback; the hook reports what Claude used.
//! The local Claude runtime, its hooks, and WFC state share the Unix account.
use super::{Launch, Options};
use crate::workflow;
use crate::workflow::messages::PublishLock;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::{CString, OsStr},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_EVENT: u64 = 16 * 1024;

// Claude Code 2.1.325 Linux native artifacts from Anthropic's signed manifest.
// An unrecognized installation can still launch and handle its own trust prompt.
#[cfg(target_arch = "x86_64")]
const NATIVE_RELEASE: &[(u64, &str)] = &[
    (
        365842960,
        "0dc4f4a9a165fc60746d7ae8489712e5fc352f1e7ad4eb22e7141bd0812eb0cf",
    ),
    (
        352201992,
        "a5e3bd36dce97db0429705aa5c3573eea346a77938a89104b9c28fce409b20cd",
    ),
];
#[cfg(target_arch = "aarch64")]
const NATIVE_RELEASE: &[(u64, &str)] = &[
    (
        364701112,
        "7b6e484b1989a5a14a4b354709ccc8672518a3ec5b9ccd7981fc6254327fde67",
    ),
    (
        349795272,
        "14dc17cd958f62d82fb2ff6276368fc611a88be303e0ae1a8cca580495964efe",
    ),
];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const NATIVE_RELEASE: &[(u64, &str)] = &[];

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

#[derive(Deserialize, Eq, PartialEq, Serialize)]
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

fn worker_path(launch: &Launch, key: &str) -> Option<PathBuf> {
    launch
        .env
        .get(key)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| env::var_os(key).map(PathBuf::from))
}

fn effective_profile(cwd: &Path, launch: &Launch) -> Result<PathBuf> {
    let nonempty = |key| worker_path(launch, key).filter(|p| !p.as_os_str().is_empty());
    let path = match nonempty("CLAUDE_CONFIG_DIR") {
        Some(path) => path,
        None => nonempty("HOME")
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

fn verified_native(cwd: &Path, search: &OsStr, release: &[(u64, &str)]) -> Option<PathBuf> {
    for directory in env::split_paths(search) {
        let candidate = cwd.join(directory).join("claude");
        let metadata = match candidate.metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            continue;
        }
        // Command resolution would try this executable before later entries.
        // A shim in front of the official binary must leave trust to Claude.
        let &(size, checksum) = release.iter().find(|(size, _)| *size == metadata.len())?;
        let path = candidate.canonicalize().ok()?;
        let executable = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        if unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                executable.as_ptr(),
                libc::X_OK,
                libc::AT_EACCESS,
            )
        } != 0
        {
            return None;
        }
        let trusted_owner = |metadata: &fs::Metadata| {
            metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() }
        };
        if !path.ancestors().skip(1).all(|parent| {
            parent.metadata().is_ok_and(|metadata| {
                metadata.is_dir()
                    && trusted_owner(&metadata)
                    && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0)
            })
        }) {
            return None;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file()
            || metadata.mode() & 0o6022 != 0
            || !trusted_owner(&metadata)
            || metadata.len() != size
        {
            return None;
        }
        let mut bytes = [0; 64 * 1024];
        let mut stream = file.take(size + 1);
        let mut digest = Sha256::new();
        loop {
            let count = stream.read(&mut bytes).ok()?;
            if count == 0 {
                break;
            }
            digest.update(&bytes[..count]);
        }
        return (format!("{:x}", digest.finalize()) == checksum).then_some(path);
    }
    None
}

pub(super) fn apply_existing_trust(record: &mut Value, launch: &mut Launch) -> Result<()> {
    let home = worker_path(launch, "HOME");
    let profile = worker_path(launch, "CLAUDE_CONFIG_DIR");
    if home.as_ref().is_none_or(|path| !path.is_absolute())
        || profile.is_some()
        || ["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT", "BUN_OPTIONS"]
            .iter()
            .any(|key| worker_path(launch, key).is_some_and(|path| !path.as_os_str().is_empty()))
    {
        return Ok(());
    }
    let cwd = Path::new(field(record, "cwd")?);
    let Some(path) = worker_path(launch, "PATH")
        .and_then(|search| verified_native(cwd, search.as_os_str(), NATIVE_RELEASE))
    else {
        return Ok(());
    };
    let Some(program) = path.to_str() else {
        return Ok(());
    };
    let requested = worker_path(launch, "WFC_CLAUDE_CONFIG");
    match trust(
        cwd,
        home.as_deref(),
        profile.as_deref(),
        requested.as_deref(),
    ) {
        Ok(Some(ancestor)) => {
            launch.argv[0] = program.to_owned();
            record["trustedBy"] = json!(ancestor);
        }
        Ok(None) => {}
        Err(_) => eprintln!("wfc: could not confirm Claude worktree trust"),
    }
    Ok(())
}

pub(in crate::workflow) struct ResumeProof {
    state: PathBuf,
    next_attempt: String,
    expected: Expected,
    adopted: Adoption,
}

impl ResumeProof {
    /// Called after the owner's CAS, with scope, execution and Inbox locks held.
    /// The new callback and the pending resume audit must already be in `next`.
    /// Adoption consumption and assignment publication share one atomic write.
    pub(in crate::workflow) fn publish(
        self,
        ledger: &PublishLock,
        previous: &Value,
        next: &mut Value,
        launch: &Launch,
    ) -> Result<()> {
        let identity = &self.adopted.identity;
        let current_attempt = field(previous, "attempt")?;
        ensure!(
            ledger.dir() == self.state.as_path()
                && previous["host"] == "claude"
                && previous["dispatch"] == identity.dispatch
                && next["dispatch"] == previous["dispatch"]
                && next["host"] == "claude"
                && next["attempt"] == self.next_attempt
                && self.next_attempt != current_attempt
                && next["cwd"] == previous["cwd"]
                && next["hostSession"] == previous["hostSession"]
                && launch
                    .env
                    .get(workflow::controller::WORKER_CALLBACK)
                    .and_then(Value::as_str)
                    .is_some_and(|secret| {
                        !secret.is_empty()
                            && next[workflow::controller::CALLBACK_HASH]
                                == workflow::messages::body_hash(secret)
                    })
                && next[workflow::controller::CALLBACK_HASH]
                    != previous[workflow::controller::CALLBACK_HASH],
            "Claude resume publication mismatch"
        );
        let dir = ledger.dir().join("dispatches");
        let (file, path) = workflow::assignment::open_assignment(&dir, &identity.dispatch)?;
        ensure!(
            path.extension() == Some(OsStr::new("json"))
                && &workflow::decode_record(file, &path)? == previous,
            "Claude assignment changed while preparing resume"
        );
        let (expected, adopted) = resume_source_adoption(ledger, previous)?;
        ensure!(
            expected == self.expected
                && adopted == self.adopted
                && identity.cwd.canonicalize()? == identity.cwd
                && effective_profile(&identity.cwd, launch)? == expected.profile,
            "Claude adoption changed while preparing resume"
        );
        persisted_transcript(&adopted)?;
        let round = next["round"].clone();
        let event = next["audit"]
            .as_array_mut()
            .and_then(|events| events.last_mut())
            .context("missing pending Claude resume audit")?;
        ensure!(
            event["operation"] == "resume"
                && event["result"] == "pending"
                && event["dispatch"] == identity.dispatch
                && event["fromAttempt"] == current_attempt
                && event["toAttempt"] == self.next_attempt
                && event["round"] == round
                && event["session"] == identity.session
                && event["claudeAdoption"].is_null(),
            "pending audit does not match Claude resume"
        );
        let receipt = serde_json::to_string(&adopted)?;
        event["claudeAdoption"] = json!({
            "attempt": identity.attempt,
            "source": identity.source,
            "receiptHash": workflow::messages::body_hash(&receipt)
        });
        workflow::write_json(&path, next, None)?;
        workflow::messages::sync_directory(&dir)
            .context("Claude resume may already be published; inspect the pending audit")
    }
}

/// The shared resume entry calls this under the scope and execution locks,
/// after checking the old worker and before taking the Inbox lock. Preparation
/// snapshots adoption under its own short Inbox lock. The caller then binds the
/// new callback and uses the proof to publish under its Inbox lock before launch.
pub(in crate::workflow) fn prepare_resume(
    state: &Path,
    previous: &Value,
    next: &mut Value,
    launch: &mut Launch,
    brief: String,
    writable: &[PathBuf],
) -> Result<ResumeProof> {
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
    let (expected, saved) = {
        let ledger = workflow::messages::lock_for_publish(state)?;
        resume_source_adoption(&ledger, previous)?
    };
    ensure!(
        expected.profile == effective_profile(cwd, launch)?,
        "Claude resume profile changed"
    );
    persisted_transcript(&saved)?;
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
        saved.identity.session.clone(),
        Some(saved.transcript.clone()),
    )?;
    Ok(ResumeProof {
        state: state.to_owned(),
        next_attempt: field(next, "attempt")?.to_owned(),
        expected,
        adopted: saved,
    })
}

fn persisted_transcript(saved: &Adoption) -> Result<()> {
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
    Ok(())
}

fn resume_source_adoption(ledger: &PublishLock, row: &Value) -> Result<(Expected, Adoption)> {
    let Some(current) = workflow::assignment::resume_audit(row) else {
        return adoption(ledger, row);
    };
    if current["launchResult"] != "definitely-not-sent" {
        ensure!(
            confirmed_resume_adoption(current),
            "Claude resume adoption is unconfirmed"
        );
        return adoption(ledger, row);
    }

    // Only settled non-starts may borrow the source of an earlier attempt.
    // The shared resume path separately confirms the current tmux slot is empty.
    let error = "Claude non-start audit does not verify the resume source";
    let mut create = &row["create"];
    let dispatch = field(row, "dispatch")?;
    let launches = crate::tmux::launch::NonStartProofs::read(ledger.dir(), dispatch)?;
    let mut attempt = field(row, "attempt")?;
    let mut round = row["round"].as_u64().context(error)?;
    let mut skipped = Vec::new();
    let mut source = Source::Startup;
    for event in row["audit"].as_array().context(error)?.iter().rev() {
        if event["operation"] != "resume"
            || (event["attempt"] != attempt && event["toAttempt"] != attempt)
        {
            continue;
        }
        let event_round = event["round"].as_u64().context(error)?;
        ensure!(
            [
                "dispatch",
                "attempt",
                "round",
                "launchResult",
                "result",
                "category",
                "adoptionResult",
                "inputResult",
                "receipt",
                "cancellationEvidence",
                "exitCode"
            ]
            .iter()
            .all(|&key| create[key] == event[key])
                && event["dispatch"] == dispatch
                && event["attempt"] == attempt
                && event["toAttempt"] == attempt
                && event["fromHost"] == "claude"
                && event["toHost"] == "claude"
                && event["session"] == row["hostSession"]
                && event["settledAt"].is_string()
                && event_round > 0
                && round
                    .checked_sub(event_round)
                    .is_some_and(|delta| delta <= 1),
            "{error}"
        );
        if event["launchResult"] != "definitely-not-sent" {
            ensure!(confirmed_resume_adoption(event), "{error}");
            source = Source::Resume;
            break;
        }
        ensure!(
            matches!(
                event["result"].as_str(),
                Some("refused" | "definitely-not-sent")
            ) && !matches!(
                event["category"].as_str(),
                Some("started" | "process_exited")
            ) && event["exitCode"].is_null()
                && (event["inputResult"].is_null()
                    || event["inputResult"] == "definitely-not-sent")
                && event["adoptionResult"].is_null(),
            "{error}"
        );
        launches.verify(attempt, create)?;
        let receipt = attempt_dir(ledger.dir(), dispatch, attempt)?.join("adopted.json");
        ensure!(
            matches!(receipt.symlink_metadata(), Err(e) if e.kind() == io::ErrorKind::NotFound),
            "{error}"
        );
        skipped.push(event);
        let previous = field(event, "fromAttempt")?;
        ensure!(previous != attempt, "{error}");
        attempt = previous;
        round = event_round;
        create = &event["priorCreate"];
    }
    ensure!(!skipped.is_empty(), "{error}");
    let (expected, saved) = adoption_for(ledger, row, attempt)?;
    let hash = workflow::messages::body_hash(&serde_json::to_string(&saved)?);
    ensure!(
        saved.identity.source == source
            && skipped.into_iter().all(|event| {
                let reference = &event["claudeAdoption"];
                // Older audits recorded a hash of the immediately prior attempt.
                reference
                    .get("attempt")
                    .unwrap_or(&event["fromAttempt"])
                    .as_str()
                    == Some(attempt)
                    && reference["source"] == source.matcher()
                    && reference["receiptHash"] == hash
            }),
        "{error}"
    );
    Ok((expected, saved))
}

fn confirmed_resume_adoption(event: &Value) -> bool {
    event["result"] == "accepted"
        && event["launchResult"] == "accepted"
        && event["adoptionResult"] == "accepted"
}

// This can also be checked immediately after startup. The transcript can be
// created later, so only resume preparation requires a regular file on disk.
fn adoption(ledger: &PublishLock, row: &Value) -> Result<(Expected, Adoption)> {
    adoption_for(ledger, row, field(row, "attempt")?)
}

fn adoption_for(ledger: &PublishLock, row: &Value, attempt: &str) -> Result<(Expected, Adoption)> {
    let dispatch = field(row, "dispatch")?;
    let dir = attempt_dir(ledger.dir(), dispatch, attempt)?;
    let error = "Claude session adoption not verified";
    let binding = open_binding(&dir).context(error)?;
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

pub(in crate::workflow) fn resume_adopted(ledger: &PublishLock, row: &Value) -> Result<()> {
    let (_, receipt) = adoption(ledger, row)?;
    ensure!(
        receipt.identity.source == Source::Resume,
        "Claude resume adoption not verified"
    );
    Ok(())
}

// Wait outside the publication lock so SessionStart can write its receipt.
// Settlement reads it again under the Inbox lock before reporting success.
pub(in crate::workflow) fn wait_for_resume_adoption(state: &Path, row: &Value) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if workflow::messages::lock_for_publish(state)
            .and_then(|ledger| resume_adopted(&ledger, row))
            .is_ok()
        {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
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
    let mut data = Vec::new();
    stdin.take(MAX_EVENT + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_EVENT,
        "Claude SessionStart event too large"
    );
    let event: HookEvent = serde_json::from_slice(&data)?;
    let ledger = workflow::messages::lock_for_publish(state)?;
    record_start_locked(&ledger, dispatch, attempt, callback, event)
}

fn record_start_locked(
    ledger: &PublishLock,
    dispatch: &str,
    attempt: &str,
    callback: &str,
    event: HookEvent,
) -> Result<()> {
    let state = ledger.dir();
    let dir = attempt_dir(state, dispatch, attempt)?;
    let file = open_binding(&dir)?;
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
fn trust(
    cwd: &Path,
    home: Option<&Path>,
    profile: Option<&Path>,
    requested: Option<&Path>,
) -> Result<Option<PathBuf>> {
    // Claude documents the default global trust file separately from profile
    // settings. A custom profile keeps Claude's own trust decision.
    let Some(home) = home.filter(|home| profile.is_none() && home.is_absolute()) else {
        return Ok(None);
    };
    let home = home.canonicalize()?;
    let path = home.join(".claude.json");
    match path.canonicalize() {
        Ok(canonical) if canonical == path => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    if requested.is_some_and(|requested| {
        !requested.is_absolute() || requested.canonicalize().ok().as_ref() != Some(&path)
    }) {
        return Ok(None);
    }
    trust_file(&path, cwd)
}

fn trust_file(path: &Path, cwd: &Path) -> Result<Option<PathBuf>> {
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
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.with_file_name(format!(".{name}.wfc.lock")))?;
    ensure!(lock.metadata()?.is_file(), "invalid Claude trust lock");
    lock.lock()?;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = f.metadata()?;
    ensure!(metadata.is_file(), "invalid Claude global config");
    let permissions = metadata.permissions();
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
        workflow::write_json(path, &config, Some(permissions))?;
    }
    Ok(ancestor)
}

#[cfg(test)]
mod tests;
