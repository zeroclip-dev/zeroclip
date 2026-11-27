//! Hides and shows the app's main window, for the tray.
//!
//! The window is hidden with [`ShowWindow`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-showwindow)
//! (`SW_HIDE`): it leaves the screen and the taskbar, and the process keeps running. The window
//! is found by title and owning process, because another program may have a window with the same
//! title (a folder named ZeroClip, for example).

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, SW_HIDE, SW_SHOW, SetForegroundWindow,
    ShowWindow,
};
use windows::core::BOOL;
use zc_core::ports::SourceError;

/// The title the app gives its main window.
pub const TITLE: &str = "ZeroClip";

struct Search {
    pid: u32,
    found: Option<HWND>,
}

/// Shows (`shown`) or hides this process's main window.
///
/// # Errors
/// No window titled [`TITLE`] belongs to this process (it was never created or was closed).
pub fn set_shown(shown: bool) -> Result<(), SourceError> {
    let hwnd = find_own_window().ok_or_else(|| SourceError::Platform {
        context: "FindMainWindow".to_owned(),
        code: 0,
        message: "the main window is not open".to_owned(),
    })?;
    // SAFETY: `hwnd` came from `EnumWindows` in this process, so it names a live window. The
    // return values only say whether the change happened; a failed show or focus leaves the
    // window as it was, so they are ignored.
    unsafe {
        let _ = ShowWindow(hwnd, if shown { SW_SHOW } else { SW_HIDE });
        if shown {
            let _ = SetForegroundWindow(hwnd);
        }
    }
    Ok(())
}

fn find_own_window() -> Option<HWND> {
    // SAFETY: `GetCurrentProcessId` has no preconditions and only reads the process id.
    let pid = unsafe { GetCurrentProcessId() };
    let mut search = Search { pid, found: None };
    // SAFETY: `search` lives on this stack frame until `EnumWindows` returns, and the callback
    // runs only inside that call, so the pointer in `LPARAM` is valid and not aliased while the
    // callback uses it. A callback that returns FALSE stops the walk early, which makes
    // `EnumWindows` report an error; that is expected, so the result is ignored.
    let _ = unsafe {
        EnumWindows(Some(match_own_window), LPARAM(std::ptr::addr_of_mut!(search) as isize))
    };
    search.found
}

/// `EnumWindows` callback: records the first top-level window of this process titled [`TITLE`]
/// and stops the walk.
///
/// # Safety
/// Only called by `EnumWindows` with the `Search` pointer passed by `find_own_window`.
unsafe extern "system" fn match_own_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Search` from `find_own_window`, valid for this call (see
    // there), and nothing else touches it while the callback runs.
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    let mut pid = 0_u32;
    // SAFETY: `hwnd` is a window handle the system passed in; `pid` is a valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != search.pid || window_title(hwnd) != TITLE {
        return BOOL(1);
    }
    search.found = Some(hwnd);
    BOOL(0)
}

/// The window's title, or an empty string when it has none or the call fails.
fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0_u16; 256];
    // SAFETY: `buffer` is a writable array of `buffer.len()` UTF-16 units, and the call writes at
    // most that many (including the terminator).
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    usize::try_from(length).map_or_else(
        |_| String::new(),
        |n| String::from_utf16_lossy(&buffer[..n.min(buffer.len())]),
    )
}
