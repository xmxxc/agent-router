//! A separate wfc survives loss of the relay long enough to stop this queue
//! invocation's process group. A persistent host owns a separate group.
use super::super::private_file;
use super::codex::{codex_home, missing_rollout, Outcome};
use anyhow::{ensure, Result};
use std::{
    env,
    ffi::OsStr,
    fs, io,
    process::{Child, Command, ExitCode, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

fn launch(thread: &OsStr, message: &OsStr) -> io::Result<(Child, fs::File)> {
    let path = env::temp_dir().join(format!("wfc-queue-{}", Uuid::new_v4()));
    let output = private_file().create_new(true).open(&path)?;
    fs::remove_file(path)?;
    let mut cmd = Command::new("codex");
    cmd.arg("queue")
        .arg("--thread")
        .arg(thread)
        .arg("--message")
        .arg(message)
        .arg("--resume")
        .env("CODEX_HOME", codex_home()?)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output.try_clone()?);
    // A session leader cannot move itself out of the owned process group.
    crate::workflow::worker_owner::new_session(&mut cmd);
    for (name, _) in env::vars_os() {
        if crate::tmux::launch::router_credential(&name.to_string_lossy()) {
            cmd.env_remove(name);
        }
    }
    Ok((cmd.spawn()?, output))
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
    // SAFETY: this private, single-threaded supervisor owns only one invocation.
    ensure!(
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } != libc::SIG_ERR
            && unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "configure queue supervisor: {}",
        io::Error::last_os_error()
    );
    let (mut child, mut output) = match launch(thread, message) {
        Ok(started) => started,
        Err(e) => {
            println!("router relay: codex queue could not start: {e}");
            return Ok(ExitCode::from(Outcome::Failed as u8));
        }
    };
    let finished = loop {
        match completed(child.id() as _) {
            Ok(false) => (),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            other => break other,
        }
        let remaining = limit.saturating_sub(start.elapsed());
        if remaining.is_zero() || !relay_alive() {
            break Ok(false);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    };
    let status = stop_group(&mut child)?;
    let outcome = if !finished? {
        Outcome::Uncertain
    } else if status.success() {
        Outcome::Delivered
    } else if status.code().is_some() && missing_rollout(&mut output, thread)? {
        Outcome::Missing
    } else {
        Outcome::Uncertain
    };
    Ok(ExitCode::from(outcome as u8))
}
