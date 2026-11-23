//! Host argv and environment for the public dispatch entry point.
//! Backend clients receive this launch once, without shell interpolation.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
#[cfg(target_os = "linux")]
pub(super) mod claude;
#[cfg(target_os = "linux")]
pub(crate) use claude::session_start as claude_session_start;

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

fn configured(path: &Path, name: &str, cwd: &Path) -> Result<Launch> {
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
    if command.is_empty() {
        command.push(name.to_owned());
    }
    if !prefix.is_empty() {
        let path = Path::new(&command[0]);
        let resolved = if path.components().count() > 1 || path.is_absolute() {
            cwd.join(path)
        } else {
            let search = env
                .get("PATH")
                .and_then(Value::as_str)
                .map(Into::into)
                .or_else(|| std::env::var_os("PATH"))
                .unwrap_or_default();
            std::env::split_paths(&search)
                .map(|p| cwd.join(p).join(path))
                .find(|p| p.is_file())
                .context("configured script is missing")?
        };
        ensure!(resolved.is_file(), "configured script is missing");
        command[0] = resolved
            .to_str()
            .context("script path must be UTF-8")?
            .to_owned();
    }
    prefix.extend(command);
    Ok(Launch { argv: prefix, env })
}

impl Options<'_> {
    pub fn prepare(&self, state: &Path, record: &mut Value) -> Result<Launch> {
        let cwd = Path::new(record["cwd"].as_str().context("missing cwd")?);
        let report = Path::new(record["report"].as_str().context("missing report")?);
        let mut writable = vec![
            state.to_owned(),
            report
                .parent()
                .context("missing report directory")?
                .to_owned(),
        ];
        let task = record["task"].as_str().context("missing task")?;
        let mut brief = format!(
            "You are a worker dispatched by the controller (dispatch {}). Read and do the task in {}. Stop any server or background process you started before reporting. Write the report to {}. Run \"$WFC_BIN\" done --report \"$WFC_REPORT\" --attempt \"$WFC_ATTEMPT\". If you need a decision, run \"$WFC_BIN\" ask \"<question>\" [--options a,b] --attempt \"$WFC_ATTEMPT\" and end your turn. The answer arrives as your next message.",
            record["dispatch"].as_str().context("missing dispatch")?, task, report.display()
        );
        if self.name == "opencode" && self.backend == "herdr" {
            brief.push_str(&format!(
                " Current attempt {}.",
                record["attempt"].as_str().context("missing attempt")?
            ));
        }
        let path = self.config.map(PathBuf::from).or_else(|| {
            env::var_os("WFC_LAUNCH_CONFIG")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        });
        let mut selected_config = None;
        let mut launch = match path {
            Some(path) => {
                let path = match path.strip_prefix("~/") {
                    Ok(rest) => env::home_dir()
                        .context("missing home directory")?
                        .join(rest),
                    Err(_) => path,
                };
                let path = path.canonicalize().context("invalid launch config path")?;
                selected_config = Some(path.clone());
                configured(&path, self.name, cwd).map_err(|_| {
                    anyhow::anyhow!("invalid launch config or missing configured script")
                })?
            }
            None => Launch {
                argv: vec![self.name.to_owned()],
                env: Map::new(),
            },
        };
        let a = &mut launch.argv;
        let mut ignored = Vec::new();
        match self.name {
            "codex" => {
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
                        let path = git_dir(cwd, flag).context("--git requires a Git worktree")?;
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
                a.push(cwd.to_str().unwrap().into());
                if let Some(m) = self.model {
                    a.extend(["-m".into(), m.into()]);
                }
                if let Some(e) = self.effort {
                    a.extend(["--variant".into(), e.into()]);
                }
                a.extend(["--prompt".into(), brief]);
                launch.env.insert("OPENCODE_CONFIG_CONTENT".into(),json!(json!({"permission":{"external_directory":{"*":"allow"},"read":{"*.env":"allow","*.env.*":"allow"}}}).to_string()));
            }
            #[cfg(target_os = "linux")]
            "claude" => {
                claude::prepare_initial(state, self, record, &mut launch, brief, &writable)?;
                claude::apply_existing_trust(record)?;
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
        for key in ["dispatch", "scope", "role", "attempt", "task", "report"] {
            launch.env.insert(
                format!("WFC_{}", key.to_ascii_uppercase()),
                record[key].clone(),
            );
        }
        launch.env.insert("WFC_ROUND".into(), json!("1"));
        launch.env.insert("WFC_STATE".into(), json!(state));
        launch
            .env
            .insert("WFC_BIN".into(), json!(env::current_exe()?));
        Ok(launch)
    }
}
