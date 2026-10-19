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
//! - Per-app audio (P2.24, D26): each enabled app track → `zc_audio::AppAudioSource`, on track
//!   `3 + i` for the i-th enabled app, found by its executable's process tree
//!
//! Ports are built once at startup, so turning game audio, the mic or an app track on or off
//! applies at the next launch; push-to-talk ⇄ always-on is live.
//!
//! Tests don't come through here: they give the engine synthetic sources instead
//! (`zc_engine::synthetic`).

use std::sync::{Arc, Mutex};

use zc_core::media::TrackId;
use zc_core::mix::{MixBus, MixInput, db_to_gain};
use zc_core::ports::{AudioSource, SourceError};
use zc_core::settings::{AppTrack, MicMode, Settings};
use zc_engine::Ports;

/// Builds the adapters for the current settings.
///
/// The enabled sources share one Master mix bus (D30): the game is input 0 when it is on, the
/// microphone follows it, and the app tracks come after both. Each source's level in the mix is
/// set here, so it applies from the next start.
pub fn ports(settings: &Settings) -> Ports {
    let audio = &settings.audio;
    let mic_on = audio.mic != MicMode::Off;
    let game_input = audio.game_audio.then_some(0);
    let mic_input = mic_on.then(|| usize::from(audio.game_audio));
    let app_base = usize::from(audio.game_audio) + usize::from(mic_on);
    // The i-th enabled app track records on audio slot 3 + i; `TrackId::audio` rejects a slot
    // past the limit, so a bad index drops the track instead of panicking.
    let apps: Vec<(TrackId, &AppTrack)> = audio
        .app_tracks
        .iter()
        .filter(|app| app.enabled)
        .enumerate()
        .filter_map(|(i, app)| Some((u8::try_from(3 + i).ok().and_then(TrackId::audio)?, app)))
        .collect();
    let inputs = app_base + apps.len();
    let bus = (inputs > 0).then(|| {
        let mut bus = MixBus::new(inputs, 2);
        if let Some(input) = game_input {
            bus.set_gain(input, db_to_gain(audio.mix_gain_db.game));
        }
        if let Some(input) = mic_input {
            bus.set_gain(input, db_to_gain(audio.mix_gain_db.mic));
        }
        for input in app_base..inputs {
            bus.set_gain(input, db_to_gain(audio.mix_gain_db.game));
        }
        Arc::new(Mutex::new(bus))
    });
    let mix_for = |input: Option<usize>| {
        input.and_then(|input| bus.as_ref().map(|bus| MixInput::new(Arc::clone(bus), input)))
    };
    let app_audio = apps
        .into_iter()
        .enumerate()
        .map(|(i, (track, app))| {
            let resolve = Box::new(|exe: &str| zc_win::process::root_pid_for_exe(exe));
            let source = zc_audio::AppAudioSource::new(track, app.exe.clone(), resolve)
                .with_mix(mix_for(Some(app_base + i)));
            (app.label.clone(), Box::new(source) as Box<dyn AudioSource>)
        })
        .collect();
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
        app_audio,
        master_audio: bus
            .map(|bus| Box::new(zc_audio::MasterMix::new(bus)) as Box<dyn AudioSource>),
        seek_penalty: Some(zc_win::drive::seek_penalty),
        lock_memory: Some(pin_memory),
        total_memory: Some(zc_win::memory::total_physical_mib),
    }
}

/// Pins a DVR ring in RAM with `zc_win::memory::lock` (P3.25). Dropping the guard unpins it.
fn pin_memory(region: &[u8]) -> Result<Box<dyn Send>, SourceError> {
    Ok(Box::new(zc_win::memory::lock(region)?))
}
