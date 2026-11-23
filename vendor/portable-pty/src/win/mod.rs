use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, Result as IoResult};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::WAIT_TIMEOUT;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};

pub mod conpty;
mod procthreadattr;
mod psuedocon;

use filedescriptor::OwnedHandle;

#[derive(Debug)]
pub struct WinChild {
    proc: Mutex<OwnedHandle>,
}

impl WinChild {
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

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.wait_for_exit(0) {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                struct PassRawHandleToWaiterThread(pub RawHandle);
                unsafe impl Send for PassRawHandleToWaiterThread {}

                let proc = self.proc.lock().unwrap().try_clone()?;
                let handle = PassRawHandleToWaiterThread(proc.as_raw_handle());

                let waker = cx.waker().clone();
                std::thread::spawn(move || {
                    unsafe {
                        WaitForSingleObject(handle.0 as _, INFINITE);
                    }
                    waker.wake();
                });
                Poll::Pending
            }
        }
    }
}
