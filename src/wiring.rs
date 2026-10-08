//! The composition root: the one place that knows both the engine and the
//! Windows adapter crates, and plugs the latter into the former.
//!
//! Each phase adds its adapter here — and adds the crate to the app's
//! `[dependencies]` at the same time:
//!
//! - Phase 1 (done): `video` → `zc_video::GpuVideoSource`
//! - Phase 2 (done): `input` → `zc_input::RawInputSource`; `game_audio` →
//!   `zc_audio::GameAudioSource` (if enabled); `mic` → `zc_audio::MicSource`
//!   (unless the mic is off)
//! - Phase 3 (P3.24, P3.25): `seek_penalty` → `zc_win::drive::seek_penalty`, the drive probe
//!   that sets DVR dump pacing; `lock_memory` → `zc_win::memory::lock`, which pins the DVR rings
//!
//! Ports are built once at startup, so turning game audio or the mic on or off
//! applies at the next launch; push-to-talk ⇄ always-on is live.
//!
//! Tests don't come through here: they give the engine synthetic sources instead
//! (`zc_engine::synthetic`).

use zc_core::ports::{AudioSource, SourceError};
use zc_core::settings::{MicMode, Settings};
use zc_engine::Ports;

/// Builds the adapters for the current settings.
pub fn ports(settings: &Settings) -> Ports {
    let audio = &settings.audio;
    Ports {
        video: Some(Box::new(zc_video::GpuVideoSource::new())),
        input: Some(Box::new(zc_input::RawInputSource::new())),
        game_audio: audio
            .game_audio
            .then(|| Box::new(zc_audio::GameAudioSource::new()) as Box<dyn AudioSource>),
        mic: (audio.mic != MicMode::Off).then(|| {
            Box::new(zc_audio::MicSource::new(
                audio.mic == MicMode::AlwaysOn,
                audio.mic_compressor,
                audio.mic_device.clone(),
            )) as Box<dyn AudioSource>
        }),
        seek_penalty: Some(zc_win::drive::seek_penalty),
        lock_memory: Some(pin_memory),
    }
}

/// Pins a DVR ring in RAM with `zc_win::memory::lock` (P3.25). Dropping the guard unpins it.
fn pin_memory(region: &[u8]) -> Result<Box<dyn Send>, SourceError> {
    Ok(Box::new(zc_win::memory::lock(region)?))
}
