//! A separate wfc survives loss of the relay long enough to stop this queue
//! invocation's process group. A persistent host owns a separate group.
use super::super::private_file;
use super::codex::{codex_home, refusal};
use super::codex_backoff::Backoff;
use super::Outcome;
use crate::workflow::{ensure_state_writable, state_dir};
use anyhow::{ensure, Context, Result};
use std::{
    env,
    ffi::OsStr,
    fs, io,
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command, ExitCode, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

// Install a sibling in the queue's new process group before exec. Its own group
// membership pins the target even if the supervisor and queue leader both die.
// The supervisor is a subreaper, so codex does not inherit a child to manage.
fn guard_group(cmd: &mut Command) -> io::Result<io::PipeWriter> {
    let (reader, writer) = io::pipe()?;
    let fd_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if fd_limit < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: after fork use only raw async-signal-safe operations. The child
    // of the intermediate exits via _exit and never returns to Rust's runtime.
    unsafe {
        cmd.pre_exec(move || {
            let intermediate = libc::fork();
            if intermediate == -1 {
                return Err(io::Error::last_os_error());
            }
            if intermediate == 0 {
                match libc::fork() {
                    -1 => libc::_exit(1),
                    0 => {
                        if libc::dup2(reader.as_raw_fd(), 0) >= 0 {
                            // Includes the pipe writer and Command's exec-error fd.
                            if libc::syscall(libc::SYS_close_range, 1u32, u32::MAX, 0u32) == -1 {
                                for fd in 1..fd_limit {
                                    libc::close(fd as i32);
                                }
                            }
                            let mut byte = 0u8;
                            while libc::read(0, (&raw mut byte).cast(), 1) < 0
                                && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                            {
                            }
                        }
                        libc::kill(0, libc::SIGKILL);
                        libc::_exit(1);
                    }
                    _ => libc::_exit(0),
                }
            }
            let mut status = 0;
            while libc::waitpid(intermediate, &mut status, 0) < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            if status != 0 {
                return Err(io::Error::from_raw_os_error(libc::EAGAIN));
            }
            Ok(())
        });
    }
    Ok(writer)
}

fn capture_output() -> io::Result<fs::File> {
    let path = env::temp_dir().join(format!("wfc-queue-{}", Uuid::new_v4()));
    let output = private_file().create_new(true).open(&path)?;
    fs::remove_file(path)?;
    Ok(output)
}

fn launch(
    thread: &OsStr,
    message: &OsStr,
) -> io::Result<(Child, fs::File, fs::File, io::PipeWriter)> {
    let stdout = capture_output()?;
    let stderr = capture_output()?;
    let mut cmd = Command::new("codex");
    cmd.arg("queue")
        .arg("--thread")
        .arg(thread)
        .arg("--message")
        .arg(message)
        .arg("--resume")
        .env("CODEX_HOME", codex_home()?)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    // A session leader cannot move itself out of the owned process group.
    crate::workflow::worker_owner::new_session(&mut cmd);
    crate::workflow::controller::strip_credentials(&mut cmd);
    let lifetime = guard_group(&mut cmd)?;
    Ok((cmd.spawn()?, stdout, stderr, lifetime))
}

fn completed(pid: libc::pid_t) -> io::Result<bool> {
    // WNOWAIT pins the leader's PID even when the command finishes before its
    // helpers. It is still safe to signal this PGID at cleanup.
    unsafe {
        let mut info = std::mem::zeroed::<libc::siginfo_t>();
        if libc::waitid(
            libc::P_PID,
            pid as _,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

fn stop_group(child: &mut Child) -> io::Result<ExitStatus> {
    let group = -(child.id() as libc::pid_t);
    // SAFETY: the unreaped queue leader pins this process group. The supervisor
    // runs in a separate session from the relay, and outside the queue group.
    if unsafe { libc::kill(group, libc::SIGKILL) } == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }
    let status = child.wait()?;
    // Adopted children in other groups include deliberately detached hosts.
    // Reap only temporary helpers; no further signals are sent after wait().
    loop {
        if unsafe { libc::waitpid(group, std::ptr::null_mut(), 0) } == -1 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ECHILD) => return Ok(status),
                Some(libc::EINTR) => continue,
                _ => return Err(err),
            }
        }
    }
}

pub fn run(parent: &OsStr, limit: &OsStr, thread: &OsStr, message: &OsStr) -> Result<ExitCode> {
    let parent: libc::pid_t = parent.to_string_lossy().parse()?;
    let limit = super::seconds(&limit.to_string_lossy())?;
    let start = Instant::now();
    let relay_alive = || unsafe { libc::getppid() == parent };
    ensure!(relay_alive(), "relay exited before queue launch");
    let state = state_dir()?;
    let backoff = Backoff::new(
        &state,
        thread.to_str().context("invalid Codex queue thread")?,
    )?;
    // SAFETY: this private, single-threaded supervisor owns only one invocation.
    ensure!(
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } != libc::SIG_ERR
            && unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "configure queue supervisor: {}",
        io::Error::last_os_error()
    );
    let (mut child, mut stdout, mut stderr, _lifetime) = match launch(thread, message) {
        Ok(started) => started,
        Err(e) if e.raw_os_error() == Some(libc::E2BIG) => {
            return Ok(ExitCode::from(Outcome::TooLong as u8))
        }
        Err(e) => {
            eprintln!("router relay: codex queue could not start: {e}");
            return Ok(ExitCode::from(Outcome::Failed as u8));
        }
    };
    let observed = loop {
        match completed(child.id() as _) {
            Ok(true) => break Ok(Some(start.elapsed())),
            Ok(false) => (),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            Err(e) => break Err(e),
        }
        let remaining = limit.saturating_sub(start.elapsed());
        if remaining.is_zero() || !relay_alive() {
            break Ok(None);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    };
    let status = stop_group(&mut child)?;
    let outcome = match (observed?, status.code()) {
        (Some(_), Some(0)) => Outcome::Delivered,
        (Some(elapsed), Some(code)) => {
            // Completed legacy outcomes keep their original ordering. Capacity
            // requires observing the exit within the queue deadline.
            refusal(
                &mut stdout,
                &mut stderr,
                thread,
                code == 1 && elapsed < limit,
            )?
            .unwrap_or(Outcome::Uncertain)
        }
        _ => Outcome::Uncertain,
    };
    if matches!(outcome, Outcome::Delivered | Outcome::TargetFull) {
        // The inherited target lock still covers the cooldown update.
        ensure_state_writable(&state)?;
    }
    if outcome == Outcome::Delivered {
        if let Err(e) = backoff.clear() {
            eprintln!("router relay: could not clear Codex queue cooldown: {e}");
        }
    }
    if outcome == Outcome::TargetFull {
        let seconds = backoff
            .record_full()
            .context("record Codex queue cooldown")?;
        eprintln!(
            "router relay: Codex queue is full for thread {}; pausing new submissions for {seconds}s",
            thread.to_string_lossy()
        );
    }
    Ok(ExitCode::from(outcome as u8))
}
