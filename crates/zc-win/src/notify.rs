//! A Windows desktop notification for a failure, for when the user is in a game and ZeroClip's
//! window is not in front.
//!
//! It is a tray balloon: a short-lived notification-area icon added with
//! [`Shell_NotifyIconW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw)
//! and `NIF_INFO`, which Windows 10 and 11 show as a toast in the action centre. No new
//! `windows` feature is needed (`Win32_UI_Shell` is on). The icon is removed again after a
//! few seconds. The owner window is the toast overlay's, so it needs no window of its own.

use std::ffi::c_void;
use std::time::Duration;

use windows::Win32::Foundation::HWND;

use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_TIP, NIIF_ERROR, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IDI_ERROR, LoadIconW,
};

/// Our own icon id, so it can't clash with the tray's.
const ICON_ID: u32 = 0x5a43;
/// How long the balloon's icon stays in the tray before it is removed.
const LIFETIME: Duration = Duration::from_secs(10);

/// Whether a window of this process has the keyboard focus.
pub fn app_is_foreground() -> bool {
    // SAFETY: both calls take plain values and write only to the local `pid`.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        pid == std::process::id()
    }
}

fn fill<const N: usize>(dst: &mut [u16; N], text: &str) {
    // Leave room for the closing NUL; the array starts zeroed.
    for (slot, unit) in dst.iter_mut().zip(text.encode_utf16().take(N - 1)) {
        *slot = unit;
    }
}

/// Shows a desktop notification with `title` and `text`.
///
/// # Errors
/// There is no overlay window to own the icon, or the shell refused it.
pub fn error(title: &str, text: &str) -> Result<(), String> {
    let hwnd = super::toast::window().ok_or("no window to own the notification")?;
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_ICON | NIF_TIP | NIF_INFO,
        dwInfoFlags: NIIF_ERROR,
        ..Default::default()
    };
    // SAFETY: loads a stock system icon, which needs no freeing.
    data.hIcon = unsafe { LoadIconW(None, IDI_ERROR) }.map_err(|e| e.to_string())?;
    fill(&mut data.szTip, "ZeroClip");
    fill(&mut data.szInfoTitle, title);
    fill(&mut data.szInfo, text);
    // SAFETY: `data` is fully initialised and lives across the call.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        return Err("the shell refused the notification".into());
    }
    let owner = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("zc-notify".into())
        .spawn(move || {
            std::thread::sleep(LIFETIME);
            // The shell finds the icon by window and id alone.
            let gone = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: HWND(owner as *mut c_void),
                uID: ICON_ID,
                ..Default::default()
            };
            // SAFETY: removes the icon added above; `gone` lives across the call.
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &gone) };
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}
