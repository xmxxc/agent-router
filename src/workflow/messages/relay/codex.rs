use super::super::spaced_json;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, BufRead, BufReader, Read, Seek},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

pub(super) fn codex_home() -> io::Result<PathBuf> {
    let home = || {
        env::home_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing Codex home"))
    };
    let Some(path) = env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(home()?.join(".codex"));
    };
    match path.strip_prefix("~") {
        Ok(rest) => Ok(home()?.join(rest)),
        Err(_) => Ok(path),
    }
}

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
    let root = codex_home()?.join("sessions");
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
pub(super) const PREFIX: &str = "[router] Messages for the controller: {\"messages\": [";
pub(super) const SUFFIX: &str = "]}";

pub(super) fn envelope(m: &Value) -> String {
    let header = [
        "id",
        "messageId",
        "dispatch",
        "type",
        "state",
        "statusVersion",
    ];
    let mut fields = Vec::new();
    let mut field = |k: &str, v: &Value| fields.push(format!("{}: {}", json!(k), spaced_json(v)));
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
    format!("{{{}}}", fields.join(", "))
}

// Exit codes used only between wfc relay and its queue supervisor. An unexpected
// status after launch (including a signal or an internal error) stays uncertain.
#[derive(PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Outcome {
    Delivered = 0,
    Failed = 20,
    Missing = 21,
    Uncertain = 22,
    TooLong = 23,
}

pub(super) fn missing_rollout(output: &mut fs::File, thread: &OsStr) -> io::Result<bool> {
    output.rewind()?;
    let length = output.metadata()?.len();
    let mut lines = BufReader::new(output.take(length));
    let mut prefix = Vec::new();
    let mut refused = false;
    loop {
        prefix.clear();
        let n = lines.by_ref().take(512).read_until(b'\n', &mut prefix)?;
        if n == 0 {
            return Ok(refused);
        }
        if prefix.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if n == 512 && prefix.last() != Some(&b'\n') {
            return Ok(false);
        }
        let line = String::from_utf8_lossy(&prefix);
        let mut diagnostic = line.trim();
        while let Some(rest) = ["Error: ", "error: ", "codex queue: "]
            .iter()
            .find_map(|prefix| diagnostic.strip_prefix(prefix))
        {
            diagnostic = rest;
        }
        const MISSING: &str = "no rollout found for thread";
        if diagnostic == MISSING {
            refused = true;
            continue;
        }
        if let Some(id) = diagnostic
            .strip_prefix(MISSING)
            .and_then(|rest| rest.strip_prefix(' '))
        {
            let id = id.strip_prefix("id ").unwrap_or(id);
            if id.eq_ignore_ascii_case(&thread.to_string_lossy()) {
                refused = true;
                continue;
            }
        }
        // A historical refusal can occur before or after evidence of submission.
        // Only a complete transcript consisting of explicit refusals is conclusive.
        return Ok(false);
    }
}

pub(super) fn queue(
    thread: &str,
    messages: &[Value],
    text: &str,
    limit: Duration,
) -> Result<Outcome> {
    let ids: Vec<_> = messages.iter().map(|m| m["id"].as_u64().unwrap()).collect();
    let mut supervisor = Command::new(env::current_exe()?);
    supervisor
        .arg("--relay-queue")
        .arg(std::process::id().to_string())
        .arg(limit.as_secs_f64().to_string())
        .arg(thread)
        .arg(text)
        .stdin(Stdio::null());
    crate::workflow::worker_owner::new_session(&mut supervisor);
    let mut child = match supervisor.spawn() {
        Ok(child) => child,
        Err(e) if e.raw_os_error() == Some(libc::E2BIG) => return Ok(Outcome::TooLong),
        Err(e) => {
            println!("router relay: queue supervisor could not start for messages {ids:?}: {e}");
            return Ok(Outcome::Failed);
        }
    };
    let outcome = match child.wait()?.code() {
        Some(0) => Outcome::Delivered,
        Some(code) if code == Outcome::Failed as i32 => Outcome::Failed,
        Some(code) if code == Outcome::TooLong as i32 => Outcome::TooLong,
        Some(code) if code == Outcome::Missing as i32 => Outcome::Missing,
        _ => {
            println!("router relay: delivery of messages {ids:?} is uncertain");
            Outcome::Uncertain
        }
    };
    Ok(outcome)
}
