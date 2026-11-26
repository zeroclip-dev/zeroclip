//! WASAPI capture plumbing shared by the mic and game-audio sources: the stream,
//! the event, MMCSS registration, timestamping and encoding, and the worker thread.
//!
//! WASAPI is used directly through windows-rs rather than the `wasapi` crate, so the
//! audio loop's no-allocation rule stays under our control.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient,
    IAudioClient, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::w;
use zc_core::Hns;
use zc_core::media::TrackId;
use zc_core::mix::{MixInput, sample_index};
use zc_core::ports::{AudioStart, SourceError};
use zc_core::sink::PacketSink;
use zc_core::time::SampleTimeline;
use zc_win::error::WinResultExt;

use crate::aac::AacEncoder;

/// 16-bit PCM, which the AAC encoder takes directly.
pub(crate) fn pcm16_format(rate: u32, channels: u16) -> WAVEFORMATEX {
    let block = channels * 2;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Registers the current thread with MMCSS as "Pro Audio" until dropped, so audio
/// capture keeps running under game load
/// ([MMCSS](https://learn.microsoft.com/windows/win32/procthread/multimedia-class-scheduler-service)).
pub(crate) struct Mmcss(HANDLE);

impl Mmcss {
    /// `None` if the service refused; capture still works, just at normal priority.
    pub(crate) fn join() -> Option<Self> {
        let mut index = 0u32;
        // SAFETY: plain FFI call with a valid task name and out-parameter.
        unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.ok().map(Self)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: balances the successful registration, on the same thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// An auto-reset event WASAPI signals when a buffer is ready.
pub(crate) struct Event(HANDLE);

impl Event {
    pub(crate) fn new() -> Result<Self, SourceError> {
        // SAFETY: plain FFI call; an unnamed auto-reset event.
        unsafe { CreateEventW(None, false, false, None) }.map(Self).context("CreateEventW")
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    pub(crate) fn wait(&self, ms: u32) -> bool {
        // SAFETY: the handle is valid until drop.
        (unsafe { WaitForSingleObject(self.0, ms) }) == WAIT_OBJECT_0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A started, event-driven, shared-mode capture stream.
pub(crate) struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    channels: usize,
}

impl Stream {
    /// Initialises and starts `client`. A 100 ms buffer: recording isn't latency
    /// bound, and a large buffer survives the game hogging the CPU.
    pub(crate) fn start(
        client: IAudioClient,
        loopback: bool,
        format: &WAVEFORMATEX,
    ) -> Result<Self, SourceError> {
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let event = Event::new()?;
        // SAFETY: plain COM calls on a fresh client with a valid format.
        unsafe {
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, format, None)
                .context("IAudioClient::Initialize")?;
            client.SetEventHandle(event.0).context("SetEventHandle")?;
            let capture: IAudioCaptureClient =
                client.GetService().context("IAudioCaptureClient")?;
            client.Start().context("IAudioClient::Start")?;
            Ok(Self { client, capture, event, channels: usize::from(format.nChannels) })
        }
    }

    /// Waits up to 100 ms for audio, then hands every ready buffer to `f` as
    /// (PCM or `None` for silence, frames, QPC time if valid). Allocates nothing.
    pub(crate) fn read(
        &self,
        mut f: impl FnMut(Option<&[i16]>, u32, Option<Hns>) -> Result<(), SourceError>,
    ) -> Result<(), SourceError> {
        if !self.event.wait(100) {
            return Ok(());
        }
        loop {
            // SAFETY: plain COM call.
            let next = unsafe { self.capture.GetNextPacketSize() }.context("GetNextPacketSize")?;
            if next == 0 {
                return Ok(());
            }
            let (mut data, mut frames, mut flags, mut qpc) =
                (std::ptr::null_mut(), 0u32, 0u32, 0u64);
            // SAFETY: valid out-parameters; the buffer stays valid until ReleaseBuffer.
            unsafe {
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
            }
            .context("GetBuffer")?;
            let pcm =
                (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null()).then(|| {
                    // SAFETY: WASAPI gives `frames` frames of `channels` i16s, valid until
                    // ReleaseBuffer below.
                    unsafe {
                        std::slice::from_raw_parts(
                            data.cast::<i16>(),
                            frames as usize * self.channels,
                        )
                    }
                });
            // u64QPCPosition is already in 100 ns units: don't convert it again.
            let time = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0)
                .then(|| Hns(i64::try_from(qpc).unwrap_or(i64::MAX)));
            let result = f(pcm, frames, time);
            // SAFETY: releases exactly the buffer we got.
            unsafe { self.capture.ReleaseBuffer(frames) }.context("ReleaseBuffer")?;
            result?;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: plain COM call; nothing to do if it fails.
        let _ = unsafe { self.client.Stop() };
    }
}

/// How far behind "now" silence padding stops (see [`Encode::pad_silence`]). It is the same
/// 100 ms as one `Stream::read` wait, so a real buffer whose QPC time is a few ms late still
/// lands after the padding, instead of overlapping it and being skipped by the timeline.
pub(crate) const SILENCE_MARGIN: Hns = Hns::from_millis(100);

/// Timestamps (via [`SampleTimeline`]) and encodes one track. The PCM also goes to the Master
/// mix, when there is one (D30).
pub(crate) struct Encode {
    timeline: SampleTimeline,
    encoder: AacEncoder,
    channels: usize,
    rate: u32,
    /// This source's input in the Master mix, `None` when it isn't in one.
    mix: Option<MixInput>,
}

impl Encode {
    pub(crate) fn new(track: TrackId, config: &AudioStart) -> Result<Self, SourceError> {
        Ok(Self {
            timeline: SampleTimeline::new(config.sample_rate),
            encoder: AacEncoder::new(
                track,
                config.sample_rate,
                config.channels,
                config.bitrate_bps,
            )?,
            channels: usize::from(config.channels),
            rate: config.sample_rate,
            mix: None,
        })
    }

    /// Also writes every captured block into `mix`, at the sample this track's PTS names.
    pub(crate) fn with_mix(mut self, mix: Option<MixInput>) -> Self {
        self.mix = mix;
        self
    }

    /// One captured buffer: `None` PCM means silence; `None` QPC means WASAPI had no
    /// valid timestamp, so the timeline's own estimate is used.
    pub(crate) fn push(
        &mut self,
        pcm: Option<&[i16]>,
        frames: u32,
        qpc: Option<Hns>,
        sink: &dyn PacketSink,
    ) -> Result<(), SourceError> {
        let qpc = qpc.or(self.timeline.expected()).unwrap_or_else(zc_win::clock::now);
        let p = self.timeline.place(qpc, frames);
        if p.restart {
            self.encoder.drain(sink)?;
        }
        if p.silence > 0 {
            self.encoder.encode_silence(p.silence, p.pts, sink)?;
        }
        let start = p.pts + Hns(i64::from(p.silence) * Hns::PER_SECOND / i64::from(self.rate));
        let kept = frames - p.skip;
        match pcm {
            Some(pcm) => {
                let pcm = &pcm[p.skip as usize * self.channels..];
                // Silence needs no write: a sample nothing wrote is silent in the mix.
                if let Some(mix) = &self.mix {
                    mix.write(sample_index(start), pcm);
                }
                self.encoder.encode(pcm, start, sink)
            }
            None => self.encoder.encode_silence(kept, start, sink),
        }
    }

    /// Keeps the track continuous while WASAPI delivers nothing: the game is quiet, there is
    /// no target yet, or the stream is down. Writes silence from the timeline's next frame up
    /// to `now − SILENCE_MARGIN`. Call it before every read, so a buffer that follows a gap (a
    /// retarget, a failed activation) lands after the silence rather than restarting the
    /// timeline. The first call anchors an unanchored timeline, and so the track's time zero,
    /// at `now − SILENCE_MARGIN`.
    pub(crate) fn pad_silence(
        &mut self,
        now: Hns,
        sink: &dyn PacketSink,
    ) -> Result<(), SourceError> {
        if self.timeline.expected().is_none() {
            // Nothing has been encoded yet, so there is nothing to drain: only the anchor moves.
            self.timeline.place(now - SILENCE_MARGIN, 0);
            return Ok(());
        }
        let frames = self.timeline.silence_due(now, SILENCE_MARGIN);
        if frames == 0 {
            return Ok(());
        }
        // No QPC: the timeline's own next frame is on time, so `push` writes plain silence.
        self.push(None, frames, None, sink)
    }

    /// Flushes the encoder; call on stop.
    pub(crate) fn finish(&mut self, sink: &dyn PacketSink) {
        if let Err(e) = self.encoder.drain(sink) {
            tracing::warn!(%e, "audio encoder drain failed");
        }
    }
}

/// A capture thread with a start-up handshake and a stop flag.
#[derive(Debug)]
pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawns `body`, which must call `ready` exactly once after its setup; this
    /// returns that result. `body` runs until the flag it gets is set.
    pub(crate) fn spawn(
        name: &str,
        body: impl FnOnce(&AtomicBool, &mut dyn FnMut(Result<(), SourceError>)) + Send + 'static,
    ) -> Result<Self, SourceError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut ready = |result| {
                    let _ = tx.try_send(result);
                };
                body(&flag, &mut ready);
            })
            .map_err(|e| SourceError::Other(format!("couldn't start the {name} thread: {e}")))?;
        let mut worker = Self { stop, handle: Some(handle) };
        match rx.recv() {
            Ok(Ok(())) => Ok(worker),
            Ok(Err(e)) => {
                worker.stop();
                Err(e)
            }
            Err(_) => {
                worker.stop();
                Err(SourceError::Other(format!("the {name} thread exited during start-up")))
            }
        }
    }

    /// Signals the thread and joins it. Idempotent.
    pub(crate) fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.stop.store(true, Ordering::Release);
            if handle.join().is_err() {
                tracing::error!("an audio capture thread panicked");
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use zc_core::Packet;
    use zc_core::media::PacketRef;
    use zc_win::com::Apartment;
    use zc_win::mf::MediaFoundation;

    use super::*;

    #[derive(Default)]
    struct Collect(Mutex<Vec<Packet>>);

    impl PacketSink for Collect {
        fn push(&self, packet: PacketRef<'_>) {
            self.0.lock().unwrap().push(packet.to_packet());
        }
    }

    #[test]
    fn silent_then_loud_track_has_no_gap() {
        let (Ok(_apartment), Ok(_mf)) = (Apartment::multithreaded(), MediaFoundation::start())
        else {
            return; // no Media Foundation on this image
        };
        let config = AudioStart { sample_rate: 48_000, channels: 2, bitrate_bps: 192_000 };
        let mut encode = Encode::new(TrackId::GameAudio, &config).expect("encoder");
        let sink = Collect::default();
        let ms = Hns::from_millis;
        let start = Hns::from_secs(100);
        // 1 s of game silence: no buffers arrive, so the capture loop pads every 100 ms.
        for k in 0..=10 {
            encode.pad_silence(start + ms(100 * k), &sink).expect("pad");
        }
        // 1 s of loud 10 ms buffers, each read 1 ms after its capture.
        let loud = [4_000i16; 480 * 2];
        for k in 0..100 {
            let captured = start + ms(1_000 + 10 * k);
            encode.pad_silence(captured + ms(1), &sink).expect("pad");
            encode.push(Some(&loud[..]), 480, Some(captured), &sink).expect("push");
        }
        // Silence again.
        for k in 21..=25 {
            encode.pad_silence(start + ms(100 * k), &sink).expect("pad");
        }
        encode.finish(&sink);
        let packets = sink.0.into_inner().unwrap();
        assert!(packets.len() > 100, "{} packets", packets.len());
        let first = packets[0].meta.pts;
        assert!(first < start + ms(500), "the track starts at {first}, not at the silence");
        let last = packets.last().expect("packets");
        assert!(last.meta.pts + last.meta.duration >= start + ms(2_300), "the track ends early");
        // Neighbouring packets touch: no gap longer than 25 ms, and no overlap.
        for pair in packets.windows(2) {
            let gap = pair[1].meta.pts - (pair[0].meta.pts + pair[0].meta.duration);
            assert!(gap.ticks().abs() <= ms(25).ticks(), "gap {gap} after {}", pair[0].meta.pts);
        }
    }
}
