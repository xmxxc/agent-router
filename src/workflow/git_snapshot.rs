//! Sample the dispatched worktree without refreshing its index. Either both
//! values describe a successful Git observation or both remain unknown.
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub head: Option<String>,
    pub dirty: Option<bool>,
}

impl Snapshot {
    pub fn read(cwd: &Path) -> Self {
        fn observe(cwd: &Path) -> Option<Snapshot> {
            let oid = git(cwd, &["rev-parse", "--verify", "HEAD"]).ok()?;
            let head = std::str::from_utf8(&oid).ok()?.trim();
            if !matches!(head.len(), 40 | 64) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let status = git(cwd, &["status", "--porcelain", "--untracked-files=normal"]).ok()?;
            Some(Snapshot {
                head: Some(head.into()),
                dirty: Some(!status.is_empty()),
            })
        }
        observe(cwd).unwrap_or_default()
    }
}

// Only a short prefix is needed. An unlinked file lets Git finish arbitrarily
// large byte-oriented status output without filling a pipe or allocating it all.
fn git(cwd: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("wfc-git-{}", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut output = options.open(&path)?;
    fs::remove_file(path)?;
    let mut cmd = Command::new("git");
    super::controller::strip_credentials(&mut cmd);
    // Local repository variables (git rev-parse --local-env-vars) can point at
    // the caller's index, refs, or objects even after changing directory.
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        cmd.env_remove(name);
    }
    cmd.current_dir(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(output.try_clone()?);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err(io::Error::other("git failed")),
            Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                #[cfg(unix)]
                // SAFETY: this unreaped child leads its own process group.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(result
                    .err()
                    .unwrap_or_else(|| io::ErrorKind::TimedOut.into()));
            }
        }
    }
    output.rewind()?;
    let mut prefix = Vec::new();
    output.take(128).read_to_end(&mut prefix)?;
    Ok(prefix)
}
