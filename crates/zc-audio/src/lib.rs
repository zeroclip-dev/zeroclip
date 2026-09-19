//! # zc-audio — audio capture (DESIGN §6)
//!
//! Two tracks, both timestamped from WASAPI's QPC position (same timeline as
//! WGC's `SystemRelativeTime`) and encoded to AAC with Media Foundation:
//!
//! - **Track 1, game audio** — [`GameAudioSource`]: WASAPI *process loopback* in
//!   include mode, capturing the focused game's process tree. That's simpler than
//!   excluding a list of apps, and the profile engine already knows the game.
//!   Needs Windows 11 (build 20348+); older systems fall back to whole-endpoint
//!   loopback.
//! - **Track 2, microphone** — [`MicSource`]: the default capture device, gated by
//!   push-to-talk (a closed gate writes silence so the track keeps its length),
//!   optionally through the dynamics compressor in `zc_core::dsp`.
//!
//! Both run an event-driven capture loop on their own thread, registered with
//! MMCSS (`AvSetMmThreadCharacteristicsW("Pro Audio")`) so they don't glitch
//! under game load. The `wasapi` crate (DESIGN §2) wraps much of the WASAPI side;
//! using it or the raw `windows` APIs is a Phase 2 call.

mod aac;
mod game;
mod mic;

pub use game::GameAudioSource;
pub use mic::MicSource;
