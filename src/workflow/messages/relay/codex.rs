use super::super::private_file;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub(super) fn controller(thread: &str) -> Result<String> {
    let thread = Uuid::parse_str(thread)
        .context("Codex controller thread must be a session UUID")?
        .to_string();
    if let Ok(current) = env::var("CODEX_THREAD_ID") {
        ensure!(
            current.is_empty() || current.eq_ignore_ascii_case(&thread),
            "Codex thread {thread} differs from CODEX_THREAD_ID={current}"
        );
    }
    let home = env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|p| p.join(".codex")))
        .context("missing Codex home")?;
    let root = home.join("sessions");
    let mut dirs = vec![root.clone()];
    let suffix = format!("-{thread}.jsonl");
    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            other => other?,
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push(entry.path());
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !kind.is_file() || !name.starts_with("rollout-") || !name.ends_with(&suffix) {
                continue;
            }
            let Ok(file) = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(entry.path())
            else {
                continue;
            };
            let mut line = Vec::new();
            // Only the first rollout record can identify the controller.
            BufReader::new(file)
                .take(4 * 1024 * 1024)
                .read_until(b'\n', &mut line)?;
            let Ok(meta) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if meta["type"] != "session_meta" || meta["payload"]["id"] != thread {
                continue;
            }
            ensure!(
                meta["payload"]["source"].get("subagent").is_none(),
                "Codex thread {thread} is a sub-agent"
            );
            return Ok(thread);
        }
    }
    bail!(
        "no rollout for Codex thread {thread} under {}",
        root.display()
    )
}

// Match the Python queue text, including UTF-8, JSON separators and field order.
fn queue_text(messages: &[Value]) -> String {
    let mut envelopes = Vec::with_capacity(messages.len());
    for m in messages {
        let header = [
            "id",
            "messageId",
            "dispatch",
            "type",
            "state",
            "statusVersion",
        ];
        let mut fields = Vec::new();
        let mut field = |k: &str, v: &Value| fields.push(format!("{}: {v}", json!(k)));
        for k in header {
            field(k, &m[k]);
        }
        for (k, v) in m.as_object().expect("controller message").iter() {
            if !header.contains(&k.as_str()) {
                field(k, v);
            }
        }
        field("delivery", &json!("pending"));
        field(
            "deliveryAttempt",
            &json!(format!(
                "relay:{}:{}",
                m["messageId"].as_str().unwrap(),
                m["statusVersion"]
            )),
        );
        envelopes.push(format!("{{{}}}", fields.join(", ")));
    }
    format!(
        "[router] Messages for the controller: {{\"messages\": [{}]}}",
        envelopes.join(", ")
    )
}

#[derive(PartialEq, Eq)]
pub(super) enum Outcome {
    Delivered,
    Failed,
    Missing,
    Uncertain,
}

fn start_queue(thread: &str, messages: &[Value]) -> io::Result<(Child, fs::File)> {
    let path = env::temp_dir().join(format!("wfc-queue-{}", Uuid::new_v4()));
    let output = private_file().create_new(true).open(&path)?;
    fs::remove_file(path)?;
    let mut cmd = Command::new("codex");
    cmd.args([
        "queue",
        "--thread",
        thread,
        "--message",
        &queue_text(messages),
        "--resume",
    ])
    .stdin(Stdio::null())
    .stdout(output.try_clone()?)
    .stderr(output.try_clone()?)
    .process_group(0);
    for (name, _) in env::vars_os() {
        if crate::tmux::launch::router_credential(&name.to_string_lossy()) {
            cmd.env_remove(name);
        }
    }
    let parent = std::process::id();
    // SAFETY: these Linux syscalls use only scalar arguments after fork.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    Ok((cmd.spawn()?, output))
}

fn missing_rollout(output: &mut fs::File) -> io::Result<bool> {
    output.rewind()?;
    let length = output.metadata()?.len();
    let mut lines = BufReader::new(output.take(length));
    let mut prefix = Vec::new();
    loop {
        prefix.clear();
        if lines.by_ref().take(512).read_until(b'\n', &mut prefix)? == 0 {
            return Ok(false);
        }
        let line = String::from_utf8_lossy(&prefix);
        let diagnostic = line.trim().strip_prefix("Error: ").unwrap_or(line.trim());
        if diagnostic == "no rollout found for thread"
            || diagnostic.starts_with("no rollout found for thread ")
        {
            return Ok(true);
        }
        if prefix.last() != Some(&b'\n') {
            lines.skip_until(b'\n')?;
        }
    }
}

pub(super) fn queue(thread: &str, messages: &[Value], limit: Duration) -> Result<Outcome> {
    let ids: Vec<_> = messages.iter().map(|m| m["id"].as_u64().unwrap()).collect();
    let start = Instant::now();
    let (mut child, mut output) = match start_queue(thread, messages) {
        Ok(started) => started,
        Err(e) => {
            println!("router relay: codex queue could not start for messages {ids:?}: {e}");
            return Ok(Outcome::Failed);
        }
    };
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(Outcome::Delivered);
            }
            if missing_rollout(&mut output)? {
                return Ok(Outcome::Missing);
            }
            println!("router relay: codex queue exited {status}; delivery of messages {ids:?} is uncertain");
            return Ok(Outcome::Uncertain);
        }
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            // SAFETY: the unreaped child owns this process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            child.wait()?;
            println!(
                "router relay: codex queue timed out; delivery of messages {ids:?} is uncertain"
            );
            return Ok(Outcome::Uncertain);
        }
        std::thread::sleep(left.min(Duration::from_millis(10)));
    }
}
