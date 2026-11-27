//! # zc-input — hotkeys and foreground tracking
//!
//! One [`MessageThread`](zc_win::message_thread::MessageThread) hosts both:
//!
//! - **Raw Input** ([`raw_input`]): keyboard and mouse registered with
//!   `RIDEV_INPUTSINK`, so `WM_INPUT` arrives even while a game has focus. Raw
//!   Input is how the OS itself delivers input — no hooks, nothing injected, so it
//!   is invisible to anti-cheat and has no `SetWindowsHookEx` latency.
//! - **Foreground tracking** ([`foreground`]): `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`
//!   notifies on focus changes — event-driven, nothing polled.
//!
//! Both are turned into [`OsEvent`](zc_core::ports::OsEvent)s for the engine,
//! where the hotkey matcher and the profile engine live.
//!
//! - **Process watch** ([`process_watch`]): one parked thread per watched game reports its
//!   exit code, so the engine can tell a crash from a clean quit.

pub mod foreground;
pub mod process_watch;
pub mod raw_input;
mod source;

pub use source::RawInputSource;
