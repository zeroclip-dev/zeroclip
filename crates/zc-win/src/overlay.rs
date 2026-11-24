//! Click-through, capture-excluded toast windows.
//!
//! The app opens a small GPUI window for each event (clip saved, recording, screenshot). GPUI
//! creates it as a borderless top-most tool window. This module then:
//!
//! - adds `WS_EX_TRANSPARENT` (mouse clicks pass through to the game) and `WS_EX_NOACTIVATE`
//!   (showing it never takes focus from the game);
//! - calls [`SetWindowDisplayAffinity`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity)
//!   with `WDA_EXCLUDEFROMCAPTURE`, so screen capture and screenshots leave it out. This is a
//!   public API and needs Windows 10 2004 or later;
//! - shows it with `SW_SHOWNOACTIVATE`. This happens after the affinity is set, so the card is
//!   never on screen while capturable.
//!
//! Limit: borderless and windowed games only. An exclusive-fullscreen game owns the display
//! and composites nothing over itself, so the card does not show there. No DLL injection and
//! no hooks: only public Win32 calls.
//!
//! If anything fails, [`prepare_toast`] returns an error and the caller closes the window. A
//! toast that can't be excluded from capture is not shown.

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, SPI_GETCLIENTAREAANIMATION,
    SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, SystemParametersInfoW, WDA_EXCLUDEFROMCAPTURE, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::{BOOL, HSTRING, PCWSTR};

/// Applies the toast styles to the window titled `title`, then shows it without activating it.
///
/// The title must be unique among top-level windows: the caller builds it from a counter.
///
/// # Errors
/// No window has that title, or the affinity call failed. Nothing is shown in that case.
pub fn prepare_toast(title: &str) -> windows::core::Result<()> {
    let title = HSTRING::from(title);
    // SAFETY: `title` is a valid NUL-terminated wide string that lives across the call. A null
    // class name matches any class.
    let hwnd: HWND = unsafe { FindWindowW(PCWSTR::null(), &title) }?;

    // SAFETY: `hwnd` is a live window of this process, found just now. Reading its extended
    // style has no side effects.
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let added = (WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0) as isize;
    // SAFETY: `hwnd` is still the same live window. We only add bits to its extended style, so
    // its other flags (tool window, top-most) stay as GPUI set them.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style | added) };

    // SAFETY: `hwnd` is still live. The affinity is set before the window is ever shown, so the
    // card is never visible to a capture. Errors are returned, not ignored: no affinity, no card.
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }?;

    // GPUI makes a `PopUp` window with style 0 (`WS_OVERLAPPED`), which still has a title bar and
    // a frame. Make it a plain `WS_POPUP`, like OpenCrosshair's overlay, so nothing is drawn
    // outside the client area, then tell Windows the frame changed.
    // SAFETY: `hwnd` is still live. Replacing the style only drops the frame bits; SetWindowPos
    // with these flags moves, resizes and activates nothing.
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, WS_POPUP.0 as isize);
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
    }

    // Windows 11 draws a thin border (and rounds the corners) around top-level windows, even a
    // borderless one, so the toast would not look transparent. Turn both off.
    // https://learn.microsoft.com/windows/win32/api/dwmapi/nf-dwmapi-dwmsetwindowattribute
    // Best effort: older Windows 10 builds don't know these attributes and just ignore them.
    let no_border: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
    let square: i32 = 1; // DWMWCP_DONOTROUND
    // SAFETY: `hwnd` is still live. Each pointer is to a local that lives across its call, and the
    // size passed is that local's size (4 bytes), as the attribute requires.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const no_border).cast::<c_void>(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const square).cast::<c_void>(),
            4,
        );
    }

    // SAFETY: `hwnd` is still live. SW_SHOWNOACTIVATE shows the window without giving it focus.
    // The return value only says whether it was visible before, so it is not needed.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    Ok(())
}

/// Whether the user has "Show animations in Windows" on
/// ([`SPI_GETCLIENTAREAANIMATION`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-systemparametersinfow)).
/// Returns `true` if the query fails, so motion is the default.
pub fn system_animations_enabled() -> bool {
    let mut on = BOOL(1);
    // SAFETY: the buffer is a BOOL that lives across the call, and the action's documented
    // output is one BOOL. The pointer is only written to during the call.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(std::ptr::from_mut(&mut on).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_err() || on.as_bool()
}
