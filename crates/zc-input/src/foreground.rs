//! Foreground-window tracking for the profile engine.

use std::cell::RefCell;

use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    IsZoomed, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};
use windows::core::PWSTR;
use zc_core::ports::{OsEvent, SourceError};
use zc_win::error::platform_error;

type Emitter = Box<dyn FnMut(OsEvent)>;

thread_local! {
    // The WinEvent callback has no user-data pointer, so it finds the emitter here.
    // It only runs on the thread that installed the hook.
    static EMIT: RefCell<Option<Emitter>> = const { RefCell::new(None) };
}

fn report(hwnd: HWND) {
    if let Some((pid, exe)) = process_for_window(hwnd) {
        let fullscreen = covers_its_monitor(hwnd);
        EMIT.with(|cell| {
            // try_borrow: a panic must never unwind out of the extern callback.
            if let Ok(mut emit) = cell.try_borrow_mut()
                && let Some(emit) = emit.as_mut()
            {
                emit(OsEvent::Foreground { pid, hwnd: hwnd.0 as isize, exe, fullscreen });
            }
        });
    }
}

/// Whether the window covers its whole monitor: fullscreen, or borderless on it. A
/// maximised window doesn't count, even when it overshoots the monitor by its frame (no taskbar
/// on that monitor, or an auto-hide taskbar): it is checked with
/// [IsZoomed](https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-iszoomed) first.
/// Otherwise compares [GetWindowRect](https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getwindowrect)
/// with the monitor's `rcMonitor` from [MONITORINFO](https://learn.microsoft.com/windows/win32/api/winuser/ns-winuser-monitorinfo).
/// `false` when either can't be read (the window closed already).
#[must_use]
pub fn covers_its_monitor(hwnd: HWND) -> bool {
    // SAFETY: plain FFI call; a bad handle gives FALSE.
    if unsafe { IsZoomed(hwnd) }.as_bool() {
        return false;
    }
    let mut window = RECT::default();
    // SAFETY: `window` is a valid out-parameter for the call; it is only read on success.
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_err() {
        return false;
    }
    // SAFETY: plain FFI call; for any window it returns the nearest monitor, and a bad handle
    // makes `GetMonitorInfoW` below fail instead.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
        ..MONITORINFO::default()
    };
    // SAFETY: `info` is a valid out-parameter, and its `cbSize` is set as the API requires.
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return false;
    }
    let m = info.rcMonitor;
    window.left <= m.left
        && window.top <= m.top
        && window.right >= m.right
        && window.bottom >= m.bottom
}

unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND && id_object == OBJID_WINDOW.0 {
        report(hwnd);
    }
}

/// Installs the foreground hook on the current (message) thread
/// ([SetWinEventHook](https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setwineventhook)),
/// and reports the window that has focus right now.
///
/// Out-of-context, so nothing is injected into other processes, and our own
/// windows are skipped. The hook goes away with the thread that installed it, so
/// stopping the message thread is enough.
///
/// # Errors
/// The hook couldn't be installed.
pub fn install(emit: impl FnMut(OsEvent) + 'static) -> Result<(), SourceError> {
    EMIT.with(|cell| *cell.borrow_mut() = Some(Box::new(emit)));
    // SAFETY: a valid callback with the WINEVENTPROC signature; out-of-context, so
    // it runs on this thread's message loop.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.is_invalid() {
        return Err(platform_error("SetWinEventHook", &windows::core::Error::from_thread()));
    }
    // SAFETY: plain FFI calls.
    let (current, own_pid) = unsafe { (GetForegroundWindow(), GetCurrentProcessId()) };
    // At start-up ZeroClip itself usually has focus; that isn't a game.
    if process_for_window(current).is_some_and(|(pid, _)| pid != own_pid) {
        report(current);
    }
    Ok(())
}

/// The process behind a window: its id and full executable path. `None` for
/// windows we can't inspect (elevated processes, or it closed already).
#[must_use]
pub fn process_for_window(hwnd: HWND) -> Option<(u32, String)> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out-parameter; an invalid window gives 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    // SAFETY: plain FFI calls; the handle is closed on every path.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        ok.ok()?;
        Some((pid, String::from_utf16_lossy(&buf[..len as usize])))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use zc_win::message_thread::MessageThread;

    use super::*;

    #[test]
    fn process_for_window_finds_our_own_exe() {
        let (tx, rx) = mpsc::channel();
        let _thread = MessageThread::spawn(
            "test-fg",
            move |hwnd| {
                let _ = tx.send(hwnd.0 as isize);
                Ok(())
            },
            |_| {},
        )
        .expect("spawn");
        let hwnd = HWND(rx.recv().expect("hwnd") as *mut std::ffi::c_void);
        let (pid, exe) = process_for_window(hwnd).expect("our own window");
        assert_eq!(pid, std::process::id());
        assert!(exe.to_ascii_lowercase().ends_with(".exe"), "{exe}");
    }
}
