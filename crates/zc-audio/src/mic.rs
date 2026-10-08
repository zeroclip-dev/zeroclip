use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, DEVICE_STATE_ACTIVE, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows_core::HSTRING;
use zc_core::Hns;
use zc_core::dsp::{Compressor, CompressorParams};
use zc_core::media::TrackId;
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;
use zc_win::com::Apartment;
use zc_win::error::WinResultExt;
use zc_win::mf::MediaFoundation;

use crate::capture::{Encode, Mmcss, Stream, Worker, pcm16_format};

/// Audio track 3: the microphone, gated by push-to-talk (D30).
///
/// The chosen capture device, or the default *communications* one (the headset
/// mic, usually), in shared mode. A closed gate writes silence, so the track keeps its length and
/// timestamps (DESIGN §6). The optional compressor has no lookahead, so the Mic
/// track needs no offset (`Compressor::latency`, `MasterClock::set_track_offset`).
/// If the mic is unplugged or the default device changes, capture reopens every
/// second; the gap becomes silence. A chosen device that is missing is waited for,
/// never swapped for another microphone.
#[derive(Debug)]
pub struct MicSource {
    /// Shared with the capture thread; flipped by the push-to-talk hotkey.
    gate: Arc<AtomicBool>,
    compressor: bool,
    /// Endpoint id of the chosen mic; `None` is the Windows default.
    device: Option<String>,
    worker: Option<Worker>,
}

impl MicSource {
    /// A stopped mic source. `always_on` opens the gate permanently (no PTT);
    /// `device` is an id from [`list_microphones`], or `None` for the default.
    #[must_use]
    pub fn new(always_on: bool, compressor: bool, device: Option<String>) -> Self {
        Self { gate: Arc::new(AtomicBool::new(always_on)), compressor, device, worker: None }
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

/// A capture device Windows reports as plugged in and enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicDevice {
    /// Stable Windows endpoint id; what `AudioSettings::mic_device` stores.
    pub id: String,
    /// What the user sees, for example "Microphone (Realtek Audio)".
    pub name: String,
}

/// The microphones available right now, in Windows' order. Empty on any failure.
///
/// Safe to call from the UI thread: the COM work runs on its own short-lived thread.
#[must_use]
pub fn list_microphones() -> Vec<MicDevice> {
    std::thread::Builder::new()
        .name("zc-mic-list".into())
        .spawn(|| {
            enumerate()
                .inspect_err(|e| tracing::warn!(%e, "couldn't list microphones"))
                .unwrap_or_default()
        })
        .ok()
        .and_then(|t| t.join().ok())
        .unwrap_or_default()
}

fn enumerate() -> Result<Vec<MicDevice>, SourceError> {
    let _apartment = Apartment::multithreaded()?;
    let mut found = Vec::new();
    // SAFETY: plain COM calls on this thread's apartment; the id string is freed
    // with `CoTaskMemFree` after it is copied.
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("MMDeviceEnumerator")?;
        let list = devices
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .context("EnumAudioEndpoints")?;
        for i in 0..list.GetCount().context("GetCount")? {
            let device = list.Item(i).context("Item")?;
            let raw = device.GetId().context("GetId")?;
            let id = raw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(raw.as_ptr().cast()));
            let name = device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .map_or_else(|_| "Microphone".to_owned(), |v| v.to_string());
            if !id.is_empty() {
                found.push(MicDevice { id, name });
            }
        }
    }
    Ok(found)
}

fn open_mic(config: &AudioStart, chosen: Option<&str>) -> Result<Stream, SourceError> {
    // SAFETY: plain COM calls on this thread's apartment.
    let client: IAudioClient = unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("MMDeviceEnumerator")?;
        let device: IMMDevice = match chosen {
            Some(id) => devices.GetDevice(&HSTRING::from(id)).map_err(|_| {
                SourceError::Unsupported("the chosen microphone isn't connected".into())
            })?,
            None => devices
                .GetDefaultAudioEndpoint(eCapture, eCommunications)
                .map_err(|_| SourceError::Unsupported("no microphone".into()))?,
        };
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
        let device = self.device.clone();
        self.worker = Some(Worker::spawn("zc-mic", move |stop, ready| {
            let setup = (|| {
                let apartment = Apartment::multithreaded()?;
                let mf = MediaFoundation::start()?;
                let encode = Encode::new(TrackId::Mic, &config)?;
                // A missing mic isn't fatal: the loop below keeps trying (a dock, a USB plug).
                let stream = open_mic(&config, device.as_deref())
                    .inspect_err(
                        |e| tracing::warn!(%e, "microphone not available yet; will keep trying"),
                    )
                    .ok();
                Ok((apartment, mf, encode, stream))
            })();
            let (_apartment, _mf, mut encode, stream) = match setup {
                Ok(parts) => {
                    ready(Ok(()));
                    if parts.3.is_some() {
                        tracing::info!("microphone capturing");
                    }
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
            let mut stream = stream;
            // While the mic is away, retry once a second but keep the track continuous.
            let mut retry_at = Hns::ZERO;
            while !stop.load(Ordering::Acquire) {
                let Some(s) = &stream else {
                    // Silence up to 100 ms ago, so the mic track has no hole (as game audio).
                    if let Err(e) = encode.pad_silence(zc_win::clock::now(), &*sink) {
                        tracing::warn!(%e, "microphone silence failed");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    let now = zc_win::clock::now();
                    if now < retry_at {
                        continue;
                    }
                    retry_at = now + Hns::from_secs(1);
                    stream = open_mic(&config, device.as_deref())
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
