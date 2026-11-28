//! Windows process handles and private job ownership.
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
    mem::{size_of, zeroed},
    os::windows::{
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle, RawHandle},
        process::CommandExt,
    },
    process::{Child, Command},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use winapi::{
    shared::{
        minwindef::{FALSE, FILETIME},
        winerror::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT},
    },
    um::{
        handleapi::INVALID_HANDLE_VALUE,
        ioapiset::{CreateIoCompletionPort, GetQueuedCompletionStatus},
        jobapi::IsProcessInJob,
        jobapi2::{
            AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
            SetInformationJobObject, TerminateJobObject,
        },
        processthreadsapi::{GetCurrentProcess, GetExitCodeProcess, GetProcessId, GetProcessTimes, OpenProcess},
        synchapi::WaitForSingleObject,
        winbase::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, FILE_FLAG_OPEN_REPARSE_POINT,
            WAIT_OBJECT_0,
        },
        winnt::{
            JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_MSG_NEW_PROCESS,
            PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE,
        },
    },
};

pub(crate) const OPEN_REPARSE_POINT: u32 = FILE_FLAG_OPEN_REPARSE_POINT;
pub(crate) const NO_WINDOW: u32 = CREATE_NO_WINDOW;
pub(crate) const TICK: Duration = Duration::from_millis(25);
const JOB_STOP_LIMIT: Duration = Duration::from_secs(5);

// Keep only a caller-provided operation and a typed system error. Error
// chains from process libraries can contain launch arguments or paths.
pub(crate) fn diagnostic(operation: &str, error: Option<&io::Error>) -> String {
    match error {
        Some(error) => match error.raw_os_error() {
            Some(code) => format!("{operation}: {}", io::Error::from_raw_os_error(code)),
            None => format!("{operation}: {}", error.kind()),
        },
        None => operation.to_owned(),
    }
}

pub(crate) fn launch_category(error: Option<&io::Error>) -> &'static str {
    match error.map(io::Error::kind) {
        Some(io::ErrorKind::NotFound) => "executable_missing",
        Some(io::ErrorKind::PermissionDenied) => "permission_denied",
        _ => "spawn_failed",
    }
}

pub(crate) fn background(command: &mut Command) -> anyhow::Result<Child> {
    // Both callers gate their child on stdin. Verify detachment before
    // sending authority to launch a worker or deliver relay messages.
    let mut child = match command
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            // An enclosing Job can grant silent breakaway without granting
            // the explicit creation flag.
            command.creation_flags(CREATE_NO_WINDOW).spawn()
        }
        result => result,
    }?;
    match in_job(child.as_raw_handle(), null_mut()) {
        Ok(false) => Ok(child),
        Ok(true) => {
            stop_staged(&mut child)?;
            Err(
                anyhow::Error::new(io::Error::from(io::ErrorKind::PermissionDenied))
                    .context("enclosing Windows Job prevents detachment"),
            )
        }
        Err(error) => {
            stop_staged(&mut child)?;
            Err(anyhow::Error::new(error).context("verify Windows staging process Job membership"))
        }
    }
}

#[derive(Debug)]
pub(crate) struct UnverifiedStageExit;

impl std::fmt::Display for UnverifiedStageExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not verify exit of the Windows staging process")
    }
}

impl std::error::Error for UnverifiedStageExit {}

pub(crate) fn stop_staged(child: &mut Child) -> anyhow::Result<()> {
    use anyhow::Context;
    drop(child.stdin.take());
    child
        .kill()
        .or_else(|error| exit_code(child.as_raw_handle())?.map(|_| ()).ok_or(error))
        .context(UnverifiedStageExit)?;
    let result = unsafe { WaitForSingleObject(child.as_raw_handle().cast(), 5000) };
    anyhow::ensure!(result == WAIT_OBJECT_0, UnverifiedStageExit);
    Ok(())
}

pub(crate) struct Job {
    handle: Arc<OwnedHandle>,
    watch: Option<JobWatch>,
}

struct JobWatch {
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    members: Arc<Mutex<JobMembers>>,
}

#[derive(Default)]
struct JobMembers {
    waiting: HashMap<(u32, u64), OwnedHandle>,
    exited: HashSet<(u32, u64)>,
}

