//! Windows process handles and private job ownership.
use serde_json::{json, Value};
use std::{
    io,
    mem::{size_of, zeroed},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
        process::CommandExt,
    },
    process::{Child, Command},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use winapi::{
    shared::{
        minwindef::{FALSE, FILETIME},
        winerror::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT},
    },
    um::{
        jobapi::IsProcessInJob,
        jobapi2::{
            AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
            SetInformationJobObject, TerminateJobObject,
        },
        processthreadsapi::{GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess},
        synchapi::WaitForSingleObject,
        winbase::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, FILE_FLAG_OPEN_REPARSE_POINT,
            WAIT_OBJECT_0,
        },
        winnt::{
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE,
        },
    },
};

pub(crate) const OPEN_REPARSE_POINT: u32 = FILE_FLAG_OPEN_REPARSE_POINT;
pub(crate) const NO_WINDOW: u32 = CREATE_NO_WINDOW;
pub(crate) const TICK: Duration = Duration::from_millis(25);

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
    // sending authority to launch a worker or register a relay.
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

pub(crate) struct Job(OwnedHandle);

impl Job {
    pub(crate) fn new() -> io::Result<Self> {
        // A null SECURITY_ATTRIBUTES gives the owner a non-inheritable handle.
        let raw = unsafe { CreateJobObjectW(null_mut(), null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw.cast()) });
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        check(unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;
        Ok(job)
    }

    // The child is waiting on an inherited pipe and cannot spawn descendants
    // before the caller sends its first instruction.
    pub(crate) fn assign(&self, child: &Child) -> io::Result<()> {
        check(unsafe {
            AssignProcessToJobObject(self.0.as_raw_handle().cast(), child.as_raw_handle().cast())
        })
    }

    pub(crate) fn contains(&self, process: RawHandle) -> io::Result<bool> {
        in_job(process, self.0.as_raw_handle().cast())
    }

    pub(crate) fn active(&self) -> io::Result<u32> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        check(unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle().cast(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        })?;
        Ok(info.ActiveProcesses)
    }

    pub(crate) fn stop(&self) -> io::Result<()> {
        if self.active()? != 0 {
            let terminate = check(unsafe { TerminateJobObject(self.0.as_raw_handle().cast(), 1) });
            if terminate.is_err() && self.active()? != 0 {
                terminate?;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
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

fn in_job(process: RawHandle, job: winapi::um::winnt::HANDLE) -> io::Result<bool> {
    let mut member = FALSE;
    check(unsafe { IsProcessInJob(process.cast(), job, &mut member) })?;
    Ok(member != FALSE)
}

pub(crate) fn identity(pid: u32, process: RawHandle) -> io::Result<Value> {
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
    let filetime = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Ok(json!({"pid":pid,"creation":filetime.to_string()}))
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
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid) };
    if raw.is_null() {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    Ok((identity(pid, process.as_raw_handle())? == *expected).then_some(process))
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
