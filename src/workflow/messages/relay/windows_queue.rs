//! The relay owns a Job for each queue call. A staging child starts Codex only
//! after it has joined that Job, and the Job dies with the relay.
use super::{codex, codex_backoff::Backoff, Outcome};
use crate::{
    windows_process::{self as process, Job, TICK},
    workflow::{controller, ensure_state_writable, state_dir},
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, BufRead, Read, Write},
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::{Command, ExitCode, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;
use winapi::{
    shared::winerror::ERROR_FILENAME_EXCED_RANGE, um::winbase::FILE_FLAG_DELETE_ON_CLOSE,
};

fn capture(state: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(state.join(format!("wfc-queue-{}", Uuid::new_v4())))
}

pub(super) fn queue(
    thread: &str,
    _messages: &[Value],
    text: &str,
    limit: Duration,
    _scope_lock: Option<&File>,
    _target_lock: &File,
) -> Result<Outcome> {
    if limit.is_zero() {
        return Ok(Outcome::Failed);
    }
    let state = state_dir()?;
    let (mut stdout, mut stderr) = (capture(&state)?, capture(&state)?);
    let job = Job::new()?;
    let mut command = Command::new(env::current_exe()?);
    controller::strip_credentials(&mut command);
    command
        .arg("--windows-relay-queue")
        .creation_flags(process::NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(Outcome::Failed),
    };
    if job.assign(&child).is_err() {
        process::stop_staged(&mut child)?;
        return Ok(Outcome::Failed);
    }
    let mut input = child
        .stdin
        .take()
        .context("missing queue supervisor input")?;
    let mut request = serde_json::to_vec(&json!({"thread":thread,"message":text}))?;
    request.push(b'\n');
    let start = Instant::now();
    let writer = std::thread::spawn(move || input.write_all(&request));
    let (exit, elapsed) = loop {
        if let Some(exit) = child.try_wait()? {
            break (exit.code(), start.elapsed());
        }
        let remaining = limit.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break (None, start.elapsed());
        }
        std::thread::sleep(remaining.min(TICK));
    };
    job.stop()?;
    // Job teardown closes the reading side even if the staging child
    // stalled before reading a frame larger than the pipe buffer.
    let written = writer.join().is_ok_and(|result| result.is_ok());
    if !written {
        return Ok(Outcome::Uncertain);
    }
    let outcome = match exit {
        Some(0) => Outcome::Delivered,
        Some(20) => Outcome::Failed,
        Some(23) => Outcome::TooLong,
        Some(1 | 2) => codex::refusal(
            &mut stdout,
            &mut stderr,
            thread.as_ref(),
            exit == Some(1) && elapsed < limit,
        )?
        .unwrap_or(Outcome::Uncertain),
        _ => Outcome::Uncertain,
    };
    let backoff = Backoff::new(&state, thread)?;
    if matches!(outcome, Outcome::Delivered | Outcome::TargetFull) {
        // The relay still owns its scope and target locks.
        ensure_state_writable(&state)?;
    }
    if outcome == Outcome::Delivered {
        if let Err(e) = backoff.clear() {
            eprintln!("router relay: could not clear Codex queue cooldown: {e}");
        }
    }
    if outcome == Outcome::TargetFull {
        let delay = backoff.record_full()?;
        eprintln!("router relay: Codex queue is full for thread {thread}; pausing new submissions for {delay}s");
    }
    Ok(outcome)
}

pub(crate) fn run() -> Result<ExitCode> {
    let mut request = Vec::new();
    io::stdin()
        .lock()
        .take(256 * 1024)
        .read_until(b'\n', &mut request)?;
    let request: Value = serde_json::from_slice(&request)?;
    let thread = request["thread"].as_str().context("missing queue thread")?;
    let message = request["message"]
        .as_str()
        .context("missing queue message")?;
    let mut command = Command::new("codex.exe");
    controller::strip_credentials(&mut command);
    command
        .args([
            "queue",
            "--thread",
            thread,
            "--message",
            message,
            "--resume",
        ])
        .env("CODEX_HOME", codex::codex_home()?)
        .creation_flags(process::NO_WINDOW)
        .stdin(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.raw_os_error() == Some(ERROR_FILENAME_EXCED_RANGE as i32) => {
            return Ok(ExitCode::from(Outcome::TooLong as u8))
        }
        Err(_) => return Ok(ExitCode::from(Outcome::Failed as u8)),
    };
    let code = match child.wait()?.code() {
        Some(0) => 0,
        Some(1) => 1,
        _ => 2,
    };
    Ok(ExitCode::from(code))
}
