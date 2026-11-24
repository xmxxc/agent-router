//! Host argv and environment for the public dispatch entry point.
//! Backend clients receive this launch once, without shell interpolation.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
};
#[cfg(target_os = "linux")]
pub(super) mod claude;
#[cfg(target_os = "linux")]
pub(crate) use claude::session_start as claude_session_start;

#[cfg(target_os = "linux")]
pub(super) mod opencode;
#[cfg(target_os = "linux")]
pub(crate) use opencode::adoption::first_message as opencode_first_message;
#[cfg(target_os = "linux")]
pub(super) use opencode::adoption::preserve_pending as preserve_opencode_adoption;
#[cfg(target_os = "linux")]
pub(crate) use opencode::resume::resume_ready as opencode_resume_ready;

#[derive(Default)]
pub struct Options<'a> {
    pub name: &'a str,
    pub backend: &'a str,
    pub endpoint: Option<&'a str>,
    pub config: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub label: Option<&'a str>,
    pub git: bool,
    pub network: bool,
}

pub struct Launch {
    pub argv: Vec<String>,
    pub env: Map<String, Value>,
}

pub fn clean(value: &str) -> String {
    value
        .split(|c: char| c.is_whitespace() || c.is_ascii_control())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

fn git_dir(cwd: &Path, flag: &str) -> Result<PathBuf> {
    let mut command = Command::new("git");
    super::controller::strip_credentials(&mut command);
    let out = command
        .args(["rev-parse", flag])
        .current_dir(cwd)
        .output()?;
    ensure!(out.status.success(), "git rev-parse failed for {flag}");
    let name = std::str::from_utf8(&out.stdout)?.trim_end_matches(['\r', '\n']);
    Ok(cwd.join(name).canonicalize()?)
}

pub fn default_label(cwd: &Path, report_dir: &str) -> String {
    let common = git_dir(cwd, "--git-common-dir").ok();
    let project = common
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(cwd)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let (ticket, role) = report_dir.rsplit_once('-').unwrap_or(("", report_dir));
    clean(
        &[project.as_ref(), ticket, role]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" | "),
    )
}

