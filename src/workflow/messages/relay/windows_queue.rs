//! The relay owns a Job for each queue call. A staging child starts Codex only
//! after it has joined that Job, and the Job dies with the relay.
use super::{codex, codex_backoff::Backoff, Outcome};
use crate::{
    windows_process::{self as process, Job, TICK},
    workflow::{controller, ensure_state_writable, state_dir},
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, Read, Seek, Write},
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;
use winapi::{
    shared::winerror::{ERROR_FILENAME_EXCED_RANGE, ERROR_FILE_NOT_FOUND},
    um::winbase::FILE_FLAG_DELETE_ON_CLOSE,
};

const START_ERROR: &str = "wfc queue start os error ";
const WAIT_ERROR: &str = "wfc queue wait os error ";

fn log(diagnostic: &str) {
    let _ = writeln!(io::stderr().lock(), "router relay: {diagnostic}");
}

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
    let state = match state_dir() {
        Ok(state) => state,
        Err(e) => {
            log(&process::diagnostic(
                "resolve Windows queue state",
                e.downcast_ref(),
            ));
            return Ok(Outcome::Failed);
        }
    };
    queue_in_state(&state, thread, text, limit)
}

fn queue_in_state(state: &Path, thread: &str, text: &str, limit: Duration) -> Result<Outcome> {
    let started = (|| -> Result<_> {
        let mut request = serde_json::to_vec(&json!({"thread":thread,"message":text}))
            .context("encode Windows queue request")?;
        request.push(b'\n');
        let (stdout, stderr, job, mut child) = prepare(state)?;
        let input = child.stdin.take().context("open Windows queue input")?;
        Ok((request, stdout, stderr, job, child, input))
    })();
    let (request, mut stdout, mut stderr, job, mut child, mut input) = match started {
        Ok(started) => started,
        Err(e) => {
            let operation = e
                .downcast_ref::<&'static str>()
                .copied()
                .unwrap_or("prepare Windows queue supervisor");
            log(&process::diagnostic(operation, e.downcast_ref()));
            return Ok(Outcome::Failed);
        }
    };

    let start = Instant::now();
    let writer = match std::thread::Builder::new().spawn(move || input.write_all(&request)) {
        Ok(writer) => writer,
        Err(e) => {
            log(&process::diagnostic(
                "start Windows queue request writer",
                Some(&e),
            ));
            return Ok(Outcome::Failed);
        }
    };
    let exit = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break exit.code(),
            Ok(None) => (),
            Err(e) => {
                log(&process::diagnostic(
                    "wait for Windows queue result",
                    Some(&e),
                ));
                break None;
            }
        }
        let remaining = limit.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break None;
        }
        std::thread::sleep(remaining.min(TICK));
    };
    let elapsed = start.elapsed();
    let cleanup = job.stop();
    // Even on a cleanup error, dropping the Job closes the reading side.
    drop(job);
    if let Err(e) = &cleanup {
        log(&process::diagnostic(
            "confirm Windows queue Job cleanup",
            Some(e),
        ));
    }
    let written = writer.join().is_ok_and(|result| result.is_ok());
    let outcome = match exit {
        Some(0) => Outcome::Delivered,
        Some(20) => {
            let diagnostic = transcript_diagnostic(&mut stdout, &mut stderr, true);
            log(&diagnostic);
            Outcome::Failed
        }
        Some(23) => Outcome::TooLong,
        Some(1 | 2) if cleanup.is_ok() => match codex::refusal(
            &mut stdout,
            &mut stderr,
            thread.as_ref(),
            exit == Some(1) && elapsed < limit,
        ) {
            Ok(Some(refused)) => refused,
            Ok(None) => {
                log(&transcript_diagnostic(&mut stdout, &mut stderr, false));
                Outcome::Uncertain
            }
            Err(e) => {
                log(&process::diagnostic(
                    "read Windows queue evidence",
                    Some(&e),
                ));
                Outcome::Uncertain
            }
        },
        Some(code) if code == Outcome::Uncertain as i32 => {
            log(&transcript_diagnostic(&mut stdout, &mut stderr, false));
            Outcome::Uncertain
        }
        _ => {
            log(if written {
                "Windows queue result is unconfirmed"
            } else {
                "Windows queue request write and result are unconfirmed"
            });
            Outcome::Uncertain
        }
    };
    let backoff = Backoff::new(state, thread)?;
    if matches!(outcome, Outcome::Delivered | Outcome::TargetFull) {
        // The relay still owns its scope and target locks.
        ensure_state_writable(state)?;
    }
    if outcome == Outcome::Delivered {
        if let Err(e) = backoff.clear() {
            log(&process::diagnostic("clear Codex queue cooldown", Some(&e)));
        }
    }
    if outcome == Outcome::TargetFull {
        let delay = backoff.record_full()?;
        eprintln!("router relay: Codex queue is full for thread {thread}; pausing new submissions for {delay}s");
    }
    Ok(outcome)
}

