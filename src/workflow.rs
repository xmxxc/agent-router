mod git_snapshot;
pub mod messages;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::process::Stdio;
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod worker_owner;

mod assignment;
mod backend;
pub mod host;
mod view;

pub const DISPATCH_USAGE: &str =
    "wfc dispatch --host {codex,opencode,claude} --task FILE --cwd DIR --report FILE [--backend {tmux,herdr,bundled}] [--herdr-endpoint PATH] [--launch-config FILE] [--scope S] [--role R] [--model M] [--effort E] [--label L] [--git] [--network]\n    wfc dispatch --role ROLE --task FILE --cwd DIR [--report FILE] [--scope S] -- PROGRAM [ARG ...]";
pub const STATUS_USAGE: &str = "wfc status [--dispatch ID | --active]";

pub struct Dispatch<'a> {
    role: Option<&'a str>,
    host: Option<host::Options<'a>>,
    task: &'a Path,
    cwd: &'a Path,
    report: Option<&'a Path>,
    scope: Option<&'a str>,
    worker: &'a [OsString],
}

impl<'a> Dispatch<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let split = args.iter().position(|v| v == "--");
        let (options, worker) = match split {
            Some(i) => {
                let tail = &args[i + 1..];
                ensure!(
                    tail.first().is_some_and(|v| !v.is_empty()),
                    "missing worker program"
                );
                (&args[..i], tail)
            }
            None => (args, &args[0..0]),
        };
        let mut values = std::collections::BTreeMap::new();
        let mut flags = std::collections::BTreeSet::new();
        let mut it = options.iter();
        while let Some(flag) = it.next() {
            let flag = flag.to_str().context("invalid option")?;
            if matches!(flag, "--git" | "--network") {
                ensure!(flags.insert(flag), "duplicate {flag}");
                continue;
            }
            ensure!(
                matches!(
                    flag,
                    "--role"
                        | "--task"
                        | "--cwd"
                        | "--scope"
                        | "--report"
                        | "--host"
                        | "--backend"
                        | "--label"
                        | "--herdr-endpoint"
                        | "--model"
                        | "--effort"
                        | "--launch-config"
                        | "--owner-token"
                ),
                "unknown dispatch option {flag}"
            );
            let value = it
                .next()
                .filter(|v| !v.is_empty() && !v.as_encoded_bytes().starts_with(b"--"))
                .with_context(|| format!("missing value for {flag}"))?;
            ensure!(values.insert(flag, value).is_none(), "duplicate {flag}");
        }
        let string = |key: &str| {
            values
                .get(key)
                .map(|v| {
                    v.to_str()
                        .with_context(|| format!("invalid UTF-8 for {key}"))
                })
                .transpose()
        };
        ensure!(
            !values.contains_key("--owner-token"),
            "controller owner authorization is unavailable"
        );
        let role = string("--role")?;
        let report = values.get("--report").copied().map(Path::new);
        let host = if worker.is_empty() {
            let name = string("--host")?.context("missing --host or worker command")?;
            ensure!(
                matches!(name, "codex" | "opencode" | "claude"),
                "invalid --host"
            );
            let backend = string("--backend")?.unwrap_or("tmux");
            ensure!(
                matches!(backend, "tmux" | "herdr" | "bundled"),
                "invalid --backend"
            );
            let endpoint = string("--herdr-endpoint")?;
            ensure!(
                endpoint.is_none() || backend == "herdr",
                "--herdr-endpoint requires --backend herdr"
            );
            ensure!(report.is_some(), "missing --report");
            Some(host::Options {
                name,
                backend,
                endpoint,
                config: string("--launch-config")?,
                model: string("--model")?,
                effort: string("--effort")?,
                label: string("--label")?,
                git: flags.contains("--git"),
                network: flags.contains("--network"),
            })
        } else {
            ensure!(
                role.is_some_and(|r| !r.trim().is_empty()),
                "missing or empty --role"
            );
            ensure!(
                flags.is_empty()
                    && values.keys().all(|k| matches!(
                        *k,
                        "--role" | "--task" | "--cwd" | "--report" | "--scope"
                    )),
                "host options require public dispatch without -- PROGRAM"
            );
            None
        };
        Ok(Self {
            role,
            host,
            task: Path::new(values.get("--task").copied().context("missing --task")?),
            cwd: Path::new(values.get("--cwd").copied().context("missing --cwd")?),
            report,
            scope: string("--scope")?,
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

        let state = state_dir()?;
        let records = state.join("dispatches");
        fs::create_dir_all(&records).context("create dispatch state directory")?;
        let dispatch = format!("d-{}", Uuid::new_v4().simple());
        let attempt = format!("a-{}", Uuid::new_v4().simple());
        let round = 1;
        let scope = host::clean(self.scope.unwrap_or(""));
        let scope = if scope.is_empty() {
            format!("s-{}", Uuid::new_v4().simple())
        } else {
            scope.chars().take(120).collect()
        };
        let default_report = cwd.join(format!("report-{dispatch}.md"));
        let report = report_destination(self.report.unwrap_or(&default_report))?;
        let report_dir = report
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
            .unwrap_or("");
        let role = if self.host.is_some() {
            let role = host::clean(self.role.unwrap_or(""));
            if role.is_empty() {
                report_dir
                    .rsplit('-')
                    .next()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("worker")
                    .to_owned()
            } else {
                role
            }
        } else {
            self.role.unwrap().to_owned()
        };
        let mut record = json!({
            "dispatch": dispatch, "round": round, "attempt": attempt, "role": role,
            "report": report.to_str().context("report path must be UTF-8")?, "scope": scope,
            "task": task.to_str().context("task path must be UTF-8")?,
            "cwd": cwd.to_str().context("worker directory must be UTF-8")?,
        });

        record["created"] = json!(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64());
        let launch = if let Some(host) = &self.host {
            let label = host::clean(host.label.unwrap_or(""));
            record["label"] = json!(if label.is_empty() {
                host::default_label(&cwd, report_dir)
            } else {
                label
            });
            record["host"] = json!(host.name);
            record["backend"] = json!(host.backend);
            record["model"] = json!(host.model);
            record["git"] = json!(host.git);
            record["tmux"] = json!(format!("wfc-{dispatch}"));
            if let Some(endpoint) = host.endpoint {
                record["endpoint"] = json!(endpoint);
            }
            ensure!(
                cfg!(target_os = "linux"),
                "public backend is unavailable on this platform"
            );
            ensure!(
                host.backend == "tmux",
                "{} backend adapter is unavailable in this build",
                host.backend
            );
            #[cfg(target_os = "linux")]
            {
                record["tmuxSocket"] = json!(backend::tmux_socket()?);
            }
            record["executionId"] = record["attempt"].clone();
            Some(host.prepare(&state, &mut record)?)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        {
            record["worker"] = json!({"state":"running"});
        }

        let pending = assignment::reserve(&records, &dispatch, &record)?;
        let mut preserve_binding = false;
        let result = (|| -> Result<()> {
            if let Some(launch) = launch {
                let mut answer = backend::create(&state, &record, launch)?;
                let accepted = answer["result"] == "accepted";
                if accepted || answer["result"] == "uncertain" {
                    preserve_binding = true;
                    // Bind the launch receipt to the assignment that requested it.
                    answer["dispatch"] = record["dispatch"].clone();
                    answer["attempt"] = record["attempt"].clone();
                    answer["round"] = record["round"].clone();
                    record["create"] = answer.clone();
                    let published = records.join(format!("{dispatch}.json"));
                    if let Err(e) = write_json(&published, &record, None) {
                        // A live host may already be using this identity. Keep its
                        // original assignment reachable for inspection and close.
                        fs::rename(&pending, &published).with_context(|| format!("publishing {dispatch} failed: {e}; retaining its pending assignment"))?;
                        return Err(e).with_context(|| {
                            format!("could not publish launch evidence for {dispatch}")
                        });
                    }
                    fs::remove_file(&pending)?;
                }
                ensure!(
                    accepted,
                    "backend create {} for {dispatch}: {}",
                    answer["result"],
                    answer["diagnostic"]
                );
                return Ok(());
            }
            // Resolve an explicit relative executable before changing to the worker's cwd.
            let program = Path::new(&self.worker[0]);
            let program = if program.is_relative() && program.components().count() > 1 {
                std::path::absolute(program)?
            } else {
                program.to_owned()
            };

            #[cfg(target_os = "linux")]
            let mut command = worker_owner::command(&program)?;
            #[cfg(not(target_os = "linux"))]
            let mut command = Command::new(program);
            command
                .args(&self.worker[1..])
                .current_dir(&cwd)
                .env("WFC_STATE", &state)
                .env("WFC_DISPATCH", &dispatch)
                .env("WFC_ROUND", round.to_string())
                .env("WFC_ATTEMPT", &attempt)
                .env("WFC_ROLE", &role)
                .env("WFC_TASK", &task)
                .env("WFC_SCOPE", &scope)
                .env("WFC_REPORT", &report)
                .stdin(Stdio::null());
            #[cfg(target_os = "linux")]
            worker_owner::launch(command)?;
            #[cfg(not(target_os = "linux"))]
            {
                let exit = command
                    .stdout(Stdio::null())
                    .status()
                    .context("start controlled worker")?;
                ensure!(exit.success(), "worker for {dispatch} exited with {exit}");
            }
            Ok(())
        })();
        if let Err(err) = result {
            if preserve_binding {
                return Err(err);
            }
            fs::remove_file(&pending).with_context(|| {
                format!("{err:#}; removing unpublished dispatch {dispatch} also failed")
            })?;
            return Err(err);
        }
        #[cfg(not(target_os = "linux"))]
        fs::rename(&pending, records.join(format!("{dispatch}.json"))).with_context(|| {
            format!("worker succeeded but publishing dispatch {dispatch} failed")
        })?;
        Ok(record)
    }
}

// Publish a complete generation while callback readers can still use .pending.
fn write_json(path: &Path, value: &Value, permissions: Option<fs::Permissions>) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&temp)?;
    let result = (|| -> Result<()> {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        serde_json::to_writer(&file, value)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

// Resolve links before creating directories: even a link in the middle of the
// assigned path may point to a directory that the worker does not have yet.
fn follow_report_links(resolved: &mut PathBuf, path: &Path, remaining: &mut usize) -> Result<()> {
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            resolved.pop();
            continue;
        }
        resolved.push(component);
        if matches!(component, Component::Normal(_)) && resolved.is_symlink() {
            *remaining = remaining
                .checked_sub(1)
                .context("too many symbolic links in report path")?;
            let target = fs::read_link(&*resolved).context("read report link")?;
            resolved.pop();
            follow_report_links(resolved, &target, remaining)?;
        }
    }
    Ok(())
}

