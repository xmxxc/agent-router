//! Read-only decoding of the retired tmux launch index and receipts.
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::ErrorKind,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

// Keep validating the complete historical wire shapes, including fields
// that were only used to operate the retired backend.
#[allow(dead_code)]
#[derive(Deserialize)]
enum Claim {
    LegacyPending {
        execution: Option<String>,
        socket: Option<PathBuf>,
        #[serde(flatten)]
        hints: LegacyHints,
    },
    Released(Option<String>),
    Launch(String),
    Registered {
        id: String,
        execution: String,
    },
    Observed {
        execution: Option<String>,
        server: ServerSession,
        token: String,
        #[serde(flatten)]
        hints: LegacyHints,
        receipt: Option<String>,
    },
}

// Historical claims may contain a saved pane hint or a bound process.
#[allow(dead_code)]
#[derive(Deserialize)]
enum PaneEvidence {
    Hint {
        boot: String,
        namespace: u64,
        listener: u64,
        ticks: u64,
    },
    Bound(PaneProcess),
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct PaneProcess {
    pid: u32,
    ticks: u64,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct LegacyHints {
    pane: Option<String>,
    launch: Option<String>,
    evidence: Option<PaneEvidence>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct ServerSession {
    #[serde(rename = "launchId")]
    launch_id: Option<String>,
    socket: PathBuf,
    boot: String,
    namespace: u64,
    pid: u32,
    start_ticks: u64,
    session: String,
    pane: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CloseEffect {
    AlreadyAbsent,
    SessionClosed,
    WorkerExited,
    UnstartedSessionClosed,
}

#[allow(dead_code)]
#[derive(Deserialize)]
enum CloseOperation {
    Pending,
    NotExecuted,
    Confirmed(CloseEffect),
}

#[allow(dead_code)]
#[derive(Default, Deserialize)]
struct Registry {
    launches: BTreeMap<String, String>,
    #[serde(default)]
    legacy_launches: BTreeMap<String, String>,
    closed: BTreeSet<String>,
    close_operations: BTreeMap<String, BTreeMap<String, CloseOperation>>,
}

#[derive(Deserialize)]
enum SubmissionPhase {
    Prepared,
    InFlight,
    Returned,
    LegacyAbsent,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Submission {
    launch_id: String,
    dispatch: String,
    execution_id: Option<String>,
    #[serde(default)]
    round: Option<u64>,
    socket: Option<PathBuf>,
    origin: Option<ServerSession>,
    phase: SubmissionPhase,
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

fn valid_launch_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'))
}

fn valid_dispatch(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|e| e.kind() == ErrorKind::NotFound)
}

fn attempt_launches(state: &Path, dispatch: &str) -> Result<BTreeMap<String, String>> {
    fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        if missing(path) {
            return Ok(None);
        }
        let value = read_private_json(path).context("unreadable tmux launch index")?;
        Ok(Some(
            serde_json::from_value(value).context("invalid tmux launch index")?,
        ))
    }
    ensure!(valid_dispatch(dispatch), "invalid tmux dispatch identity");
    let dir = state.join("tmux");
    if missing(&dir) {
        return Ok(BTreeMap::new());
    }
    ensure!(
        dir.symlink_metadata()?.is_dir(),
        "invalid tmux launch index directory"
    );
    let path = dir.join(format!("{dispatch}.json"));
    // The backend archives the old claim before replacing it. Read the claim
    // first so a concurrent archive cannot hide the outgoing attempt.
    let claim = read::<Claim>(&path)?;
    let registry = read::<Registry>(&path.with_extension("executions.json"))?.unwrap_or_default();
    let active = match claim {
        Some(Claim::Registered { id, execution }) => Some((execution, id)),
        Some(Claim::Observed {
            execution: Some(execution),
            receipt: Some(id),
            ..
        }) => Some((execution, id)),
        _ => None,
    };
    let mut launches = registry.launches;
    for (attempt, launch) in registry.legacy_launches.into_iter().chain(active) {
        ensure!(
            launches.get(&attempt).is_none_or(|id| id == &launch),
            "conflicting tmux launch index"
        );
        launches.insert(attempt, launch);
    }
    Ok(launches)
}

impl Submission {
    fn identifies(&self, launch: &str, dispatch: &str, execution: Option<&str>) -> bool {
        self.launch_id == launch
            && self.dispatch == dispatch
            && self.execution_id.as_deref() == execution
    }
}

pub(super) struct NonStartProofs<'a> {
    dir: PathBuf,
    dispatch: &'a str,
    launches: BTreeMap<String, String>,
}

impl<'a> NonStartProofs<'a> {
    pub(super) fn read(state: &Path, dispatch: &'a str) -> Result<Self> {
        let state = state.canonicalize()?;
        Ok(Self {
            dir: state.join("launches"),
            dispatch,
            launches: attempt_launches(&state, dispatch)?,
        })
    }

    // A pre-submission refusal can have no private launch at all. If tmux
    // registered a launch, check it even when the public attempt has no path.
    pub(super) fn verify(&self, attempt: &str, outcome: &Value) -> Result<()> {
        let error = "tmux launch evidence does not verify that the worker never started";
        let category = &outcome["category"];
        ensure!(category.is_null() || category.is_string(), "{error}");
        let indexed = self.launches.get(attempt).map(String::as_str);
        let reference = match (&outcome["receipt"], &outcome["cancellationEvidence"]) {
            (Value::String(path), Value::Null) => Some((Path::new(path), "json")),
            (Value::Null, Value::String(path)) => Some((Path::new(path), "revoked")),
            (Value::Null, Value::Null) => None,
            _ => anyhow::bail!("{error}"),
        };
        let id = match (reference, indexed) {
            (Some((path, _)), _) => path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context(error)?,
            (None, Some(id)) => id,
            (None, None) => {
                ensure!(!spawn_refusal(category), "{error}");
                return Ok(());
            }
        };
        ensure!(
            valid_launch_id(id)
                && indexed.is_none_or(|registered| registered == id)
                && self.dir.symlink_metadata()?.is_dir(),
            "{error}"
        );
        let prefix = self.dir.join(id);
        let marker = prefix.with_extension("revoked");
        let path = if let Some((path, extension)) = reference {
            ensure!(path == prefix.with_extension(extension), "{error}");
            path.to_owned()
        } else if missing(&marker) {
            prefix.with_extension("json")
        } else {
            marker.clone()
        };
        let proof = read_receipt(&path).context(error)?;
        ensure!(
            (category.is_null() || proof.get("category") == Some(category))
                && missing(&prefix.with_extension("spec")),
            "{error}"
        );
        let spawn_refusal = spawn_refusal(&proof["category"]);
        let dispatch = self.dispatch;
        let submission = prefix.with_extension("submission");
        if spawn_refusal || !missing(&submission) {
            let submission: Submission =
                serde_json::from_value(read_receipt(&submission).context(error)?).context(error)?;
            ensure!(
                submission.identifies(id, dispatch, Some(attempt)),
                "{error}"
            );
        }
        if spawn_refusal {
            let binding = if ["launchId", "dispatch", "executionId"]
                .iter()
                .all(|&key| proof.get(key).is_none())
            {
                // Older writers omitted the identifiers after recording the
                // supervisor's server binding and claiming the descriptor.
                read_receipt(&prefix.with_extension("server")).is_some_and(|server| {
                    server["launchId"] == id
                        && serde_json::from_value::<ServerSession>(server).is_ok()
                })
            } else {
                proof["launchId"] == id
                    && proof["dispatch"] == dispatch
                    && proof["executionId"] == attempt
            };
            ensure!(
                path != marker
                    && binding
                    && ["pid", "exitCode", "processStart", "startupExit"]
                        .iter()
                        .all(|&key| proof[key].is_null())
                    && missing(&marker),
                "{error}"
            );
        } else {
            ensure!(
                proof == cancellation_record(id, dispatch, Some(attempt), None)
                    && missing(&prefix.with_extension("server")),
                "{error}"
            );
            let receipt = prefix.with_extension("json");
            if path == marker {
                // A marker can replace an unwritable initial receipt, never a
                // receipt recording that the worker acquired the launch.
                ensure!(
                    missing(&receipt)
                        || read_receipt(&receipt).is_some_and(|record| {
                            record == json!({"category":"unknown", "launchId":id})
                        }),
                    "{error}"
                );
            } else {
                ensure!(missing(&marker), "{error}");
            }
        }
        Ok(())
    }
}

fn spawn_refusal(category: &Value) -> bool {
    matches!(
        category.as_str(),
        Some("executable_missing" | "permission_denied" | "spawn_failed")
    )
}

fn cancellation_record(
    id: &str,
    dispatch: &str,
    execution: Option<&str>,
    operation: Option<&str>,
) -> Value {
    json!({"category":"not_started", "dispatch":dispatch, "executionId":execution,
        "closeOperationId":operation, "launchId":id})
}
