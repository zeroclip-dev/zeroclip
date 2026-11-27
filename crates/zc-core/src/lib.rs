//! # zc-core — ZeroClip's domain model and platform-independent logic
//!
//! Everything in this crate is pure Rust with no Windows dependency, so it builds
//! and tests on any OS in seconds. The Windows adapter crates (`zc-video`,
//! `zc-audio`, `zc-input`) and the engine (`zc-engine`) are built on top of it.
//!
//! | Module | What it owns |
//! |---|---|
//! | [`time`] | [`Hns`] (100 ns ticks), the [`FramePacer`](time::FramePacer) and the [`MasterClock`](time::MasterClock) |
//! | [`media`] | codecs and the capability ladder, tracks, encoded packets |
//! | [`mix`] | the Master mix: one sample timeline with a ring per source, mixed 200 ms behind |
//! | [`sink`] | how packets flow: [`PacketSink`](sink::PacketSink), [`Tee`](sink::Tee) |
//! | [`dvr`] | the GOP-aware retrospective ring buffer |
//! | [`ring`] | allocation-free packet FIFO in one byte ring, for the DVR buffer and the recording spool |
//! | [`watchdog`] | the encoder watchdog state machine |
//! | [`profile`] | per-game profiles and RAM clamping |
//! | [`focus_prompt`] | what a focus change means while a game is tracked |
//! | [`hotkey`] | keys, chords and the chord matcher |
//! | [`backoff`] | retry schedule for HDCP-protected content |
//! | [`dsp`] | the mic dynamics compressor |
//! | [`ports`] | the traits Windows adapters implement |
//! | [`control`] | commands to, and events from, the engine |
//! | [`settings`] | user settings (serde) |
//!
//! Behaviour that is designed but not built yet is pinned down by tests marked
//! `#[ignore = "spec: …"]`. Run `cargo test -p zc-core -- --ignored` to see what is
//! left; see `CONTRIBUTING.md` for the workflow.

pub mod backoff;
pub mod bookmarks;
pub mod control;
pub mod dsp;
pub mod dvr;
pub mod focus_prompt;
pub mod fs_name;
pub mod game_tag;
pub mod hotkey;
pub mod media;
pub mod mix;
pub mod ports;
pub mod profile;
pub mod ring;
pub mod settings;
pub mod sink;
pub mod testing;
pub mod time;
pub mod watchdog;

pub use media::{Codec, Packet, PacketMeta, PacketRef, TrackId};
pub use time::Hns;