fn resolve_report_path(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut remaining_links = 40;
    follow_report_links(
        &mut resolved,
        &std::path::absolute(path)?,
        &mut remaining_links,
    )?;
    Ok(resolved)
}

fn report_destination(path: &Path) -> Result<PathBuf> {
    let resolved = resolve_report_path(path)?;
    match resolved.canonicalize() {
        Ok(report) => {
            ensure!(report.is_file(), "--report must name a file");
            Ok(report)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let parent = resolved.parent().context("missing report directory")?;
            let name = resolved.file_name().context("missing report filename")?;
            fs::create_dir_all(parent).context("create report directory")?;
            Ok(parent.canonicalize()?.join(name))
        }
        Err(e) => Err(e).context("resolve report destination"),
    }
}

pub(crate) fn state_dir() -> Result<PathBuf> {
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
    valid_identity(id, "d-")
}

fn valid_identity(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|hex| {
        (hex.len() == 32
            || (prefix == "d-" && hex.len() == 8)
            || (prefix == "a-" && hex.len() == 12))
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn read_record(path: &Path) -> Result<Value> {
    let file =
        File::open(path).with_context(|| format!("open dispatch record {}", path.display()))?;
    decode_record(file, path)
}

fn decode_record(file: File, path: &Path) -> Result<Value> {
    let mut record: Value = serde_json::from_reader(file)
        .with_context(|| format!("read dispatch record {}", path.display()))?;
    ensure!(
        record["dispatch"]
            .as_str()
            .is_some_and(|id| path.file_stem() == Some(OsStr::new(id))),
        "dispatch record identity mismatch in {}",
        path.display()
    );
    // Hosted assignments predate the explicit backend column.
    if record["host"].is_string() && (record["backend"].is_null() || record["backend"] == "") {
        record["backend"] = json!("tmux");
    }
    Ok(record)
}

pub fn status(id: Option<&OsStr>, active: bool) -> Result<Value> {
    status_with_history(id, active, messages::history)
}

// The integrated message ledger supplies #7's read-only messages::history.
fn status_with_history(
    id: Option<&OsStr>,
    active: bool,
    history: impl Fn(&Path, &str) -> Result<Vec<Value>>,
) -> Result<Value> {
    let state = state_dir()?;
    let dir = state.join("dispatches");
    let mut dispatches = Vec::new();
    let mut named_history = None;
    if let Some(id) = id {
        let id = id
            .to_str()
            .filter(|s| valid_id(s))
            .context("invalid dispatch ID")?;
        let messages = history(&state, id)?;
        let row = match assignment::public_record(&dir, id)? {
            Some(row) => row,
            None => {
                let last = messages
                    .last()
                    .with_context(|| format!("unknown dispatch {id}"))?;
                json!({"dispatch":id,"round":last["round"],"attempt":last["attempt"],"scope":last["scope"],"role":last["role"],"report":last["report"]})
            }
        };
        dispatches.push(row);
        named_history = Some(messages);
    } else {
        for id in assignment::ids(&dir)? {
            if let Some(row) = assignment::public_record(&dir, &id)? {
                dispatches.push(row);
            }
        }
    }
    let mut undelivered = Vec::new();
    let mut reclaimable = Vec::new();
    for record in &mut dispatches {
        if record["createPending"] == true && record["publicationDiagnostic"].is_string() {
            record["worker"] = json!({"state":"unknown"});
            record["backendResult"] = json!("uncertain");
            record["backendAlive"] = Value::Null;
        } else if record["host"].is_string() {
            let (worker, inspection) = backend::observe(&state, record)?;
            if let Some(effect) = inspection.get("closeEffect") {
                record["closeEffect"] = effect.clone();
            }
            record["worker"] = worker;
            record["backendResult"] = inspection["result"].clone();
            record["backendAlive"] = if inspection["alive"] == true && inspection["owned"] != true {
                Value::Null
            } else {
                inspection["alive"].clone()
            };
        } else {
            #[cfg(target_os = "linux")]
            if record.get("worker").is_some() {
                record["worker"] = match worker_owner::status(&dir, record) {
                    Ok(worker) => worker,
                    Err(_) if record["createPending"] == true => json!({"state":"unknown"}),
                    Err(e) => return Err(e),
                };
            }
            record["backendAlive"] = match record["worker"]["state"].as_str() {
                Some("running") => json!(true),
                Some("unknown") => Value::Null,
                _ => json!(false),
            };
        }
        let messages = match named_history.take() {
            Some(messages) => messages,
            None => history(
                &state,
                record["dispatch"].as_str().context("missing dispatch")?,
            )?,
        };
        let messages = view::merged_history(record, messages)?;
        undelivered.extend(
            messages
                .iter()
                .filter(|m| m["to"] == "controller" && m["state"] == "unsent")
                .cloned(),
        );
        let alive = record["backendAlive"].as_bool();
        if record["createPending"] == true && !assignment::identified(record) {
            for field in ["state", "status", "lifecycle"] {
                record[field] = json!("unknown");
            }
            record["rounds"] = json!([]);
            if id.is_some() {
                record["messages"] = json!(messages);
            }
        } else {
            view::project(record, &messages, alive, id.is_some())?;
        }
        if active && record["state"] == "done" && alive == Some(true) {
            reclaimable.push(json!({"dispatch":record["dispatch"],"label":record["label"],"tmux":record["tmux"]}));
        }
    }
    if active {
        dispatches.retain(|r| r["state"] != "done");
    }
    undelivered.sort_by_key(|v| v["id"].as_u64());
    let mut response =
        json!({"state":state,"dispatches":dispatches,"undelivered":undelivered,"waiters":[]});
    if active {
        response["reclaimable"] = json!(reclaimable);
    }
    Ok(response)
}
