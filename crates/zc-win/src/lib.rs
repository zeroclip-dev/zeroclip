//! # zc-win — shared Windows plumbing
//!
//! Small, boring, correct helpers the Windows adapter crates share:
//!
//! - [`com`]: per-thread COM/WinRT apartment guard.
//! - [`mf`]: Media Foundation startup/shutdown guard.
//! - [`clock`]: the QPC clock, in [`Hns`](zc_core::Hns).
//! - [`error`]: turning `windows::core::Error` into [`SourceError`](zc_core::ports::SourceError).
//! - [`message_thread`]: a thread with a message-only window and a message loop,
//!   which Raw Input and `SetWinEventHook` both need.
//! - [`drive`]: "is this path on a spinning disk?".
//! - [`memory`]: page-locking the DVR buffer, so Windows can't page it out.
//! - [`process`]: open windows and process trees, for per-app audio.
//! - [`single_instance`]: a named mutex, so two copies can't share the DVR file.
//! - [`overlay`]: click-through, capture-excluded toast windows.
//!
//! Every `unsafe` block must carry a `// SAFETY:` comment.

pub mod autostart;
pub mod chime;
pub mod clock;
pub mod com;
pub mod drag;
pub mod drive;
pub mod error;
pub mod icon;
pub mod main_window;
pub mod memory;
pub mod message_thread;
pub mod mf;
pub mod notify;
pub mod overlay;
pub mod process;
pub mod shell;
pub mod single_instance;
pub mod toast;
