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

use std::sync::{Arc, Mutex};

use zc_core::mix::{MixBus, MixInput, db_to_gain};
use zc_core::ports::{AudioSource, SourceError};
use zc_core::settings::{MicMode, Settings};
use zc_engine::Ports;

/// Builds the adapters for the current settings.
///
/// The enabled sources share one Master mix bus (D30): the game is input 0 when it is on, and
/// the microphone follows it. Each source's level in the mix is set here, so it applies from the
/// next start.
pub fn ports(settings: &Settings) -> Ports {
    let audio = &settings.audio;
    let mic_on = audio.mic != MicMode::Off;
    let game_input = audio.game_audio.then_some(0);
    let mic_input = mic_on.then(|| usize::from(audio.game_audio));
    let inputs = usize::from(audio.game_audio) + usize::from(mic_on);
    let bus = (inputs > 0).then(|| {
        let mut bus = MixBus::new(inputs, 2);
        if let Some(input) = game_input {
            bus.set_gain(input, db_to_gain(audio.mix_gain_db.game));
        }
        if let Some(input) = mic_input {
            bus.set_gain(input, db_to_gain(audio.mix_gain_db.mic));
        }
        Arc::new(Mutex::new(bus))
    });
    let mix_for = |input: Option<usize>| {
        input.and_then(|input| bus.as_ref().map(|bus| MixInput::new(Arc::clone(bus), input)))
    };
    Ports {
        video: Some(Box::new(zc_video::GpuVideoSource::new())),
        input: Some(Box::new(zc_input::RawInputSource::new())),
        game_audio: audio.game_audio.then(|| {
            Box::new(zc_audio::GameAudioSource::new().with_mix(mix_for(game_input)))
                as Box<dyn AudioSource>
        }),
        mic: mic_on.then(|| {
            Box::new(
                zc_audio::MicSource::new(
                    audio.mic == MicMode::AlwaysOn,
                    audio.mic_compressor,
                    audio.mic_device.clone(),
                )
                .with_mix(mix_for(mic_input)),
            ) as Box<dyn AudioSource>
        }),
        master_audio: bus
            .map(|bus| Box::new(zc_audio::MasterMix::new(bus)) as Box<dyn AudioSource>),
        seek_penalty: Some(zc_win::drive::seek_penalty),
        lock_memory: Some(pin_memory),
    }
}

/// Pins a DVR ring in RAM with `zc_win::memory::lock` (P3.25). Dropping the guard unpins it.
fn pin_memory(region: &[u8]) -> Result<Box<dyn Send>, SourceError> {
    Ok(Box::new(zc_win::memory::lock(region)?))
}
