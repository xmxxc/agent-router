//! Keep slow backend I/O out of the controller's receipt polling loop.
use super::{binding, candidate, classify, Sample, Watch};
use crate::workflow::{backend, state_dir, worker_owner::exit_pending};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek},
    os::{fd::FromRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    task::Poll,
    time::{Duration, Instant},
};

const MAX_PROBE: Duration = Duration::from_secs(1);

#[derive(Serialize, Deserialize)]
pub(super) struct Observation {
    pub binding: String,
    pub sample: Sample,
}

pub(super) struct Probe {
    pub id: String,
    pub previous: Option<Watch>,
    output: File,
    child: Option<Child>,
    deadline: Instant,
}

impl Probe {
    pub fn start(
        state: &Path,
        row: &serde_json::Value,
        previous: Option<Watch>,
        salt: &str,
        remaining: Duration,
    ) -> Result<Self> {
        let deadline = Instant::now() + remaining.min(MAX_PROBE);
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
        let child = Command::new(env::current_exe()?)
            .args(["--observe-host-runtime", id, &binding(row), salt])
            .env("WFC_STATE", state)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        Ok(Self {
            id: id.to_owned(),
            previous,
            output,
            child: Some(child),
            deadline,
        })
    }

    // An empty finished result advances to the next dispatch.
    pub fn poll(&mut self) -> Result<Poll<Option<Observation>>> {
        if Instant::now() >= self.deadline {
            self.finish()?;
            return Ok(Poll::Ready(None));
        }
        let pid = self.child.as_ref().expect("running probe").id() as i32;
        // Observe termination without reaping the leader. Group cleanup can
        // still address its pinned PID if a client left descendants behind.
        if !exit_pending(pid)? {
            return Ok(Poll::Pending);
        }
        if !self.finish()?.is_some_and(|status| status.success()) {
            return Ok(Poll::Ready(None));
        }
        self.output.rewind()?;
        let result = serde_json::from_reader((&self.output).take(4096)).ok();
        Ok(Poll::Ready(result))
    }

    fn finish(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .take()
            .map(|mut child| {
                // SAFETY: the unreaped helper leads a fresh group containing only
                // the observation clients it spawned, never the host service.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                child.wait()
            })
            .transpose()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.finish();
    }
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
        },
    )?;
    Ok(())
}
