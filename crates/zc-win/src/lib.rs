//! # zc-win — shared Windows plumbing
//!
//! Small, boring, correct helpers the Windows adapter crates share:
//!
//! - [`com`]: per-thread COM/WinRT apartment guard.
//! - [`mf`]: Media Foundation startup/shutdown guard.
//! - [`clock`]: the QPC clock, in [`Hns`](zc_core::Hns).
//! - [`error`]: turning `windows::core::Error` into [`SourceError`](zc_core::ports::SourceError).
//! - [`message_thread`]: a thread with a message-only window and a message loop,
//!   which Raw Input and `SetWinEventHook` both need (DESIGN §8).
//! - [`drive`]: "is this path on a spinning disk?" (DESIGN §4).
//!
//! Every `unsafe` block must carry a `// SAFETY:` comment.

pub mod clock;
pub mod com;
pub mod drive;
pub mod error;
pub mod message_thread;
pub mod mf;