impl JobMembers {
    fn record(&mut self, pid: u32, process: OwnedHandle) -> io::Result<()> {
        let identity = (pid, creation_time(process.as_raw_handle())?);
        if !self.exited.contains(&identity) {
            self.waiting.entry(identity).or_insert(process);
        }
        Ok(())
    }

    fn reap(&mut self) {
        self.waiting.retain(|identity, process| {
            if matches!(exit_code(process.as_raw_handle()), Ok(Some(_))) {
                self.exited.insert(*identity);
                false
            } else {
                true
            }
        });
    }
}

impl JobWatch {
    fn new(job: Arc<OwnedHandle>) -> io::Result<Self> {
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        let port = unsafe { OwnedHandle::from_raw_handle(port.cast()) };
        let mut association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: 1usize as *mut _,
            CompletionPort: port.as_raw_handle().cast(),
        };
        check(unsafe {
            SetInformationJobObject(
                job.as_raw_handle().cast(),
                JobObjectAssociateCompletionPortInformation,
                (&raw mut association).cast(),
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let members = Arc::new(Mutex::new(JobMembers::default()));
        let observed = members.clone();
        let reader = thread::Builder::new().spawn(move || {
            let mut next_reap = Instant::now();
            while !stopped.load(Ordering::Relaxed) {
                if Instant::now() >= next_reap {
                    let Ok(mut observed) = observed.lock() else { break };
                    observed.reap();
                    next_reap = Instant::now() + Duration::from_millis(250);
                }
                let (mut message, mut key, mut value) = (0, 0, null_mut());
                let ok = unsafe {
                    GetQueuedCompletionStatus(port.as_raw_handle().cast(), &mut message,
                        &mut key, &mut value, TICK.as_millis() as u32)
                };
                if ok == FALSE {
                    if io::Error::last_os_error().raw_os_error() != Some(WAIT_TIMEOUT as i32) {
                        break;
                    }
                    continue;
                }
                if key == 1 && message == JOB_OBJECT_MSG_NEW_PROCESS {
                    if let Ok(pid) = u32::try_from(value as usize) {
                        if let Ok(Some(process)) = open_pid(pid) {
                            if matches!(in_job(process.as_raw_handle(), job.as_raw_handle().cast()), Ok(true)) {
                                // Retain the original object even after its PID leaves the Job list.
                                let Ok(mut observed) = observed.lock() else { break };
                                let _ = observed.record(pid, process);
                            }
                        }
                    }
                }
            }
        })?;
        Ok(Self { stop, reader: Some(reader), members })
    }

    fn members(&self) -> io::Result<MutexGuard<'_, JobMembers>> {
        self.members.lock().map_err(|_| unverified_members())
    }

    fn wait_for_members(&self, total: u32, deadline: Instant) -> io::Result<()> {
        loop {
            {
                let mut members = self.members()?;
                members.reap();
                if members.exited.len() == total as usize && members.waiting.is_empty() {
                    return Ok(());
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(unverified_members());
            }
            thread::sleep(TICK.min(remaining));
        }
    }
}

impl Drop for JobWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Job {
    pub(crate) fn new() -> io::Result<Self> {
        // A null SECURITY_ATTRIBUTES gives the owner a non-inheritable handle.
        let raw = unsafe { CreateJobObjectW(null_mut(), null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self {
            handle: Arc::new(unsafe { OwnedHandle::from_raw_handle(raw.cast()) }),
            watch: None,
        };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        check(unsafe {
            SetInformationJobObject(
                job.handle.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;
        Ok(job)
    }

    pub(crate) fn new_tracked() -> io::Result<Self> {
        let mut job = Self::new()?;
        // Associate while the Job is still empty. Notifications discover PIDs,
        // while the final lifetime count detects a missed process.
        job.watch = Some(JobWatch::new(job.handle.clone())?);
        Ok(job)
    }

    // The child is waiting on an inherited pipe and cannot spawn descendants
    // before the caller sends its first instruction.
    pub(crate) fn assign(&self, child: &Child) -> io::Result<()> {
        check(unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle().cast(), child.as_raw_handle().cast())
        })?;
        self.track(child.as_handle())
    }

    pub(crate) fn track(&self, original: BorrowedHandle<'_>) -> io::Result<()> {
        if let Some(watch) = &self.watch {
            if !self.contains(original.as_raw_handle())? {
                return Ok(());
            }
            let pid = unsafe { GetProcessId(original.as_raw_handle().cast()) };
            if pid == 0 {
                return Err(io::Error::last_os_error());
            }
            let process = original.try_clone_to_owned()?;
            watch.members()?.record(pid, process)?;
        }
        Ok(())
    }

    pub(crate) fn contains(&self, process: RawHandle) -> io::Result<bool> {
        in_job(process, self.handle.as_raw_handle().cast())
    }

    pub(crate) fn active(&self) -> io::Result<u32> {
        Ok(self.accounting()?.ActiveProcesses)
    }

    fn accounting(&self) -> io::Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        check(unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle().cast(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        })?;
        Ok(info)
    }

    pub(crate) fn stop_and_verify(&self) -> io::Result<()> {
        let deadline = Instant::now() + JOB_STOP_LIMIT;
        self.stop_until(deadline)?;
        self.confirm_stopped(deadline)
    }

    fn confirm_stopped(&self, deadline: Instant) -> io::Result<()> {
        let watch = self.watch.as_ref().ok_or_else(unverified_members)?;
        let total = self.accounting()?.TotalProcesses;
        watch.wait_for_members(total, deadline)?;
        let after = self.accounting()?;
        if after.ActiveProcesses != 0 || after.TotalProcesses != total {
            return Err(unverified_members());
        }
        Ok(())
    }

    pub(crate) fn stop(&self) -> io::Result<()> {
        self.stop_until(Instant::now() + JOB_STOP_LIMIT)
    }

    fn stop_until(&self, deadline: Instant) -> io::Result<()> {
        if self.active()? != 0 {
            let terminate = check(unsafe { TerminateJobObject(self.handle.as_raw_handle().cast(), 1) });
            if terminate.is_err() && self.active()? != 0 {
                terminate?;
            }
        }
        while self.active()? != 0 {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Windows job has active processes",
                ));
            }
            std::thread::sleep(TICK);
        }
        Ok(())
    }
}

fn unverified_members() -> io::Error {
    io::Error::other("could not confirm membership of all Windows Job processes")
}

fn in_job(process: RawHandle, job: winapi::um::winnt::HANDLE) -> io::Result<bool> {
    let mut member = FALSE;
    check(unsafe { IsProcessInJob(process.cast(), job, &mut member) })?;
    Ok(member != FALSE)
}

fn creation_time(process: RawHandle) -> io::Result<u64> {
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { zeroed() };
    check(unsafe {
        GetProcessTimes(
            process.cast(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    })?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

pub(crate) fn identity(pid: u32, process: RawHandle) -> io::Result<Value> {
    Ok(json!({"pid":pid,"creation":creation_time(process)?.to_string()}))
}

pub(crate) fn current_identity() -> io::Result<Value> {
    identity(std::process::id(), unsafe { GetCurrentProcess() }.cast())
}

pub(crate) fn open(expected: &Value) -> io::Result<Option<OwnedHandle>> {
    let pid = expected["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process identity"))?;
    let Some(process) = open_pid(pid)? else {
        return Ok(None);
    };
    Ok((identity(pid, process.as_raw_handle())? == *expected).then_some(process))
}

fn open_pid(pid: u32) -> io::Result<Option<OwnedHandle>> {
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid) };
    if raw.is_null() {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(raw.cast()) }))
}