fn append_env(to: &mut Map<String, Value>, from: Option<&Value>) -> Result<()> {
    if let Some(value) = from {
        for (k, v) in value.as_object().context("invalid env")? {
            ensure!(
                !k.is_empty()
                    && !k.contains(['=', '\0'])
                    && v.as_str().is_some_and(|s| !s.contains('\0')),
                "invalid env"
            );
            if !super::controller::is_credential(k) {
                to.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(())
}

fn argv(object: &Value, key: &str) -> Result<Vec<String>> {
    let Some(values) = object.get(key) else {
        return Ok(Vec::new());
    };
    let values = values.as_array().context("invalid argv")?;
    ensure!(!values.is_empty(), "empty argv");
    values
        .iter()
        .map(|v| {
            Ok(v.as_str()
                .filter(|s| !s.is_empty() && !s.contains('\0'))
                .context("invalid argument")?
                .to_owned())
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ScriptUnavailable {
    Missing,
    PermissionDenied,
}

impl ScriptUnavailable {
    pub(super) fn category(self) -> &'static str {
        match self {
            Self::Missing => "configured_script_missing",
            Self::PermissionDenied => "configured_script_permission_denied",
        }
    }
}

impl std::fmt::Display for ScriptUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "invalid launch config or missing configured script",
            Self::PermissionDenied => "permission denied while resolving configured script",
        })
    }
}

impl std::error::Error for ScriptUnavailable {}

fn resolve_script(paths: impl IntoIterator<Item = PathBuf>) -> Result<PathBuf> {
    let mut uncertain = false;
    let mut denied = false;
    for path in paths {
        match path.metadata() {
            Ok(meta) if meta.is_file() => return Ok(path),
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => (),
            Err(err) if err.kind() == ErrorKind::PermissionDenied => denied = true,
            _ => uncertain = true,
        }
    }
    if denied {
        return Err(ScriptUnavailable::PermissionDenied.into());
    }
    ensure!(!uncertain, "configured script could not be resolved");
    Err(ScriptUnavailable::Missing.into())
}

fn configured(path: &Path, name: &str, cwd: &Path) -> Result<(Launch, bool)> {
    let data: Value = serde_json::from_reader(fs::File::open(path)?)?;
    ensure!(
        data.as_object()
            .is_some_and(|m| m.keys().all(|k| matches!(k.as_str(), "env" | "hosts"))),
        "invalid config"
    );
    let mut env = Map::new();
    append_env(&mut env, data.get("env"))?;
    if let Some(hosts) = data.get("hosts") {
        for (host, settings) in hosts.as_object().context("invalid hosts")? {
            ensure!(
                matches!(host.as_str(), "codex" | "opencode" | "claude"),
                "unknown host"
            );
            ensure!(
                settings.as_object().is_some_and(|m| m
                    .keys()
                    .all(|k| matches!(k.as_str(), "env" | "runtime" | "command"))),
                "invalid host settings"
            );
            append_env(&mut Map::new(), settings.get("env"))?;
            argv(settings, "runtime")?;
            argv(settings, "command")?;
        }
    }
    let selected = &data["hosts"][name];
    append_env(&mut env, selected.get("env"))?;
    let mut prefix = argv(selected, "runtime")?;
    let mut command = argv(selected, "command")?;
    let default_command = prefix.is_empty() && command.is_empty();
    if command.is_empty() {
        command.push(name.to_owned());
    }
    if !prefix.is_empty() {
        let path = Path::new(&command[0]);
        let resolved = if path.components().count() > 1 || path.is_absolute() {
            resolve_script([cwd.join(path)])?
        } else {
            let search = env
                .get("PATH")
                .and_then(Value::as_str)
                .map(Into::into)
                .or_else(|| std::env::var_os("PATH"))
                .unwrap_or_default();
            resolve_script(std::env::split_paths(&search).map(|p| cwd.join(p).join(path)))?
        };
        command[0] = resolved
            .to_str()
            .context("script path must be UTF-8")?
            .to_owned();
    }
    prefix.extend(command);
    Ok((Launch { argv: prefix, env }, default_command))
}

pub(super) const FIRST_BRIEF: &str = "You are a worker dispatched by the controller";

pub(super) fn worker_brief(record: &Value, opening: &str) -> Result<String> {
    let task = record["task"].as_str().context("missing task")?;
    let report = record["report"].as_str().context("missing report")?;
    let dispatch = record["dispatch"].as_str().context("missing dispatch")?;
    Ok(format!(
        "{opening} (dispatch {dispatch}). Read and do the task in {task}. Stop any server or background process you started before reporting. Write the report to {report}. Run \"$WFC_BIN\" done --report \"$WFC_REPORT\" --attempt \"$WFC_ATTEMPT\". If you need a decision, run \"$WFC_BIN\" ask \"<question>\" [--options a,b] --attempt \"$WFC_ATTEMPT\" and end your turn. The answer arrives as your next message."
    ))
}

#[derive(Clone, Copy)]
enum WorkerStart<'a> {
    Initial,
    Resume(&'a str),
    #[cfg(target_os = "linux")]
    Handoff,
}

impl Options<'_> {
    pub fn prepare(&self, state: &Path, record: &mut Value) -> Result<Launch> {
        self.prepare_worker(state, record, None)
    }

    pub(super) fn prepare_worker(
        &self,
        state: &Path,
        record: &mut Value,
        resume: Option<&str>,
    ) -> Result<Launch> {
        let start = resume.map_or(WorkerStart::Initial, WorkerStart::Resume);
        self.prepare_launch(state, record, start, None)
            .map(|(launch, _)| launch)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn prepare_handoff(&self, state: &Path, record: &mut Value) -> Result<Launch> {
        self.prepare_launch(state, record, WorkerStart::Handoff, None)
            .map(|(launch, _)| launch)
    }
    // The resume transaction owns publication and the subsequent terminal input.
    #[cfg(target_os = "linux")]
    pub fn prepare_opencode_resume(
        state: &Path,
        original: &Value,
        next: &mut Value,
        opening: &str,
    ) -> Result<(Launch, String)> {
        ensure!(
            original["host"] == "opencode" && original["backend"] == "tmux",
            "expected an OpenCode tmux assignment"
        );
        ensure!(
            original["dispatch"].is_string()
                && original["dispatch"] == next["dispatch"]
                && original["cwd"] == next["cwd"]
                && next["attempt"]
                    .as_str()
                    .is_some_and(|id| super::valid_identity(id, "a-"))
                && original["attempt"]
                    .as_str()
                    .is_some_and(|id| super::valid_identity(id, "a-"))
                && original["attempt"] != next["attempt"],
            "OpenCode resume requires the original dispatch and directory"
        );
        let (launch, input) = Options {
            name: "opencode",
            backend: "tmux",
            config: original["launchConfig"].as_str(),
            model: original["model"].as_str(),
            effort: original["effort"].as_str(),
            git: original["git"] == true,
            ..Options::default()
        }
        .prepare_launch(state, next, WorkerStart::Resume(opening), Some(original))?;
        Ok((launch, input.context("missing OpenCode resume input")?))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn prepare_claude_resume(
        &self,
        state: &Path,
        previous: &Value,
        next: &mut Value,
        opening: &str,
    ) -> Result<(Launch, claude::ResumeProof)> {
        let cwd = PathBuf::from(next["cwd"].as_str().context("missing cwd")?);
        let (mut launch, config, _) = self.launch_prefix(&cwd, false)?;
        ensure!(
            previous["launchConfig"] == json!(config),
            "configured Claude launch config differs from the original dispatch"
        );
        let brief = worker_brief(next, opening)?;
        let writable = writable_paths(state, next)?;
        let proof = claude::prepare_resume(state, previous, next, &mut launch, brief, &writable)?;
        bind_environment(state, next, &mut launch)?;
        Ok((launch, proof))
    }

    fn launch_prefix(
        &self,
        cwd: &Path,
        allow_default_config: bool,
    ) -> Result<(Launch, Option<PathBuf>, bool)> {
        let path = self.config.map(PathBuf::from).or_else(|| {
            if !allow_default_config {
                return None;
            }
            env::var_os("WFC_LAUNCH_CONFIG")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        });
        let mut selected_config = None;
        let (launch, default_command) = match path {
            Some(path) => {
                let path = match path.strip_prefix("~/") {
                    Ok(rest) => env::home_dir()
                        .context("missing home directory")?
                        .join(rest),
                    Err(_) => path,
                };
                let path = path.canonicalize().context("invalid launch config path")?;
                selected_config = Some(path.clone());
                configured(&path, self.name, cwd).map_err(|err| {
                    if err.is::<ScriptUnavailable>() {
                        err
                    } else {
                        anyhow::anyhow!("invalid launch config or missing configured script")
                    }
                })?
            }
            None => (
                Launch {
                    argv: vec![self.name.to_owned()],
                    env: Map::new(),
                },
                true,
            ),
        };
        Ok((launch, selected_config, default_command))
    }

    fn prepare_launch(
        &self,
        state: &Path,
        record: &mut Value,
        start: WorkerStart<'_>,
        original: Option<&Value>,
    ) -> Result<(Launch, Option<String>)> {
        let cwd = PathBuf::from(record["cwd"].as_str().context("missing cwd")?);
        let mut writable = writable_paths(state, record)?;
        let opening = match start {
            WorkerStart::Initial => FIRST_BRIEF,
            WorkerStart::Resume(opening) => opening,
            #[cfg(target_os = "linux")]
            WorkerStart::Handoff => "[router] This task has been handed off to a new host",
        };
        let resume = matches!(start, WorkerStart::Resume(_));
        let mut brief = worker_brief(record, opening)?;
        if self.name == "opencode" {
            brief.push_str(&format!(
                " Current attempt {}.",
                record["attempt"].as_str().context("missing attempt")?
            ));
        }
        let (mut launch, selected_config, default_command) =
            self.launch_prefix(&cwd, matches!(start, WorkerStart::Initial))?;
        #[cfg(not(target_os = "linux"))]
        let _ = default_command;
        if let Some(original) = original {
            ensure!(
                original["launchConfig"] == json!(selected_config),
                "configured OpenCode launch config differs from the original dispatch"
            );
        }
        let a = &mut launch.argv;
        let mut ignored = Vec::new();
        let mut resume_input = None;
        match self.name {
            "codex" => {
                if resume {
                    a.push("resume".into());
                }
                a.extend(
                    ["--no-alt-screen", "-s", "workspace-write", "-a", "never"].map(String::from),
                );
                if let Some(m) = self.model {
                    a.extend(["-m".into(), m.into()]);
                }
                if let Some(e) = self.effort {
                    a.extend(["-c".into(), format!("model_reasoning_effort={e}")]);
                }
                if self.network {
                    a.extend(
                        ["-c", "sandbox_workspace_write.network_access=true"].map(String::from),
                    );
                }
                if self.git {
                    for flag in ["--git-dir", "--git-common-dir"] {
                        let path = git_dir(&cwd, flag).context("--git requires a Git worktree")?;
                        if !writable.contains(&path) {
                            writable.push(path);
                        }
                    }
                }
                for p in writable {
                    a.extend([
                        "--add-dir".into(),
                        p.to_str().context("writable path must be UTF-8")?.into(),
                    ]);
                }
                a.push(brief);
            }
            "opencode" => {
                #[cfg(target_os = "linux")]
                ensure!(
                    self.backend != "tmux"
                        || !matches!(start, WorkerStart::Handoff)
                        || !opencode::effective_setting(&launch.env, "WFC_OPENCODE_ISOLATED_STORE")
                            .is_some_and(|value| value == Path::new("1")),
                    "handoff does not support a private OpenCode opening"
                );
                #[cfg(target_os = "linux")]
                let session = opencode::prepare(state, &mut launch.env, record, original, &brief)?;
                #[cfg(not(target_os = "linux"))]
                let session: Option<String> = None;
                let cwd = record["cwd"].as_str().context("missing cwd")?;
                ensure!(
                    Path::new(cwd).is_absolute(),
                    "OpenCode requires an absolute cwd"
                );
                a.push(cwd.into());
                if let Some(effort) = self.effort {
                    record["effort"] = json!(effort);
                }
                if let Some(m) = self.model {
                    a.extend(["-m".into(), m.into()]);
                }
                if let Some(e) = self.effort {
                    a.extend(["--variant".into(), e.into()]);
                }
                if let Some(session) = session {
                    a.extend(["-s".into(), session]);
                    resume_input = Some(brief);
                } else {
                    a.extend(["--prompt".into(), brief]);
                }
                let controlled = original.is_some() || record["opencodeOpening"].is_object();
                #[cfg(target_os = "linux")]
                let use_default_config = !controlled
                    || [
                        "OPENCODE_CONFIG_CONTENT",
                        "OPENCODE_CONFIG",
                        "OPENCODE_CONFIG_DIR",
                    ]
                    .iter()
                    .all(|key| opencode::effective_setting(&launch.env, key).is_none());
                #[cfg(not(target_os = "linux"))]
                let use_default_config = !controlled;
                #[cfg(target_os = "linux")]
                if original.is_none() && record["opencodeOpening"].is_object() {
                    opencode::adoption::configure(state, &mut launch.env, record)?;
                }
                if use_default_config {
                    let config = json!({
                        "permission":{"external_directory":{"*":"allow"},
                                      "read":{"*.env":"allow","*.env.*":"allow"}}
                    });
                    launch
                        .env
                        .insert("OPENCODE_CONFIG_CONTENT".into(), json!(config.to_string()));
                }
            }
            #[cfg(target_os = "linux")]
            "claude" => {
                claude::prepare_initial(state, self, record, &mut launch, brief, &writable)?;
                if default_command {
                    claude::apply_existing_trust(record, &mut launch)?;
                }
            }
            _ => bail!("unsupported host"),
        }
        if self.name != "codex" {
            let display = if self.name == "claude" {
                "Claude Code"
            } else {
                "OpenCode"
            };
            if self.network {
                ignored.push(format!(
                    "--network: {display} is not network-sandboxed; ignored"
                ));
            }
            if self.git {
                ignored.push(format!(
                    "--git: {display} can write the git directories anyway; ignored"
                ));
            }
        }
        if !ignored.is_empty() {
            record["ignored"] = json!(ignored);
        }
        if let Some(path) = selected_config {
            record["launchConfig"] = json!(path);
        }
        #[cfg(target_os = "linux")]
        if self.name == "codex" && matches!(self.backend, "tmux" | "herdr") {
            match codex_home(&cwd, &launch.env) {
                Ok(home) => {
                    record["codexHome"] = json!(home);
                    if resume || self.backend == "herdr" {
                        launch.env.insert("CODEX_HOME".into(), json!(home));
                    }
                }
                Err(error) if resume => return Err(error),
                Err(_) => {}
            }
            if !resume {
                record["codexBriefHash"] = json!(super::messages::body_hash(
                    launch.argv.last().context("missing Codex brief")?
                ));
            }
        }
        bind_environment(state, record, &mut launch)?;
        Ok((launch, resume_input))
    }
}

fn writable_paths(state: &Path, record: &Value) -> Result<Vec<PathBuf>> {
    let report = Path::new(record["report"].as_str().context("missing report")?);
    Ok(vec![
        state.to_owned(),
        report
            .parent()
            .context("missing report directory")?
            .to_owned(),
    ])
}

fn bind_environment(state: &Path, record: &Value, launch: &mut Launch) -> Result<()> {
    for key in ["dispatch", "scope", "role", "attempt", "task", "report"] {
        launch.env.insert(
            format!("WFC_{}", key.to_ascii_uppercase()),
            record[key].clone(),
        );
    }
    launch.env.insert(
        "WFC_ROUND".into(),
        json!(record["round"]
            .as_u64()
            .context("missing round")?
            .to_string()),
    );
    launch.env.insert("WFC_STATE".into(), json!(state));
    launch
        .env
        .insert("WFC_BIN".into(), json!(env::current_exe()?));
    Ok(())
}

#[cfg(target_os = "linux")]
fn codex_home(cwd: &Path, settings: &Map<String, Value>) -> Result<PathBuf> {
    let effective = |key| {
        settings
            .get(key)
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| env::var_os(key).map(PathBuf::from))
            .filter(|p| !p.as_os_str().is_empty())
    };
    let path = match effective("CODEX_HOME") {
        Some(path) => path,
        None => effective("HOME")
            .or_else(env::home_dir)
            .context("missing home directory")?
            .join(".codex"),
    };
    let path = cwd.join(path);
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path,
        Err(e) => return Err(e).context("resolve Codex home"),
    };
    ensure!(path.to_str().is_some(), "Codex home must be UTF-8");
    Ok(path)
}

#[cfg(target_os = "linux")]
pub(super) fn matches_unresolved_codex_home(saved: &Path, current: &Path) -> bool {
    use std::path::Component;

    // A home saved before creation can contain "." as well as "..".
    // Accept alternate spellings only after checking the actual directories.
    if !saved.is_absolute() {
        return false;
    }
    let mut checked = PathBuf::new();
    for part in saved.components() {
        if part == Component::ParentDir {
            checked.pop();
            continue;
        }
        checked.push(part);
        // Check before popping any component. A symlink followed by ".."
        // resolves relative to the symlink's target, not its spelling.
        if matches!(part, Component::Normal(_))
            && !fs::symlink_metadata(&checked).is_ok_and(|metadata| metadata.is_dir())
        {
            return false;
        }
    }
    checked.as_path() == current && saved.canonicalize().is_ok_and(|path| path == checked)
}