fn prepare(state: &Path) -> Result<(File, File, Job, Child)> {
    let stdout = capture(state).context("open Windows queue stdout capture")?;
    let stderr = capture(state).context("open Windows queue stderr capture")?;
    let job = Job::new().context("create Windows queue Job")?;
    let mut command = Command::new(env::current_exe().context("resolve Windows queue supervisor")?);
    controller::strip_credentials(&mut command);
    command
        .arg("--windows-relay-queue")
        .creation_flags(process::NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(
            stdout
                .try_clone()
                .context("copy Windows queue stdout handle")?,
        )
        .stderr(
            stderr
                .try_clone()
                .context("copy Windows queue stderr handle")?,
        );
    let mut child = command.spawn().context("start Windows queue supervisor")?;
    if let Err(e) = job.assign(&child) {
        // Without a request frame, even a staging process whose exit could
        // not be verified cannot start Codex.
        if process::stop_staged(&mut child).is_err() {
            log("Windows queue staging process exit is unconfirmed");
        }
        return Err(e).context("assign Windows queue Job");
    }
    Ok((stdout, stderr, job, child))
}

fn transcript_diagnostic(stdout: &mut File, stderr: &mut File, before_launch: bool) -> String {
    let operation = if before_launch {
        "Codex queue could not start"
    } else {
        "Codex queue result is unconfirmed"
    };
    for output in [stderr, stdout] {
        let mut bytes = Vec::new();
        // Limit diagnostic reads and distinguish a truncated UTF-8 sequence at
        // this limit from an actual malformed stream.
        const LIMIT: u64 = 4096;
        let result = output
            .rewind()
            .and_then(|()| output.take(LIMIT).read_to_end(&mut bytes));
        let len = match result {
            Ok(len) => len,
            Err(e) => return process::diagnostic("read Windows queue diagnostic", Some(&e)),
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(e) if e.error_len().is_none() && len == LIMIT as usize => {
                std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap()
            }
            Err(_) => return format!("{operation}: output is not UTF-8"),
        };
        // External output can contain the whole receipt. Retain only a
        // numeric OS error from a recognized diagnostic, never free text.
        for line in text.lines() {
            let code = if before_launch {
                line.strip_prefix(START_ERROR)
            } else if let Some(code) = line.strip_prefix(WAIT_ERROR) {
                Some(code)
            } else if line.starts_with("Error: failed to start embedded app server: ") {
                line.strip_suffix(')')
                    .and_then(|s| s.rsplit_once(" (os error "))
                    .map(|(_, code)| code)
            } else {
                None
            };
            if let Some(code) = code
                .and_then(|code| code.parse::<i32>().ok())
                .filter(|&code| code > 0)
            {
                return process::diagnostic(operation, Some(&io::Error::from_raw_os_error(code)));
            }
        }
    }
    operation.to_owned()
}

fn codex_program() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = std::path::absolute(dir.join("codex.exe"))?;
            match fs::metadata(&candidate) {
                Ok(metadata) if metadata.is_file() => return Ok(candidate),
                Ok(_) => (),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    ()
                }
                Err(e) => return Err(e),
            }
        }
    }
    Err(io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32))
}

fn codex_command() -> Result<Command> {
    let mut request = Vec::new();
    io::stdin()
        .lock()
        .take(256 * 1024)
        .read_until(b'\n', &mut request)?;
    ensure!(request.last() == Some(&b'\n'), "incomplete queue request");
    let request: Value = serde_json::from_slice(&request)?;
    let thread = request["thread"].as_str().context("missing queue thread")?;
    let message = request["message"]
        .as_str()
        .context("missing queue message")?;
    let mut command = Command::new(codex_program()?);
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
    Ok(command)
}

fn report_system_error(marker: &str, error: Option<&io::Error>) {
    if let Some(code) = error.and_then(io::Error::raw_os_error) {
        let _ = writeln!(io::stderr().lock(), "{marker}{code}");
    }
}

fn start_failed(error: Option<&io::Error>) -> ExitCode {
    report_system_error(START_ERROR, error);
    ExitCode::from(Outcome::Failed as u8)
}

pub(crate) fn run() -> Result<ExitCode> {
    let mut command = match codex_command() {
        Ok(command) => command,
        Err(e) => return Ok(start_failed(e.downcast_ref())),
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.raw_os_error() == Some(ERROR_FILENAME_EXCED_RANGE as i32) => {
            return Ok(ExitCode::from(Outcome::TooLong as u8))
        }
        Err(e) => return Ok(start_failed(Some(&e))),
    };
    let code = match child.wait() {
        Ok(status) => match status.code() {
            Some(0) => 0,
            Some(1) => 1,
            _ => 2,
        },
        Err(e) => {
            report_system_error(WAIT_ERROR, Some(&e));
            Outcome::Uncertain as u8
        }
    };
    Ok(ExitCode::from(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_failure_can_settle_as_unsent() -> Result<()> {
        let missing_state = env::temp_dir().join(Uuid::new_v4().simple().to_string());
        let outcome = queue_in_state(
            &missing_state,
            "f5ab5adc-3fa2-4546-a594-cd842ec1291a",
            "fixture receipt",
            Duration::from_secs(1),
        )?;
        assert!(outcome == Outcome::Failed);
        Ok(())
    }
}