pub(crate) fn exit_code(process: RawHandle) -> io::Result<Option<u32>> {
    wait_for_exit(process, 0)
}

pub(crate) fn wait_for_exit(process: RawHandle, timeout_ms: u32) -> io::Result<Option<u32>> {
    match unsafe { WaitForSingleObject(process.cast(), timeout_ms) } {
        WAIT_OBJECT_0 => {
            let mut code = 0;
            check(unsafe { GetExitCodeProcess(process.cast(), &mut code) })?;
            Ok(Some(code))
        }
        WAIT_TIMEOUT => Ok(None),
        _ => Err(io::Error::last_os_error()),
    }
}

fn check(result: i32) -> io::Result<()> {
    if result == FALSE {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        process::Stdio,
        sync::mpsc,
        thread,
    };

    const FIXTURE: &str = "WFC_TEST_PRIVATE_JOB_MEMBER";

    pub(crate) fn fixture(kind: &str) -> io::Result<Child> {
        Command::new(std::env::current_exe()?)
            .args(["--exact", "windows_process::tests::job_member_fixture", "--nocapture"])
            .env(FIXTURE, kind)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
    }

    #[test]
    fn job_member_fixture() -> anyhow::Result<()> {
        match std::env::var(FIXTURE).as_deref() {
            Ok("parent") => {
                let mut children = Vec::new();
                let nested = Job::new()?;
                let mut command = [0];
                while io::stdin().read_exact(&mut command).is_ok() {
                    if command == *b"s" {
                        nested.stop()?;
                        println!("\nmember stopped");
                        continue;
                    }
                    let child = fixture("leaf")?;
                    if command == *b"n" {
                        nested.assign(&child)?;
                    }
                    let process = identity(child.id(), child.as_raw_handle())?;
                    println!("\nmember {process}");
                    children.push(child);
                }
                for mut child in children {
                    drop(child.stdin.take());
                    child.wait()?;
                }
            }
            Ok("leaf") => {
                // The inherited pipe stays open and the leaf blocks in a read.
                let _ = io::stdin().read_exact(&mut [0]);
            }
            Ok("exit") => {
                let _ = io::stdin().read_exact(&mut [0]);
                std::process::exit(37);
            }
            _ => (),
        }
        Ok(())
    }

    #[test]
    fn job_confirms_original_descendant_handles_including_nested_exits() -> anyhow::Result<()> {
        use anyhow::Context;
        for nested_exit in [false, true] {
            let job = Job::new_tracked()?;
            let mut parent = fixture("parent")?;
            job.assign(&parent)?;
            let stdout = parent.stdout.take().context("missing fixture output")?;
            let (send, receive) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(value) = line.strip_prefix("member ") {
                        if send.send(value.to_owned()).is_err() {
                            break;
                        }
                    }
                }
            });
            let mut leaf = |command: &[u8]| -> anyhow::Result<OwnedHandle> {
                parent.stdin.as_mut().context("missing fixture input")?.write_all(command)?;
                let json = receive.recv_timeout(Duration::from_secs(5))?;
                let leaf = open(&serde_json::from_str(&json)?)?.context("missing fixture leaf")?;
                assert!(job.contains(leaf.as_raw_handle())?);
                assert!(exit_code(leaf.as_raw_handle())?.is_none());
                Ok(leaf)
            };
            let original = leaf(if nested_exit { b"n" } else { b"1" })?;
            if nested_exit {
                let watch = job.watch.as_ref().unwrap();
                // Let the Job watcher pin the leaf before removing it from
                // the outer Job's active accounting via the nested Job.
                let deadline = Instant::now() + JOB_STOP_LIMIT;
                while watch.members()?.waiting.len() < 2 {
                    assert!(Instant::now() < deadline, "nested leaf was not pinned");
                    thread::sleep(TICK);
                }
            }
            let late = leaf(b"1")?;
            if nested_exit {
                parent.stdin.as_mut().unwrap().write_all(b"s")?;
                assert_eq!(receive.recv_timeout(JOB_STOP_LIMIT)?, "stopped");
                let deadline = Instant::now() + JOB_STOP_LIMIT;
                while job.active()? != 2 {
                    assert!(Instant::now() < deadline, "nested leaf still active");
                    thread::sleep(TICK);
                }
            }
            job.stop_and_verify()?;
            assert!(exit_code(original.as_raw_handle())?.is_some());
            assert!(exit_code(late.as_raw_handle())?.is_some());
            assert!(parent.try_wait()?.is_some());
        }
        Ok(())
    }

    #[test]
    fn job_zero_without_all_original_handles_is_unconfirmed() -> anyhow::Result<()> {
        // Simulate a missed notification by attaching the watcher only after
        // the real Job member has left the active list.
        let mut job = Job::new()?;
        let mut leaf = fixture("leaf")?;
        job.assign(&leaf)?;
        job.stop()?;
        leaf.wait()?;
        assert_eq!(job.active()?, 0);
        assert_eq!(job.accounting()?.TotalProcesses, 1);
        job.watch = Some(JobWatch::new(job.handle.clone())?);
        assert!(job.confirm_stopped(Instant::now()).is_err());
        Ok(())
    }
}
