//! Keep slow backend I/O out of the controller's receipt polling loop.
use super::{binding, candidate, classify, execution_binding, Sample, Watch};
use crate::workflow::messages::relay::queue_owner::{become_subreaper, guard_group, stop_group};
use crate::workflow::{
    backend, state_dir,
    worker_owner::{exit_pending, new_session},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek},
    os::fd::FromRawFd,
    path::Path,
    process::{Child, Command, ExitCode, Stdio},
    task::Poll,
    time::Duration,
};

const MAX_PROBE: Duration = Duration::from_secs(1);
const READ_TIMEOUT: u8 = 3; // private supervisor outcome
const CANCELLED: u8 = 4;

#[derive(Serialize, Deserialize)]
pub(super) struct Observation {
    pub binding: String,
    pub sample: Sample,
    #[serde(default)]
    pub timed_out: bool,
}

pub(super) struct Probe {
    pub id: String,
    pub previous: Option<Watch>,
    output: File,
    child: Option<Child>,
    execution: String,
    full_budget: bool,
}

impl Probe {
    pub fn start(
        state: &Path,
        row: &serde_json::Value,
        previous: Option<Watch>,
        salt: &str,
        remaining: Duration,
    ) -> Result<Self> {
        let deadline = monotonic_ns()? + remaining.min(MAX_PROBE).as_nanos() as u64;
        // The helper writes only classified evidence and hashes. An anonymous
        // file also prevents a wedged writer or a descendant from holding up EOF.
        // SAFETY: memfd_create transfers ownership of a fresh descriptor on success.
        let fd =
            unsafe { libc::memfd_create(c"wfc-runtime-observation".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let output = unsafe { File::from_raw_fd(fd) };
        let id = row["dispatch"].as_str().context("missing dispatch")?;
        let mut supervisor = Command::new(env::current_exe()?);
        supervisor
            .args([
                "--observe-host-owner",
                &std::process::id().to_string(),
                &deadline.to_string(),
                id,
                &binding(row),
                salt,
            ])
            .env("WFC_STATE", state)
            .stdin(Stdio::piped())
            .stdout(output.try_clone()?)
            .stderr(Stdio::null());
        new_session(&mut supervisor);
        Ok(Self {
            id: id.to_owned(),
            previous,
            output,
            child: Some(supervisor.spawn()?),
            execution: execution_binding(row),
            full_budget: remaining > MAX_PROBE,
        })
    }

    // The isolated supervisor has reaped its own group before it exits.
    pub fn poll(&mut self) -> Result<Poll<Option<Observation>>> {
        let Some(status) = self.child.as_mut().expect("running probe").try_wait()? else {
            return Ok(Poll::Pending);
        };
        self.child = None;
        if status.code() == Some(i32::from(READ_TIMEOUT)) && self.full_budget {
            return Ok(Poll::Ready(Some(Observation {
                binding: self.execution.clone(),
                sample: Sample::Unavailable,
                timed_out: true,
            })));
        }
        if !status.success() {
            return Ok(Poll::Ready(None));
        }
        self.output.rewind()?;
        let result = serde_json::from_reader((&self.output).take(4096)).ok();
        Ok(Poll::Ready(result))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(mut supervisor) = self.child.take() {
            // The pipe is unique to this invocation. EOF requests cancellation
            // without racing a signal against a reused PID.
            drop(supervisor.stdin.take());
            let _ = supervisor.wait();
        }
    }
}

fn monotonic_ns() -> io::Result<u64> {
    let mut stamp = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: a successful clock_gettime initializes the provided timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, stamp.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let stamp = unsafe { stamp.assume_init() };
    Ok(stamp.tv_sec as u64 * 1_000_000_000 + stamp.tv_nsec as u64)
}

fn cancelled(parent: libc::pid_t, timeout_ms: i32) -> io::Result<bool> {
    if unsafe { libc::getppid() } != parent {
        return Ok(true);
    }
    // The parent never sends data. Readability, HUP or an invalid descriptor
    // all revoke this invocation's lease before an attempted result.
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    match unsafe { libc::poll(&mut fd, 1, timeout_ms) } {
        n if n > 0 => Ok(true),
        0 => Ok(unsafe { libc::getppid() } != parent),
        _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

pub(crate) fn run_runtime_owner(args: &[OsString]) -> Result<ExitCode> {
    let parent: libc::pid_t = args[0].to_str().context("invalid caller PID")?.parse()?;
    let deadline: u64 = args[1]
        .to_str()
        .context("invalid monotonic deadline")?
        .parse()?;
    ensure!(parent > 0, "invalid caller PID");
    if cancelled(parent, 0)? {
        return Ok(ExitCode::from(CANCELLED));
    }
    let budget = deadline.saturating_sub(monotonic_ns()?);
    ensure!(
        budget <= MAX_PROBE.as_nanos() as u64,
        "invalid probe budget"
    );
    if budget == 0 {
        return Ok(ExitCode::from(CANCELLED)); // no backend read was started
    }
    become_subreaper().context("configure observation supervisor")?;
    let mut cmd = Command::new(env::current_exe()?);
    cmd.arg("--observe-host-runtime")
        .args(&args[2..])
        .stdin(Stdio::null());
    new_session(&mut cmd);
    // The in-group guard pins the target even if the supervisor itself dies.
    let _lifetime = guard_group(&mut cmd)?;
    if cancelled(parent, 0)? || monotonic_ns()? >= deadline {
        return Ok(ExitCode::from(CANCELLED));
    }
    let mut child = cmd.spawn()?;
    let outcome = (|| -> io::Result<u8> {
        loop {
            if cancelled(parent, 0)? {
                return Ok(CANCELLED);
            }
            if exit_pending(child.id() as i32)? {
                return Ok(0);
            }
            let remaining = deadline.saturating_sub(monotonic_ns()?);
            if remaining == 0 {
                return Ok(READ_TIMEOUT);
            }
            let sleep_ms = remaining.div_ceil(1_000_000).min(10) as i32;
            if cancelled(parent, sleep_ms)? {
                return Ok(CANCELLED);
            }
        }
    })();
    let status = stop_group(&mut child)?;
    let outcome = outcome?;
    Ok(ExitCode::from(if outcome == 0 && !status.success() {
        CANCELLED
    } else {
        outcome
    }))
}

pub(crate) fn run_runtime_probe(args: &[OsString]) -> Result<()> {
    let id = args[0].to_str().context("invalid dispatch")?;
    let expected = args[1].to_str().context("invalid binding digest")?;
    let salt = args[2].to_str().context("invalid observation salt")?;
    ensure!(
        crate::workflow::valid_id(id)
            && expected.len() == 64
            && expected.bytes().all(|b| b.is_ascii_hexdigit())
            && salt.len() == 32
            && salt.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid runtime probe identity"
    );
    let state = state_dir()?;
    let Some(mut row) = candidate(&state.join("dispatches"), id, None) else {
        return Ok(());
    };
    if binding(&row) != expected {
        return Ok(());
    }
    let sample = if backend::runtime_exit(&state, &row)?.is_some() {
        Sample::Unavailable // the publisher takes the same bound local terminal evidence
    } else if let Some(screen) = backend::runtime_screen(&state, &mut row)? {
        let start = screen.ceil_char_boundary(screen.len().saturating_sub(4096));
        let tail = screen[start..].trim();
        Sample::Screen {
            fingerprint: super::body_hash(&format!("{salt}{tail}")),
            fault: classify(row["host"].as_str().unwrap_or_default(), tail),
            nonblank: !tail.is_empty(),
            salt: salt.to_owned(),
        }
    } else {
        Sample::Unavailable
    };
    serde_json::to_writer(
        io::stdout().lock(),
        &Observation {
            binding: binding(&row),
            sample,
            timed_out: false,
        },
    )?;
    Ok(())
}
