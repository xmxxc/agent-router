use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, Result as IoResult};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::WAIT_TIMEOUT;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject};
use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};

pub mod conpty;
mod procthreadattr;
mod psuedocon;

use filedescriptor::OwnedHandle;

#[derive(Debug)]
pub struct WinChild {
    proc: Mutex<OwnedHandle>,
    waiter: Option<ExitWaiter>,
}

impl WinChild {
    fn new(proc: OwnedHandle) -> Self {
        Self {
            proc: Mutex::new(proc),
            waiter: None,
        }
    }

    fn wait_for_exit(&self, timeout: DWORD) -> IoResult<Option<ExitStatus>> {
        let proc = self.proc.lock().unwrap();
        let handle = proc.as_raw_handle() as _;
        // TerminateProcess is asynchronous. The process object signals when exit is complete.
        match unsafe { WaitForSingleObject(handle, timeout) } {
            WAIT_OBJECT_0 => {
                let mut status = 0;
                if unsafe { GetExitCodeProcess(handle, &mut status) } == 0 {
                    return Err(IoError::last_os_error());
                }
                Ok(Some(ExitStatus::with_exit_code(status)))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(IoError::last_os_error()),
        }
    }
}

#[derive(Debug)]
struct ExitWaiter {
    cancel: OwnedHandle,
    state: Arc<Mutex<WaitState>>,
}

#[derive(Debug)]
struct WaitState {
    waker: Waker,
    failure: Option<IoError>,
}

impl ExitWaiter {
    fn start(proc: OwnedHandle, waker: Waker) -> anyhow::Result<Self> {
        let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(IoError::last_os_error().into());
        }
        let cancel = unsafe { OwnedHandle::from_raw_handle(event as _) };
        let thread_cancel = cancel.try_clone()?;
        let state = Arc::new(Mutex::new(WaitState {
            waker,
            failure: None,
        }));
        let thread_state = state.clone();
        std::thread::Builder::new().spawn(move || {
            // Both handles remain owned by this thread throughout the wait.
            let handles = [
                proc.as_raw_handle() as _,
                thread_cancel.as_raw_handle() as _,
            ];
            let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            if result == WAIT_OBJECT_0 + 1 {
                return;
            }
            let waker = {
                let mut state = thread_state.lock().unwrap();
                if result != WAIT_OBJECT_0 {
                    state.failure = Some(IoError::last_os_error());
                }
                state.waker.clone()
            };
            waker.wake();
        })?;
        Ok(Self { cancel, state })
    }
}

impl Drop for ExitWaiter {
    fn drop(&mut self) {
        unsafe { SetEvent(self.cancel.as_raw_handle() as _) };
    }
}

fn terminate_process(proc: &OwnedHandle) -> IoResult<()> {
    let handle = proc.as_raw_handle() as _;
    if unsafe { TerminateProcess(handle, 1) } != 0 {
        return Ok(());
    }
    let err = IoError::last_os_error();
    // A natural exit can race the termination request.
    if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(err)
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        terminate_process(&self.proc.lock().unwrap())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

#[derive(Debug)]
pub struct WinChildKiller {
    proc: OwnedHandle,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        terminate_process(&self.proc)
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.wait_for_exit(0)
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        self.wait_for_exit(INFINITE)?
            .ok_or_else(|| IoError::new(std::io::ErrorKind::TimedOut, "process wait timed out"))
    }

    fn process_id(&self) -> Option<u32> {
        let res = unsafe { GetProcessId(self.proc.lock().unwrap().as_raw_handle() as _) };
        if res == 0 {
            None
        } else {
            Some(res)
        }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = self.proc.lock().unwrap();
        Some(proc.as_raw_handle())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.wait_for_exit(0) {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                if let Some(waiter) = &self.waiter {
                    let mut state = waiter.state.lock().unwrap();
                    if let Some(error) = state.failure.take() {
                        return Poll::Ready(Err(error).context("Failed to wait for process exit"));
                    }
                    state.waker.clone_from(cx.waker());
                    drop(state);
                    // The waiter may already have woken a previous executor.
                    return match self.wait_for_exit(0) {
                        Ok(None) => Poll::Pending,
                        Ok(Some(status)) => Poll::Ready(Ok(status)),
                        Err(error) => Poll::Ready(Err(error.into())),
                    };
                }
                let proc = self.proc.lock().unwrap().try_clone()?;
                self.waiter = Some(ExitWaiter::start(proc, cx.waker().clone())?);
                Poll::Pending
            }
        }
    }
}
