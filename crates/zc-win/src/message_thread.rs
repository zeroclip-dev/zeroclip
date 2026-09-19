//! A dedicated thread with a message-only window and a Win32 message loop.
//!
//! Raw Input delivers `WM_INPUT` to a window, and `SetWinEventHook` with
//! `WINEVENT_OUTOFCONTEXT` calls back on the thread that installed it — both need
//! a running message loop. Don't rely on the egui window for that: it sleeps when
//! backgrounded, which is exactly when a game has focus (DESIGN §8). This thread
//! owns a message-only window (`HWND_MESSAGE`) whose only job is to pump messages.

use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use zc_core::ports::SourceError;

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
    #[expect(dead_code, reason = "scaffold: read by stop() once implemented (Phase 2)")]
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl MessageThread {
    /// Spawns the thread. On it: registers a window class, creates a
    /// message-only window, runs `setup` with the window handle (register Raw
    /// Input devices, install WinEvent hooks), then pumps messages, passing each
    /// one to `handler` until [`stop`](Self::stop).
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
        let _ = (name, setup, handler);
        todo!(
            "Phase 2: std::thread::Builder → RegisterClassExW → CreateWindowExW(.., HWND_MESSAGE, ..) \
             → setup(hwnd) → report back over a channel → GetMessageW / DispatchMessageW loop; \
             record GetCurrentThreadId() for stop()"
        )
    }

    /// Posts `WM_QUIT` to the thread and joins it. Idempotent.
    pub fn stop(&mut self) {
        if self.handle.is_some() {
            todo!("Phase 2: PostThreadMessageW(thread_id, WM_QUIT, ..), then join")
        }
    }
}

impl Drop for MessageThread {
    fn drop(&mut self) {
        self.stop();
    }
}
