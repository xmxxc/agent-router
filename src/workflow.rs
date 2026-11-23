mod git_snapshot;
pub mod messages;

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

pub const DISPATCH_USAGE: &str =
    "wfc dispatch --role ROLE --task FILE --cwd DIR [--report FILE] [--scope NAME] -- PROGRAM [ARG ...]";
pub const STATUS_USAGE: &str = "wfc status [--dispatch ID]";

pub struct Dispatch<'a> {
    role: &'a str,
    task: &'a Path,
    cwd: &'a Path,
    report: Option<&'a Path>,
    scope: Option<&'a str>,
    worker: &'a [OsString],
}

impl<'a> Dispatch<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let split = args
            .iter()
            .position(|a| a == "--")
            .context("missing worker command after --")?;
        let (options, tail) = args.split_at(split);
        let worker = &tail[1..];
        ensure!(
            worker.first().is_some_and(|v| !v.is_empty()),
            "missing worker program"
        );
        let (mut role, mut task, mut cwd) = (None, None, None);
        let (mut report, mut scope) = (None, None);
        for pair in options.chunks(2) {
            let [flag, value] = pair else {
                bail!("option requires a value")
            };
            ensure!(!value.is_empty(), "empty option value");
            let slot = match flag.to_str() {
                Some("--role") => &mut role,
                Some("--task") => &mut task,
                Some("--cwd") => &mut cwd,
                Some("--scope") => &mut scope,
                Some("--report") => &mut report,
                _ => bail!("unknown dispatch option {:?}", flag),
            };
            ensure!(slot.replace(value).is_none(), "duplicate option {:?}", flag);
        }
        let role = role
            .context("missing --role")?
            .to_str()
            .context("role must be UTF-8")?;
        ensure!(!role.trim().is_empty(), "empty role");
        Ok(Self {
            role,
            task: Path::new(task.context("missing --task")?),
            cwd: Path::new(cwd.context("missing --cwd")?),
            scope: scope
                .map(|v| v.to_str().context("scope must be UTF-8"))
                .transpose()?,
            report: report.map(Path::new),
            worker,
        })
    }

    pub fn run(self) -> Result<Value> {
        let cwd = self
            .cwd
            .canonicalize()
            .context("resolve worker directory")?;
        ensure!(cwd.is_dir(), "--cwd must name a directory");
        let task = self.task.canonicalize().context("resolve task file")?;
        ensure!(task.is_file(), "--task must name a file");

        // Resolve an explicit relative executable before changing to the worker's cwd.
        let program = Path::new(&self.worker[0]);
        let program = if program.is_relative() && program.components().count() > 1 {
            std::path::absolute(program)?
        } else {
            program.to_owned()
        };
        let state = state_dir()?;
        let records = state.join("dispatches");
        fs::create_dir_all(&records).context("create dispatch state directory")?;
        let dispatch = format!("d-{}", Uuid::new_v4().simple());
        let attempt = format!("a-{}", Uuid::new_v4().simple());
        let round = 1;
        let scope = self
            .scope
            .unwrap_or("")
            .split(|c: char| c.is_whitespace() || c.is_ascii_control())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let scope = if scope.is_empty() {
            format!("s-{}", Uuid::new_v4().simple())
        } else {
            scope.chars().take(120).collect()
        };
        let default_report = cwd.join("report.md");
        let report = report_destination(self.report.unwrap_or(&default_report))?;
        let record = json!({
            "dispatch": dispatch, "round": round, "attempt": attempt, "role": self.role,
            "report": report.to_str().context("report path must be UTF-8")?, "scope": scope,
            "task": task.to_str().context("task path must be UTF-8")?,
            "cwd": cwd.to_str().context("worker directory must be UTF-8")?,
        });

        // Reserve and write the record before starting the worker. Status sees it
        // only after a successful handoff, when the rename publishes it in one step.
        let pending = records.join(format!("{dispatch}.pending"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&pending).context("reserve dispatch record")?;
        let result = (|| -> Result<()> {
            writeln!(file, "{record}")?;
            file.sync_all()?;
            drop(file);
            let exit = Command::new(program)
                .args(&self.worker[1..])
                .current_dir(&cwd)
                .env("WFC_STATE", &state)
                .env("WFC_DISPATCH", &dispatch)
                .env("WFC_ROUND", round.to_string())
                .env("WFC_ATTEMPT", &attempt)
                .env("WFC_ROLE", self.role)
                .env("WFC_TASK", &task)
                .env("WFC_REPORT", &report)
                .env("WFC_SCOPE", &scope)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .context("start controlled worker")?;
            ensure!(exit.success(), "worker for {dispatch} exited with {exit}");
            Ok(())
        })();
        if let Err(err) = result {
            fs::remove_file(&pending).with_context(|| {
                format!("{err:#}; removing unpublished dispatch {dispatch} also failed")
            })?;
            return Err(err);
        }
        fs::rename(&pending, records.join(format!("{dispatch}.json"))).with_context(|| {
            format!("worker succeeded but publishing dispatch {dispatch} failed")
        })?;
        Ok(record)
    }
}

// The worker usually creates the file later. Bind its resolved parent now so
// callbacks from another current directory or via a symlink compare equally.
fn report_destination(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let report = path.canonicalize().context("resolve report destination")?;
            ensure!(report.is_file(), "--report must name a file");
            Ok(report)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let parent = path.parent().context("missing report directory")?;
            let name = path.file_name().context("missing report filename")?;
            fs::create_dir_all(parent).context("create report directory")?;
            Ok(parent.canonicalize()?.join(name))
        }
        Err(e) => Err(e).context("inspect report destination"),
    }
}

fn state_dir() -> Result<PathBuf> {
    let root = match env::var_os("WFC_STATE") {
        Some(dir) => {
            ensure!(!dir.is_empty(), "empty WFC_STATE");
            PathBuf::from(dir)
        }
        None => env::home_dir()
            .context("set WFC_STATE to a state directory")?
            .join(".local/state/agent-router"),
    };
    Ok(std::path::absolute(root)?)
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("d-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn read_record(path: &Path) -> Result<Value> {
    let file =
        File::open(path).with_context(|| format!("open dispatch record {}", path.display()))?;
    serde_json::from_reader(file)
        .with_context(|| format!("read dispatch record {}", path.display()))
}

pub fn status(id: Option<&OsStr>) -> Result<Value> {
    let state = state_dir()?;
    let dir = state.join("dispatches");
    let mut dispatches = Vec::new();
    if let Some(id) = id {
        let id = id
            .to_str()
            .filter(|s| valid_id(s))
            .context("invalid dispatch ID")?;
        let history = messages::history(&state, id)?;
        let mut record = match (read_record(&dir.join(format!("{id}.json"))), history.last()) {
            (Ok(record), _) => record,
            // A worker can publish a receipt before its foreground handoff
            // finishes, even if it subsequently exits with an error.
            (Err(e), Some(last))
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
            {
                json!({"dispatch": id, "round": last["round"], "role": last["role"], "attempt": last["attempt"]})
            }
            (Err(e), _) => return Err(e),
        };
        record["messages"] = Value::Array(history);
        dispatches.push(record);
    } else {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(json!({"dispatches": []})),
            Err(err) => return Err(err).context("list dispatch records"),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension() == Some(OsStr::new("json"))
                && path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .is_some_and(valid_id)
            {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            dispatches.push(read_record(&path)?);
        }
    }
    Ok(json!({ "dispatches": dispatches }))
}
