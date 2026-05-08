use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, Result as IoResult};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};
use winapi::shared::minwindef::DWORD;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{INFINITE, WAIT_FAILED};

pub mod conpty;
mod procthreadattr;
mod psuedocon;

use filedescriptor::OwnedHandle;

#[derive(Debug)]
pub struct WinChild {
    proc: Mutex<OwnedHandle>,
    /// Tracks whether a waiter thread has been spawned to avoid thread accumulation.
    /// Once a waiter thread is spawned, subsequent poll() calls should not spawn more.
    waiter_spawned: AtomicBool,
}

impl WinChild {
    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            if status == STILL_ACTIVE {
                Ok(None)
            } else {
                Ok(Some(ExitStatus::with_exit_code(status)))
            }
        } else {
            // Log the error but return None to allow retry.
            // Note: we do NOT assume the process has exited even if the
            // handle is invalid, because on Windows/ConPTY the handle
            // state can be transiently inconsistent while the process
            // is still alive (e.g. during REPL sessions).
            let err = IoError::last_os_error();
            log::warn!("GetExitCodeProcess failed: {:?}", err);
            Ok(None)
        }
    }

    fn do_kill(&mut self) -> IoResult<()> {
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
        // TerminateProcess returns non-zero on SUCCESS, zero on FAILURE
        if res == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        self.do_kill().ok();
        Ok(())
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
        let res = unsafe { TerminateProcess(self.proc.as_raw_handle() as _, 1) };
        // TerminateProcess returns non-zero on SUCCESS, zero on FAILURE
        if res == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Ok(Some(status)) = self.try_wait() {
            return Ok(status);
        }
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let wait_result = unsafe { WaitForSingleObject(proc.as_raw_handle() as _, INFINITE) };
        // Check if WaitForSingleObject failed
        if wait_result == WAIT_FAILED {
            return Err(IoError::last_os_error());
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
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
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                // Only spawn a waiter thread if one hasn't been spawned yet.
                // This prevents thread accumulation when poll() is called multiple times.
                if !self.waiter_spawned.swap(true, Ordering::SeqCst) {
                    struct PassRawHandleToWaiterThread(pub RawHandle);
                    unsafe impl Send for PassRawHandleToWaiterThread {}

                    let proc = self.proc.lock().unwrap().try_clone()?;
                    let handle = PassRawHandleToWaiterThread(proc.as_raw_handle());

                    let waker = cx.waker().clone();
                    std::thread::spawn(move || {
                        // Use a loop with a finite timeout so that we don't
                        // leak a permanently blocked thread if the handle
                        // becomes invalid, but also don't give up on long-
                        // running processes like REPLs.
                        const POLL_WAIT_MS: DWORD = 30000; // 30 seconds
                        loop {
                            let result = unsafe { WaitForSingleObject(handle.0 as _, POLL_WAIT_MS) };
                            if result == WAIT_FAILED {
                                log::warn!("WaitForSingleObject failed in poll(): {:?}", IoError::last_os_error());
                                waker.wake();
                                break;
                            }
                            if result != winapi::shared::winerror::WAIT_TIMEOUT {
                                // Process exited
                                waker.wake();
                                break;
                            }
                            // Timeout: process still running. Continue waiting.
                        }
                    });
                }
                Poll::Pending
            }
        }
    }
}
