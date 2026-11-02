use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use zc_core::Hns;
use zc_core::media::TrackId;
use zc_core::mix::MixInput;
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;
use zc_win::error::WinResultExt;

use crate::capture::{Mmcss, Stream, Worker, pcm16_format};
use crate::loopback;

/// Track 1: the focused game's audio via WASAPI *process loopback*, include mode
/// (D9): only the target process and its children are captured, so Discord, music
/// and notifications stay out.
///
/// Activation goes through `ActivateAudioInterfaceAsync` with
/// [`AUDIOCLIENT_ACTIVATION_PARAMS`](https://learn.microsoft.com/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_activation_params)
/// (see Microsoft's ApplicationLoopback sample). Needs Windows 11 (build 20348+).
/// Retargeting is live: the capture thread notices a new pid within 100 ms. The track has
/// no holes (D27): while the game is quiet, or nothing is targeted, the thread writes silence,
/// so the track runs from the moment capture starts.
#[derive(Debug, Default)]
pub struct GameAudioSource {
    /// The target pid; 0 means none (pid 0 is the Idle process, never a game).
    target: Arc<AtomicU32>,
    /// This source's input in the Master mix (D30), if it has one.
    mix: Option<MixInput>,
    worker: Option<Worker>,
}

impl GameAudioSource {
    /// A stopped source with no target yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts this source in the Master mix as `mix`. Set it before `start`. `None` keeps the
    /// source out of the mix.
    #[must_use]
    pub fn with_mix(mut self, mix: Option<MixInput>) -> Self {
        self.mix = mix;
        self
    }

    /// The process tree being captured.
    #[must_use]
    pub fn target_pid(&self) -> Option<u32> {
        Some(self.target.load(Ordering::Relaxed)).filter(|&p| p != 0)
    }
}

/// Whole-system loopback of the default playback device: the fallback before
/// Windows 11 build 20348 (D9). It records everything, Discord included, and
/// delivers nothing while nothing plays; the capture loop pads those gaps with
/// silence (`Encode::pad_silence`).
fn open_endpoint_loopback(
    format: &windows::Win32::Media::Audio::WAVEFORMATEX,
) -> Result<Stream, SourceError> {
    // SAFETY: plain COM calls on this thread's apartment.
    let client: IAudioClient = unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("MMDeviceEnumerator")?;
        let device = devices
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("default playback device")?;
        device.Activate(CLSCTX_ALL, None).context("IMMDevice::Activate")?
    };
    Stream::start(client, true, format)
}

impl AudioSource for GameAudioSource {
    fn track(&self) -> TrackId {
        TrackId::GameAudio
    }

    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        _health: Emit<HealthEvent>,
    ) -> Result<(), SourceError> {
        self.stop();
        let (config, target, mix) = (*config, Arc::clone(&self.target), self.mix.clone());
        self.worker = Some(Worker::spawn("zc-game-audio", move |stop, ready| {
            let (_apartment, _mf, mut encode) =
                match loopback::setup(TrackId::GameAudio, &config, mix) {
                    Ok(parts) => {
                        ready(Ok(()));
                        parts
                    }
                    Err(e) => return ready(Err(e)),
                };
            let _mmcss = Mmcss::join();
            let format = pcm16_format(config.sample_rate, config.channels);
            let (mut current, mut stream) = (0u32, None::<Stream>);
            // Set once process loopback turns out to be unsupported (pre-20348).
            let mut endpoint = false;
            // Endpoint mode retries the playback device once a second, not on every loop.
            let mut retry_at = Hns::ZERO;
            while !stop.load(Ordering::Acquire) {
                let want = target.load(Ordering::Relaxed);
                if endpoint {
                    // Targets don't matter any more; (re)open after a device change.
                    let now = zc_win::clock::now();
                    if stream.is_none() && now >= retry_at {
                        stream = open_endpoint_loopback(&format)
                            .inspect_err(|e| tracing::debug!(%e, "playback device unavailable"))
                            .ok();
                        retry_at = now + Hns::from_secs(1);
                    }
                } else if want != current {
                    // A retarget leaves a gap, which the silence below fills.
                    stream = None;
                    current = want;
                    if want != 0 {
                        match loopback::activate(want).and_then(|c| Stream::start(c, true, &format))
                        {
                            Ok(s) => {
                                tracing::info!(pid = want, "capturing game audio");
                                stream = Some(s);
                            }
                            Err(SourceError::Unsupported(e)) => {
                                tracing::info!(%e, "falling back to whole-system loopback");
                                endpoint = true;
                            }
                            Err(e) => {
                                tracing::warn!(pid = want, %e, "can't capture this process's audio")
                            }
                        }
                    }
                }
                // Before any read: silence up to 100 ms ago, so a buffer after a gap (no target,
                // a retarget, a failed activation, the playback device away) lands after it.
                if let Err(e) = encode.pad_silence(zc_win::clock::now(), &*sink) {
                    tracing::warn!(%e, "game audio silence failed");
                }
                match &stream {
                    Some(s) => {
                        if let Err(e) =
                            s.read(|pcm, frames, qpc| encode.push(pcm, frames, qpc, &*sink))
                        {
                            tracing::warn!(%e, "game audio capture stopped");
                            stream = None;
                        }
                    }
                    None => std::thread::sleep(Duration::from_millis(100)),
                }
            }
            encode.finish(&*sink);
        })?);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }

    fn set_target_process(&mut self, pid: Option<u32>) {
        self.target.store(pid.unwrap_or(0), Ordering::Relaxed);
    }
}
