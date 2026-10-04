//! The composition root: the one place that knows both the engine and the
//! Windows adapter crates, and plugs the latter into the former.
//!
//! Each phase adds its adapter here — and adds the crate to the app's
//! `[dependencies]` at the same time:
//!
//! - Phase 1 (done): `video` → `zc_video::GpuVideoSource`
//! - Phase 2: `game_audio` → `zc_audio::GameAudioSource::new()` (if
//!   `settings.audio.game_audio`), `mic` → `zc_audio::MicSource::new(..)` (unless
//!   the mic is off), `input` → `zc_input::RawInputSource::new()`
//!
//! Tests don't come through here: they give the engine synthetic sources instead
//! (`zc_engine::synthetic`).

use zc_core::settings::Settings;
use zc_engine::Ports;

/// Builds the adapters for the current settings.
pub fn ports(settings: &Settings) -> Ports {
    let _ = settings; // Phase 2 picks audio sources from it.
    Ports { video: Some(Box::new(zc_video::GpuVideoSource::new())), ..Ports::default() }
}
