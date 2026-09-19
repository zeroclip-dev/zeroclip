//! Foreground-window tracking for the profile engine (DESIGN §7).

use windows::Win32::Foundation::HWND;
use zc_core::ports::{OsEvent, SourceError};

/// Installs the foreground hook on the current (message) thread.
///
/// Phase 2 plan: `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
/// None, Some(callback), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)`.
/// The callback is a plain `extern "system" fn` with no user-data pointer, so route
/// to the emitter through a `thread_local!` set up by this function. Report the
/// current foreground window once at install, too.
///
/// # Errors
/// The hook couldn't be installed.
pub fn install(emit: impl FnMut(OsEvent) + 'static) -> Result<(), SourceError> {
    let _ = emit;
    todo!("Phase 2: SetWinEventHook (see the plan above)")
}

/// The process behind a window: `GetWindowThreadProcessId` →
/// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → `QueryFullProcessImageNameW`.
/// `None` for windows we can't inspect (elevated processes, or it closed already).
#[must_use]
pub fn process_for_window(hwnd: HWND) -> Option<(u32, String)> {
    let _ = hwnd;
    todo!("Phase 2: window → pid → executable path")
}
