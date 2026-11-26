//! # zc-audio — audio capture (DESIGN §6)
//!
//! Every track is timestamped from WASAPI's QPC position (same timeline as WGC's
//! `SystemRelativeTime`) and encoded to AAC with Media Foundation:
//!
//! - **Track 1, Master** — [`MasterMix`]: a live mix of the sources below, one mixer thread
//!   200 ms behind real time (D30). Players and Discord play only this first track.
//! - **Track 2, game audio** — [`GameAudioSource`]: WASAPI *process loopback* in
//!   include mode, capturing the focused game's process tree. That's simpler than
//!   excluding a list of apps, and the profile engine already knows the game.
//!   Needs Windows 11 (build 20348+); older systems fall back to whole-endpoint
//!   loopback of the default playback device, which records everything (Discord
//!   included) and goes silent when nothing plays.
//! - **Track 3, microphone** — [`MicSource`]: the default capture device, gated by
//!   push-to-talk (a closed gate writes silence so the track keeps its length),
//!   optionally through the dynamics compressor in `zc_core::dsp`.
//! - **Tracks 4 and up, per-app audio** — [`AppAudioSource`] (D26): one process-loopback
//!   client per chosen app (for example `Discord.exe`), each on its own track. An app that
//!   isn't running writes silence. Unlike the game track there is no whole-system fallback:
//!   before Windows 11 build 20348 the track stays silent rather than recording everything.
//!
//! Each source runs an event-driven capture loop on its own thread, registered with
//! MMCSS (`AvSetMmThreadCharacteristicsW("Pro Audio")`) so they don't glitch
//! under game load. WASAPI is used through the raw `windows` APIs rather than the
//! `wasapi` crate (DECISIONS), so the audio loop's no-allocation rule stays ours.
//!
//! The other direction is [`PcmPlayer`]: in-process WASAPI playback for the clip player and
//! editor preview, whose device clock paces the picture.

mod aac;
mod app;
mod capture;
mod game;
mod loopback;
mod mic;
mod mix;
mod render;

pub use app::AppAudioSource;
pub use game::GameAudioSource;
pub use mic::{MicDevice, MicSource, list_microphones};
pub use mix::MasterMix;
pub use render::{PCM_CHANNELS, PCM_RATE, PcmPlayer, PcmWriter};
