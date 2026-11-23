//! One detached, single-threaded wfc owns and reaps one Linux worker tree.
use super::{read_record, state_dir, valid_id, valid_identity};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::{
    fs::{FileExt, MetadataExt, OpenOptionsExt},
    process::{CommandExt, ExitStatusExt},
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const CLOSE_USAGE: &str = "wfc close DISPATCH";
const POLL: Duration = Duration::from_millis(50);

pub fn command(program: &Path) -> Result<Command> {
    let mut owner = Command::new(std::env::current_exe()?);
    owner.arg("--worker-owner").arg(program);
    new_session(&mut owner);
    Ok(owner)
}

pub(super) fn new_session(command: &mut Command) {
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn launch(mut command: Command) -> Result<()> {
    let mut owner = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start detached worker owner")?;
    let mut ready = String::new();
    BufReader::new(owner.stdout.take().unwrap()).read_line(&mut ready)?;
    if ready.trim_end() != "ready" {
        let exit = owner.wait()?;
        bail!(
            "start controlled worker: {} (owner {exit})",
            ready.trim_end()
        );
    }
    Ok(())
}

fn owner_path(dir: &Path, record: &Value) -> Result<PathBuf> {
    let dispatch = record["dispatch"]
        .as_str()
        .filter(|id| valid_id(id))
        .context("invalid dispatch identity")?;
    let attempt = record["attempt"]
        .as_str()
        .filter(|id| valid_identity(id, "a-"))
        .context("invalid attempt identity")?;
    Ok(dir.join(format!("{dispatch}.{attempt}.owner")))
}

// Only an owner holds an exclusive lock. Readers acquire shared locks so a
// finished result is visible in full, including to concurrent close calls.
fn shared_lock(file: &File) -> Result<bool> {
    match file.try_lock_shared() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

// The first line starts with a one-byte close flag and binds the file to
// this state directory and dispatch/attempt. The second line is the outcome.
fn owner_identity(record: &Value) -> String {
    format!(" {} {} ", record["dispatch"], record["attempt"])
}

fn owner_header(dir: &Path, record: &Value) -> Result<String> {
    let meta = dir.metadata()?;
    Ok(format!(
        "0{}{}:{}\n",
        owner_identity(record),
        meta.dev(),
        meta.ino()
    ))
}

fn open_owner(dir: &Path, record: &Value, writable: bool) -> Result<(File, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(owner_path(dir, record)?)
        .context("open worker owner record")?;
    ensure!(file.metadata()?.is_file(), "invalid worker owner file");
    let unlocked = shared_lock(&file)?;
    // Completed results can be read from an archive. A live control file
    // belongs only to its original state directory, even if hard-linked.
    let identity = if unlocked {
        owner_identity(record)
    } else {
        owner_header(dir, record)?[1..].to_owned()
    };
    let mut actual = vec![0; identity.len()];
    file.read_exact_at(&mut actual, 1)?;
    ensure!(
        actual == identity.as_bytes(),
        "worker owner identity mismatch"
    );
    Ok((file, unlocked))
}

fn outcome(file: &File) -> Result<Value> {
    let result = BufReader::new(file)
        .lines()
        .nth(1)
        .transpose()?
        .context("worker owner stopped without a confirmed outcome")?;
    serde_json::from_str(&result).context("invalid worker outcome")
}

pub fn status(dir: &Path, record: &Value) -> Result<Value> {
    let (file, unlocked) = open_owner(dir, record, false)?;
    if unlocked {
        return Ok(outcome(&file).unwrap_or_else(|_| json!({"state":"unknown"})));
    }
    Ok(json!({"state":"running"}))
}

pub fn close(id: &OsStr) -> Result<Value> {
    let id = id
        .to_str()
        .filter(|id| valid_id(id))
        .context("invalid dispatch ID")?;
    let dir = state_dir()?.join("dispatches");
    let record = super::assignment::public_record(&dir, id)?
        .with_context(|| format!("unknown dispatch {id}"))?;
    ensure!(
        record["publicationDiagnostic"].is_null(),
        "close uncertain: {}",
        record["publicationDiagnostic"]
    );
    ensure!(
        record.get("controllerOwner").is_none_or(Value::is_null),
        "controller owner authorization required"
    );
    if record["host"].is_string() {
        return super::backend::close(dir.parent().context("missing state directory")?, &record);
    }
    // Older foreground records were only published after successful exit.
    let mut already_gone = true;
    if record.get("worker").is_some() {
        let (reader, unlocked) = open_owner(&dir, &record, false)?;
        let (owner, unlocked) = if unlocked {
            (reader, true)
        } else {
            open_owner(&dir, &record, true)?
        };
        already_gone = unlocked;
        if !already_gone {
            owner
                .write_all_at(b"1", 0)
                .context("request worker close")?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !shared_lock(&owner)? {
                ensure!(
                    Instant::now() < deadline,
                    "worker close still pending for {id}"
                );
                std::thread::sleep(POLL);
            }
        }
        outcome(&owner)?;
    }
    let mut receipt = json!({"operation":"close","result":"completed","dispatch":id,
        "round":record["round"],"attempt":record["attempt"]});
    if already_gone {
        receipt["diagnostic"] = json!("session was already gone");
    }
    Ok(receipt)
}

pub fn run(args: &[OsString]) -> Result<()> {
    let result = supervise(args);
    if let Err(err) = &result {
        // Launch errors travel on the private readiness pipe. It may already be
        // closed if the dispatcher exited or this is a later supervision error.
        let _ = writeln!(io::stdout().lock(), "error: {err:#}");
    }
    result
}

fn claim_process_identity() -> Result<File> {
    // /proc lists PIDs in the namespace of its mount. Only pass those PIDs to
    // kill when that namespace is also ours (e.g. inside a container).
    let status = fs::read_to_string("/proc/self/status")?;
    let nspid = status.lines().find_map(|line| line.strip_prefix("NSpid:"));
    ensure!(
        nspid.map(str::trim) == Some(std::process::id().to_string().as_str()),
        "worker owner needs procfs mounted for its PID namespace"
    );
    // Orphans, including descendants which create their own sessions, will
    // become direct children of this owner when their parent exits.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1
        || unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } == libc::SIG_ERR
    {
        return Err(io::Error::last_os_error()).context("configure worker owner");
    }
    // A nested dispatch can be adopted by this subreaper too. Each wfc
    // owner marks its own live process inode before launching any children.
    let process = File::open("/proc/self")?;
    process.lock()?;
    File::open("/proc/thread-self/children").context("read Linux child process identities")?;
    Ok(process)
}

fn supervise(args: &[OsString]) -> Result<()> {
    let (program, argv) = args.split_first().context("missing worker program")?;
    let dir = state_dir()?.join("dispatches");
    let id = std::env::var("WFC_DISPATCH")?;
    ensure!(valid_id(&id), "invalid dispatch ID");
    let pending = dir.join(format!("{id}.pending"));
    let record = read_record(&pending)?;
    ensure!(
        record["attempt"] == std::env::var("WFC_ATTEMPT")?,
        "dispatch identity mismatch"
    );
    let path = owner_path(&dir, &record)?;
    let _process_identity = claim_process_identity()?;
    let header = owner_header(&dir, &record)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let start = (|| -> Result<_> {
        file.lock()?;
        file.write_all_at(header.as_bytes(), 0)?;
        Command::new(program)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn worker")
    })();
    let child = match start {
        Ok(child) => child,
        Err(err) => {
            fs::remove_file(path)?;
            return Err(err);
        }
    };
    let published = fs::rename(&pending, dir.join(format!("{id}.json")));
    if published.is_ok() {
        let _ = writeln!(io::stdout().lock(), "ready");
    }
    let result = watch_tree(&file, child.id() as i32, published.is_err())?;
    if let Err(err) = published {
        fs::remove_file(path)?;
        return Err(err).context("publish dispatch record");
    }
    file.write_all_at(result.to_string().as_bytes(), header.len() as u64)?;
    file.sync_data()?;
    Ok(())
}

fn watch_tree(file: &File, root: i32, mut closing: bool) -> Result<Value> {
    let mut root_exit = None;
    let mut killed_root = false;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
        if pid > 0 {
            if pid == root && root_exit.is_none() {
                root_exit = Some(ExitStatus::from_raw(status));
            }
            continue;
        }
        if pid == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if !closing {
            let mut flag = [0];
            file.read_exact_at(&mut flag, 0)?;
            closing = &flag == b"1";
        }
        if closing || root_exit.is_some() {
            // Children remain pinned until our next waitpid. An independent
            // wfc owner's process lock protects its entire subtree. Hold our
            // shared lock through kill so a new owner cannot race this check.
            let mut nested = false;
            let mut owned = false;
            let children = fs::read_to_string("/proc/thread-self/children")?;
            for pid in children.split_whitespace() {
                let pid: i32 = pid.parse()?;
                let process = File::open(format!("/proc/{pid}"))?;
                if !shared_lock(&process)?
                    && unsafe { libc::getsid(pid) } == pid
                    && fs::read(format!("/proc/{pid}/cmdline"))?
                        .split(|byte| *byte == 0)
                        .nth(1)
                        == Some(b"--worker-owner")
                {
                    nested = true;
                    continue;
                }
                owned = true;
                if closing {
                    if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
                        let err = io::Error::last_os_error();
                        if err.raw_os_error() != Some(libc::ESRCH) {
                            return Err(err).context("stop owned worker");
                        }
                    } else if pid == root && root_exit.is_none() {
                        killed_root = true;
                    }
                }
            }
            if nested && !owned && root_exit.is_some() {
                break;
            }
        }
        std::thread::sleep(POLL);
    }
    let exit = root_exit.context("missing worker exit status")?;
    let state = if killed_root && exit.signal() == Some(libc::SIGKILL) {
        "cancelled"
    } else {
        "exited"
    };
    Ok(json!({"state":state,"exit_code":exit.code(),"signal":exit.signal()}))
}
