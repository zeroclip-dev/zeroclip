//! Single-instance guard.
//!
//! Two copies of ZeroClip would both open the disk-ring DVR file, and one would truncate the
//! other's buffer. [`acquire`] takes a named mutex with
//! [`CreateMutexW`](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createmutexw);
//! a second copy sees that the mutex already exists and backs off.
//!
//! The name uses the `Local\` namespace, so it is per logon session: fast user switching can run
//! one copy per user. The kernel closes the handle when the process dies, so a crash never leaves
//! the guard stuck.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
use windows::core::HSTRING;
use zc_core::ports::SourceError;

use crate::error::WinResultExt;

/// The mutex name the app uses. `Local\` keeps it per session.
pub const INSTANCE_MUTEX: &str = r"Local\ZeroClip.SingleInstance";

/// Holds the named mutex. Dropping it closes the handle, so the next copy may start.
///
/// Not `Send` or `Sync` (`HANDLE` isn't): keep it alive in `main`.
#[derive(Debug)]
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateMutexW` in `acquire`, this guard is its only
        // owner, and `drop` runs once, so it is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Takes the mutex called `name`. `Ok(None)` means another copy already holds it.
///
/// # Errors
/// `CreateMutexW` failed (for example the name is invalid or access was denied).
pub fn acquire(name: &str) -> Result<Option<InstanceGuard>, SourceError> {
    let name = HSTRING::from(name);
    // SAFETY: `name` is a valid NUL-terminated wide string that outlives the call. The
    // security attributes are `None` (default), and the mutex starts unowned: the handle's
    // existence is the lock, not ownership.
    let handle = unsafe { CreateMutexW(None, false, &name) }.context("CreateMutexW")?;
    // SAFETY: reads the calling thread's last-error value. Nothing runs between
    // `CreateMutexW` and this call that could overwrite it. On an existing mutex, it
    // succeeds with ERROR_ALREADY_EXISTS.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if existed {
        // SAFETY: `handle` is valid, owned here, and not used again.
        let _ = unsafe { CloseHandle(handle) };
        return Ok(None);
    }
    Ok(Some(InstanceGuard(handle)))
}

/// Tells the user why this copy is about to exit. A release build has no console, so without
/// this the second launch would just vanish. Blocks until the box is dismissed.
/// See [`MessageBoxW`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-messageboxw).
pub fn show_already_running() {
    let text = HSTRING::from("ZeroClip is already running. Switch to its window.");
    let title = HSTRING::from("ZeroClip");
    // SAFETY: both strings are valid NUL-terminated wide strings that outlive the call; no owner
    // window (None) is allowed. The return value (which button) carries no information for MB_OK.
    let _ = unsafe { MessageBoxW(None, &text, &title, MB_OK | MB_ICONINFORMATION) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_refused_until_the_first_drops() {
        let name = format!(r"Local\ZeroClip.Test.{}", std::process::id());
        let first = acquire(&name).unwrap();
        assert!(first.is_some());
        assert!(acquire(&name).unwrap().is_none());
        drop(first);
        assert!(acquire(&name).unwrap().is_some());
    }
}
