//! Host argv and environment for the public dispatch entry point.
//! Backend clients receive this launch once, without shell interpolation.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

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
    let out = Command::new("git")
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
            if ![
                "WFC_CONTROLLER_TOKEN",
                "WFC_OWNER_TOKEN",
                "WFC_WORKER_CALLBACK",
            ]
            .iter()
            .any(|name| k.eq_ignore_ascii_case(name))
            {
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

// Match the host's existing trust decision for this directory or a real
// ancestor. Serialize router updates to the config, including symlinked files.
#[cfg(unix)]
fn claude_trust(cwd: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(config) = env::var_os("WFC_CLAUDE_CONFIG")
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|p| p.join(".claude.json")))
    else {
        return Ok(None);
    };
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
        super::write_json(&path, &config, Some(permissions))?;
    }
    Ok(ancestor)
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
        let brief = format!(
            "You are a worker dispatched by the controller (dispatch {}). Read and do the task in {}. Stop any server or background process you started before reporting. Write the report to {}. Run \"$WFC_BIN\" done --report \"$WFC_REPORT\" --attempt \"$WFC_ATTEMPT\". If you need a decision, run \"$WFC_BIN\" ask \"<question>\" [--options a,b] --attempt \"$WFC_ATTEMPT\" and end your turn. The answer arrives as your next message.",
            record["dispatch"].as_str().context("missing dispatch")?, task, report.display()
        );
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
            "claude" => {
                a.extend([brief, "--permission-mode".into(), "auto".into()]);
                if let Some(m) = self.model {
                    a.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = self.effort {
                    a.extend(["--effort".into(), e.into()]);
                }
                let session = Uuid::new_v4().to_string();
                a.extend([
                    "--name".into(),
                    record["label"].as_str().context("missing label")?.into(),
                    "--session-id".into(),
                    session.clone(),
                    "--add-dir".into(),
                ]);
                for p in writable {
                    a.push(p.to_str().context("writable path must be UTF-8")?.into());
                }
                #[cfg(unix)]
                match claude_trust(cwd) {
                    Ok(Some(ancestor)) => record["trustedBy"] = json!(ancestor),
                    Ok(None) => {}
                    Err(_) => eprintln!("wfc: could not confirm Claude worktree trust"),
                }
                record["hostSession"] = json!(session);
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
        let callback = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        record["workerCallbackHash"] = json!(super::messages::body_hash(&callback));
        launch
            .env
            .insert("WFC_WORKER_CALLBACK".into(), json!(callback));
        Ok(launch)
    }
}
