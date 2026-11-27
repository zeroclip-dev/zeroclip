//! A dedicated thread with a message-only window and a Win32 message loop.
//!
//! Raw Input delivers `WM_INPUT` to a window, and `SetWinEventHook` with
//! `WINEVENT_OUTOFCONTEXT` calls back on the thread that installed it — both need
//! a running message loop. Don't rely on the egui window for that: it sleeps when
//! backgrounded, which is exactly when a game has focus. This thread
//! owns a message-only window (`HWND_MESSAGE`) whose only job is to pump messages.

use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    PostThreadMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_QUIT,
};
use windows::core::w;
use zc_core::ports::SourceError;

use crate::error::WinResultExt;

/// A message delivered to the message-only window.
#[derive(Clone, Copy, Debug)]
pub struct WindowMessage {
    /// The message-only window.
    pub hwnd: HWND,
    /// Message id (`WM_INPUT`, …).
    pub msg: u32,
    /// First parameter.
    pub wparam: WPARAM,
    /// Second parameter.
    pub lparam: LPARAM,
}

/// A running message thread. Dropping it stops the thread.
#[derive(Debug)]
pub struct MessageThread {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl MessageThread {
    /// Spawns the thread. On it: creates a message-only window, runs `setup` with
    /// the window handle (register Raw Input devices, install WinEvent hooks), then
    /// pumps messages, passing each one to `handler` until [`stop`](Self::stop).
    ///
    /// The window uses the built-in `STATIC` class, so no class registration is
    /// needed; its window procedure hands unknown messages to `DefWindowProcW`,
    /// which is what `WM_INPUT` cleanup needs
    /// ([message-only windows](https://learn.microsoft.com/windows/win32/winmsg/window-features#message-only-windows)).
    ///
    /// Returns once `setup` has finished, with its error if it failed.
    ///
    /// # Errors
    /// Window creation or `setup` failures.
    pub fn spawn<S, H>(name: &str, setup: S, handler: H) -> Result<Self, SourceError>
    where
        S: FnOnce(HWND) -> Result<(), SourceError> + Send + 'static,
        H: FnMut(&WindowMessage) + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel::<Result<u32, SourceError>>(1);
        let mut handler = handler;
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // SAFETY: plain FFI call; a message-only window needs no parent,
                // menu or instance.
                let hwnd = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("STATIC"),
                        w!(""),
                        WINDOW_STYLE(0),
                        0,
                        0,
                        0,
                        0,
                        Some(HWND_MESSAGE),
                        None,
                        None,
                        None,
                    )
                };
                let hwnd = match hwnd.context("CreateWindowExW (message-only)") {
                    Ok(hwnd) => hwnd,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = setup(hwnd) {
                    // SAFETY: the window was created on this thread just above.
                    let _ = unsafe { DestroyWindow(hwnd) };
                    let _ = tx.send(Err(e));
                    return;
                }
                // SAFETY: plain FFI call. The queue already exists (CreateWindowExW
                // made it), so a WM_QUIT posted after this can't be lost.
                let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
                let mut msg = MSG::default();
                // SAFETY: `msg` is valid for each call. GetMessageW returns 0 for
                // WM_QUIT and -1 on error; both end the loop.
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                    handler(&WindowMessage {
                        hwnd: msg.hwnd,
                        msg: msg.message,
                        wparam: msg.wParam,
                        lparam: msg.lParam,
                    });
                    // SAFETY: `msg` came from GetMessageW. Dispatching after the
                    // handler lets DefWindowProc clean up after WM_INPUT.
                    unsafe { DispatchMessageW(&msg) };
                }
                // SAFETY: the window was created on this thread.
                let _ = unsafe { DestroyWindow(hwnd) };
            })
            .map_err(|e| SourceError::Other(format!("couldn't start the {name} thread: {e}")))?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self { thread_id, handle: Some(handle) }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(SourceError::Other(format!("the {name} thread exited during start-up")))
            }
        }
    }

    /// Posts `WM_QUIT` to the thread and joins it. Idempotent.
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: plain FFI call; a stale thread id just makes it fail.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            // A panic there was already logged by the app's panic hook.
            let _ = handle.join();
        }
    }
}

impl Drop for MessageThread {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

    use super::*;

    #[test]
    fn message_thread_reports_setup_errors() {
        let result = MessageThread::spawn(
            "test-msg",
            |_| Err(SourceError::Other("setup failed".into())),
            |_| {},
        );
        assert!(matches!(result, Err(SourceError::Other(m)) if m == "setup failed"));
    }

    #[test]
    fn message_thread_delivers_posted_messages_and_stops() {
        let (hwnd_tx, hwnd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let thread = MessageThread::spawn(
            "test-msg",
            move |hwnd| {
                // HWND isn't Send; pass the raw value.
                let _ = hwnd_tx.send(hwnd.0 as isize);
                Ok(())
            },
            move |m| {
                if m.msg == WM_APP + 1 {
                    let _ = msg_tx.send(m.msg);
                }
            },
        )
        .expect("spawn");
        let hwnd = HWND(hwnd_rx.recv().expect("hwnd") as *mut std::ffi::c_void);
        // SAFETY: posting to a live window owned by the test's message thread.
        unsafe { PostMessageW(Some(hwnd), WM_APP + 1, WPARAM(0), LPARAM(0)) }.expect("post");
        let got = msg_rx.recv_timeout(Duration::from_secs(5)).expect("message delivered");
        assert_eq!(got, WM_APP + 1);
        drop(thread); // must not hang
    }

    #[test]
    fn message_thread_can_be_spawned_twice() {
        let a = MessageThread::spawn("test-a", |_| Ok(()), |_| {}).expect("first");
        let b = MessageThread::spawn("test-b", |_| Ok(()), |_| {}).expect("second");
        drop((a, b));
    }
}
