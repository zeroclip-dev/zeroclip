//! # zc-engine — the recording engine
//!
//! The engine is a single actor thread. The UI, the input thread and the
//! pipeline's health signals all post messages to its inbox; it owns every piece
//! of mutable state (settings, profile, watchdog, recording modes) and answers
//! with [`EngineEvent`](zc_core::control::EngineEvent)s.
//!
//! ```text
//!  UI ──Command──┐
//!  input thread ─┼─► inbox ─► engine thread ─► EngineEvent ─► UI
//!  health ───────┘                │
//!                                 ├─ starts/stops the sources (Ports)
//!                                 └─ owns the Recorder ◄── packets from sources
//! ```
//!
//! Packets do **not** go through the inbox: sources push them straight into the
//! [`Recorder`](recorder::Recorder), which copies them into the DVR buffer and any
//! active disk writers under a short lock. The inbox carries control traffic only.
//!
//! The engine never depends on Windows. Real adapters are injected by the app;
//! tests inject [`synthetic`] sources, which is how the watchdog and recording
//! modes get tested deterministically (DESIGN §12, "synthetic-load test harness").

mod actor;
mod output;
pub mod recorder;
pub mod spool;
pub mod synthetic;

pub use actor::{Engine, EngineConfig, EngineError, EngineHandle, Ports};
