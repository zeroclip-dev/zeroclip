use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
    eCommunications,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use zc_core::dsp::{Compressor, CompressorParams};
use zc_core::media::TrackId;
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;
use zc_win::com::Apartment;
use zc_win::error::WinResultExt;
use zc_win::mf::MediaFoundation;

use crate::capture::{Encode, Mmcss, Stream, Worker, pcm16_format};

/// Track 2: the microphone, gated by push-to-talk.
///
/// The default *communications* capture device (the headset mic, usually), in
/// shared mode. A closed gate writes silence, so the track keeps its length and
/// timestamps (DESIGN §6). The optional compressor has no lookahead, so the Mic
/// track needs no offset (`Compressor::latency`, `MasterClock::set_track_offset`).
/// If the mic is unplugged or the default device changes, capture reopens every
/// second; the gap becomes silence.
#[derive(Debug)]
pub struct MicSource {
    /// Shared with the capture thread; flipped by the push-to-talk hotkey.
    gate: Arc<AtomicBool>,
    compressor: bool,
    worker: Option<Worker>,
}

impl MicSource {
    /// A stopped mic source. `always_on` opens the gate permanently (no PTT).
    #[must_use]
    pub fn new(always_on: bool, compressor: bool) -> Self {
        Self { gate: Arc::new(AtomicBool::new(always_on)), compressor, worker: None }
    }

    /// Whether the dynamics compressor is enabled.
    #[must_use]
    pub fn compressor(&self) -> bool {
        self.compressor
    }

    /// Whether the gate is open right now.
    #[must_use]
    pub fn gate_open(&self) -> bool {
        self.gate.load(Ordering::Relaxed)
    }
}

fn open_mic(config: &AudioStart) -> Result<Stream, SourceError> {
    // SAFETY: plain COM calls on this thread's apartment.
    let client: IAudioClient = unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("MMDeviceEnumerator")?;
        let device = devices
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .map_err(|_| SourceError::Unsupported("no microphone".into()))?;
        device.Activate(CLSCTX_ALL, None).context("IMMDevice::Activate")?
    };
    Stream::start(client, false, &pcm16_format(config.sample_rate, config.channels))
}

impl AudioSource for MicSource {
    fn track(&self) -> TrackId {
        TrackId::Mic
    }

    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        _health: Emit<HealthEvent>,
    ) -> Result<(), SourceError> {
        self.stop();
        let (config, gate, use_compressor) = (*config, Arc::clone(&self.gate), self.compressor);
        self.worker = Some(Worker::spawn("zc-mic", move |stop, ready| {
            let setup = (|| {
                let apartment = Apartment::multithreaded()?;
                let mf = MediaFoundation::start()?;
                let encode = Encode::new(TrackId::Mic, &config)?;
                let stream = open_mic(&config)?;
                Ok((apartment, mf, encode, stream))
            })();
            let (_apartment, _mf, mut encode, stream) = match setup {
                Ok(parts) => {
                    ready(Ok(()));
                    tracing::info!("microphone capturing");
                    parts
                }
                Err(e) => return ready(Err(e)),
            };
            let _mmcss = Mmcss::join();
            let ch = usize::from(config.channels);
            // A second of scratch space, allocated once: WASAPI buffers are far smaller.
            let mut pcm = vec![0i16; config.sample_rate as usize * ch];
            let mut floats = vec![0f32; pcm.len()];
            let mut compressor = use_compressor.then(|| {
                Compressor::new(CompressorParams {
                    sample_rate: config.sample_rate,
                    channels: config.channels,
                    ..CompressorParams::default()
                })
            });
            let mut stream = Some(stream);
            while !stop.load(Ordering::Acquire) {
                let Some(s) = &stream else {
                    std::thread::sleep(Duration::from_secs(1));
                    stream = open_mic(&config)
                        .inspect_err(|e| tracing::debug!(%e, "mic still unavailable"))
                        .ok();
                    if stream.is_some() {
                        tracing::info!("microphone capturing again");
                    }
                    continue;
                };
                let result = s.read(|data, frames, qpc| {
                    let n = (frames as usize * ch).min(pcm.len());
                    let buf = &mut pcm[..n];
                    match data {
                        Some(data) if gate.load(Ordering::Relaxed) => {
                            buf.copy_from_slice(&data[..n])
                        }
                        // A closed gate (or silence) writes zeros: same length, same timestamps.
                        _ => buf.fill(0),
                    }
                    if gate.load(Ordering::Relaxed)
                        && let Some(c) = &mut compressor
                    {
                        let f = &mut floats[..n];
                        for (f, s) in f.iter_mut().zip(buf.iter()) {
                            *f = f32::from(*s) / 32_768.0;
                        }
                        c.process(f);
                        for (s, f) in buf.iter_mut().zip(f.iter()) {
                            *s = (f.clamp(-1.0, 1.0) * 32_767.0) as i16;
                        }
                    }
                    encode.push(Some(buf), (n / ch) as u32, qpc, &*sink)
                });
                if let Err(e) = result {
                    match &e {
                        SourceError::Platform { code, .. }
                            if *code == AUDCLNT_E_DEVICE_INVALIDATED.0 =>
                        {
                            tracing::warn!("microphone went away; reopening");
                        }
                        _ => tracing::warn!(%e, "microphone capture failed; reopening"),
                    }
                    stream = None;
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

    fn set_gate(&mut self, open: bool) {
        self.gate.store(open, Ordering::Relaxed);
    }
}
