//! Watching a game process until it exits, for crash rescue.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
use zc_core::ports::{Emit, OsEvent, SourceError};
use zc_win::error::WinResultExt;

/// Reports [`OsEvent::ProcessExited`] once `pid` ends.
///
/// Opens the process with only `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`, rights
/// Windows grants even for most protected and anti-cheat processes, then parks a thread in
/// [WaitForSingleObject](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject)
/// until the process handle is signalled. Only then is
/// [GetExitCodeProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess)
/// asked: before exit it returns `STILL_ACTIVE` (259), which would look like a crash code.
///
/// If the wait or the exit-code query fails, the event is still sent (so the caller stops
/// tracking the pid) with [`UNKNOWN_EXIT_CODE`]: not 0, so it is never mistaken for a clean
/// quit and crash rescue errs on the side of saving the footage.
///
/// # Errors
/// The process couldn't be opened (already gone, or access denied), or the watcher thread
/// couldn't start.
// ponytail: one parked thread per watched game, never cancelled; fine for a handful of games
// per session. RegisterWaitForSingleObject if that ever matters.
pub fn watch(pid: u32, emit: Emit<OsEvent>) -> Result<(), SourceError> {
    // SAFETY: OpenProcess takes no pointers; a bad pid just returns an error.
    let handle =
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .context("OpenProcess")?;
    // SAFETY: OpenProcess succeeded, so this is a valid, non-null handle we own; nothing else
    // closes it. OwnedHandle is Send (HANDLE isn't) and closes it on drop, including when the
    // thread fails to start and the closure is dropped unrun.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    std::thread::Builder::new()
        .name(format!("zc-procwatch-{pid}"))
        .spawn(move || {
            let handle = HANDLE(owned.as_raw_handle());
            emit(OsEvent::ProcessExited { pid, exit_code: wait_for_exit(handle) });
        })
        .map_err(|e| {
            SourceError::Other(format!("couldn't start the process watcher thread: {e}"))
        })?;
    Ok(())
}

/// `exit_code` when the wait or [GetExitCodeProcess] failed. Never a real exit code in
/// practice (it would be NTSTATUS severity "error", facility 0xFFF).
///
/// [GetExitCodeProcess]: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess
pub const UNKNOWN_EXIT_CODE: u32 = u32::MAX;

/// Blocks until `handle`'s process ends and returns its exit code, or [`UNKNOWN_EXIT_CODE`].
fn wait_for_exit(handle: HANDLE) -> u32 {
    // SAFETY: `handle` is a live process handle with SYNCHRONIZE, owned by the caller for the
    // whole call.
    if unsafe { WaitForSingleObject(handle, INFINITE) } != WAIT_OBJECT_0 {
        return UNKNOWN_EXIT_CODE;
    }
    let mut code = 0u32;
    // SAFETY: a live handle with PROCESS_QUERY_LIMITED_INFORMATION; `code` is a valid
    // out-pointer.
    match unsafe { GetExitCodeProcess(handle, &raw mut code) } {
        Ok(()) => code,
        Err(_) => UNKNOWN_EXIT_CODE,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::*;

    #[test]
    fn reports_the_exit_code_of_a_watched_process() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 2 127.0.0.1 >nul & exit 3"])
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        watch(child.id(), Arc::new(move |e| tx.send(e).unwrap())).unwrap();
        let event = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(event, OsEvent::ProcessExited { exit_code: 3, .. }), "{event:?}");
        let _ = child.wait();
    }
}
