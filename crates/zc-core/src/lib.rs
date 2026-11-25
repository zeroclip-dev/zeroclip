//! # zc-core — ZeroClip's domain model and platform-independent logic
//!
//! Everything in this crate is pure Rust with no Windows dependency, so it builds
//! and tests on any OS in seconds. The Windows adapter crates (`zc-video`,
//! `zc-audio`, `zc-input`) and the engine (`zc-engine`) are built on top of it.
//!
//! | Module | What it owns | Design doc |
//! |---|---|---|
//! | [`time`] | [`Hns`] (100 ns ticks), the [`FramePacer`](time::FramePacer) and the [`MasterClock`](time::MasterClock) | §3, §6 |
//! | [`media`] | codecs and the capability ladder, tracks, encoded packets | §3 |
//! | [`mix`] | the Master mix: one sample timeline with a ring per source, mixed 200 ms behind | §6 |
//! | [`sink`] | how packets flow: [`PacketSink`](sink::PacketSink), [`Tee`](sink::Tee) | §5 |
//! | [`dvr`] | the GOP-aware retrospective ring buffer | §4, §6 |
//! | [`ring`] | allocation-free packet FIFO in one byte ring, for the DVR buffer and the recording spool | §4, §5 |
//! | [`watchdog`] | the encoder watchdog state machine | §8 |
//! | [`profile`] | per-game profiles and RAM clamping | §7 |
//! | [`focus_prompt`] | what a focus change means while a game is tracked (D35) | §7 |
//! | [`hotkey`] | keys, chords and the chord matcher | §8 |
//! | [`backoff`] | retry schedule for HDCP-protected content | §3 |
//! | [`dsp`] | the mic dynamics compressor | §6 |
//! | [`ports`] | the traits Windows adapters implement | — |
//! | [`control`] | commands to, and events from, the engine | — |
//! | [`settings`] | user settings (serde) | — |
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
