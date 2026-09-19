//! # zc-input — hotkeys and foreground tracking (DESIGN §7, §8)
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

pub mod foreground;
pub mod raw_input;
mod source;

pub use source::RawInputSource;
