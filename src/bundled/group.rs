//! All destructive writes use the descriptor of a freshly created cgroup.
use super::*;
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

fn directory(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?)
}

pub(super) fn root() -> Result<PathBuf> {
    let path = fs::canonicalize(
        std::env::var_os("WFC_BUNDLED_CGROUP_ROOT").context("set WFC_BUNDLED_CGROUP_ROOT")?,
    )?;
    check_cgroup(&directory(&path)?)?;
    ensure!(
        path.join("cgroup.kill").is_file(),
        "delegated cgroup with cgroup.kill required"
    );
    Ok(path)
}

fn check_cgroup(fd: &File) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::uninit();
    if unsafe { libc::fstatfs(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(
        unsafe { stat.assume_init() }.f_type == libc::CGROUP2_SUPER_MAGIC,
        "expected cgroup v2 filesystem"
    );
    Ok(())
}

// Nothing inside an owned group may redirect an operation across a mount,
// including a bind mount of a sibling on the very same cgroup filesystem.
fn child_file(parent: &File, name: &OsStr, flags: i32) -> Result<File> {
    let name = CString::new(name.as_bytes())?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_NO_XDEV | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_BENEATH;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

pub(super) struct Group {
    parent: File,
    dir: File,
    name: String,
    pub identity: Value,
}

#[derive(Debug)]
pub(super) struct AllocationFailure {
    pub diagnostic: anyhow::Error,
    pub group_cleanup: &'static str,
    pub path: Option<PathBuf>,
}

// Before mkdir succeeds, no group is owned and no rollback is necessary.
impl From<anyhow::Error> for AllocationFailure {
    fn from(diagnostic: anyhow::Error) -> Self {
        Self {
            diagnostic,
            group_cleanup: "not-created",
            path: None,
        }
    }
}

impl Group {
    pub fn create(root: &Path) -> Result<Self, AllocationFailure> {
        let parent = directory(root)?;
        check_cgroup(&parent)?;
        let name = format!("wfc-{}", fresh());
        let relative = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(&name);
        fs::create_dir(&relative).context("create delegated cgroup")?;
        Self::open_new(parent, root, name)
    }

    fn open_new(parent: File, root: &Path, name: String) -> Result<Self, AllocationFailure> {
        let path = root.join(&name);
        let initialized = (|| -> Result<_> {
            let dir = child_file(&parent, OsStr::new(&name), libc::O_DIRECTORY)?;
            let stat = dir.metadata()?;
            child_file(&dir, OsStr::new("cgroup.kill"), libc::O_WRONLY)?;
            let identity =
                json!({"kind":"cgroup-v2","path":path,"device":stat.dev(),"inode":stat.ino()});
            Ok((dir, identity))
        })();
        match initialized {
            Ok((dir, identity)) => Ok(Self {
                parent,
                dir,
                name,
                identity,
            }),
            Err(error) => {
                let relative =
                    PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name);
                let (group_cleanup, diagnostic) = match fs::remove_dir(relative) {
                    Ok(()) => ("removed", error),
                    Err(rollback) => (
                        "unknown",
                        error.context(format!("removing new cgroup failed: {rollback}")),
                    ),
                };
                Err(AllocationFailure {
                    diagnostic,
                    group_cleanup,
                    path: Some(path),
                })
            }
        }
    }

    fn file(&self, name: &str, flags: i32) -> Result<File> {
        child_file(&self.dir, OsStr::new(name), flags)
    }

    pub fn launch(&self, cmd: &mut Command) -> Result<Child> {
        let procs = self.file("cgroup.procs", libc::O_WRONLY)?;
        // write(2) is async-signal-safe. The child moves itself before exec,
        // so even a runtime killed on its first instruction cannot leak a child.
        unsafe {
            cmd.pre_exec(move || {
                if libc::write(procs.as_raw_fd(), b"0".as_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(cmd.spawn()?)
    }

    pub fn clean(&self) -> Result<()> {
        self.file("cgroup.kill", libc::O_WRONLY)?.write_all(b"1")?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let mut events = String::new();
            self.file("cgroup.events", libc::O_RDONLY)?
                .read_to_string(&mut events)?;
            if events.lines().any(|l| l == "populated 0") {
                break;
            }
            ensure!(Instant::now() < deadline, "cgroup still populated");
            thread::sleep(TICK);
        }
        let path =
            PathBuf::from(format!("/proc/self/fd/{}", self.parent.as_raw_fd())).join(&self.name);
        remove_empty_tree(&self.dir, &path)
    }
}

// cgroup.kill is recursive; rmdir requires that empty subgroups go first.
// Walk only directory entries on the pinned cgroup filesystem.
fn remove_empty_tree(dir: &File, name: &Path) -> Result<()> {
    let pinned = dir.metadata()?;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let sub = child_file(dir, &entry.file_name(), libc::O_DIRECTORY)?;
            remove_empty_tree(&sub, &entry.path())?;
        }
    }
    let named = fs::symlink_metadata(name)?;
    ensure!(
        named.dev() == pinned.dev() && named.ino() == pinned.ino(),
        "cgroup name changed"
    );
    fs::remove_dir(name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_failure_reports_whether_the_new_directory_was_removed() {
        let root = std::env::temp_dir().join(fresh());
        fs::create_dir(&root).unwrap();
        let name = "owned";
        let path = root.join(name);
        for nonempty in [false, true] {
            fs::create_dir(&path).unwrap();
            if nonempty {
                fs::write(path.join("kept"), "").unwrap();
            }
            // Force the post-mkdir control-file open to fail on a private fixture.
            let failure = Group::open_new(directory(&root).unwrap(), &root, name.into())
                .err()
                .unwrap();
            assert_eq!(failure.path.as_deref(), Some(path.as_path()));
            assert_eq!(
                failure.group_cleanup,
                if nonempty { "unknown" } else { "removed" }
            );
            assert_eq!(path.exists(), nonempty);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
