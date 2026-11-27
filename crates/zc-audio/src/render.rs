//! In-process audio playback for the previews: a WASAPI shared-mode render stream fed from a
//! bounded queue, and the device's own clock for "what is audible now".
//!
//! The caller's decode thread writes `48 kHz` stereo `f32` PCM through a [`PcmWriter`]; the
//! player's thread moves it into the device. The UI reads [`PcmPlayer::played`] to pace the
//! picture, so sound and picture share one clock.
//!
//! - **Endpoint:** the default render device of the console role, from
//!   [`IMMDeviceEnumerator::GetDefaultAudioEndpoint`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immdeviceenumerator-getdefaultaudioendpoint).
//!   A missing endpoint and every later device error are reported as "no sound"
//!   ([`PcmPlayer::failed`]), never a panic; the app falls back to its wall clock.
//! - **Mode:** [shared mode, event driven](https://learn.microsoft.com/en-us/windows/win32/coreaudio/rendering-a-stream),
//!   with `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` so the engine converts our fixed format to
//!   the device's mix format ([stream flags](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audclnt-streamflags-xxx-constants)).
//!   The buffer is three device periods (about 30 ms), so a volume change reaches the speakers
//!   quickly. The render thread is registered with MMCSS like the capture threads.
//! - **Clock:** [`IAudioClock::GetPosition`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getposition)
//!   is the position of the sample being heard now (it already includes the device's latency).
//!   It also counts silence we insert when the queue runs dry, so that silence is subtracted
//!   again: the clock stops while the decoder is late, and the picture waits for it. After
//!   [`PcmWriter::finish`] the silence counts, so the clock keeps running to the end of a piece
//!   whose sound is shorter than its picture.
//! - **Start:** the device starts only once about 50 ms are queued (or the stream is finished),
//!   so it doesn't begin with an underrun.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioClock, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use zc_core::Hns;
use zc_core::ports::SourceError;
use zc_win::com::Apartment;
use zc_win::error::WinResultExt;

use crate::capture::{Event, Mmcss};

/// Sample rate of the PCM the caller writes.
pub const PCM_RATE: u32 = 48_000;
/// Channel count of the PCM the caller writes (interleaved stereo).
pub const PCM_CHANNELS: usize = 2;
/// Frames the queue holds: 200 ms, so back-pressure keeps the decoder close behind the device.
const QUEUE_FRAMES: usize = 9_600;
/// Frames queued before the device starts: 50 ms.
const PREROLL_FRAMES: usize = 2_400;
/// `WAVE_FORMAT_IEEE_FLOAT`: 32-bit float samples.
const FORMAT_IEEE_FLOAT: u16 = 3;
/// The clock is extrapolated from the last device reading by at most this much, so a stalled
/// device can't run it on forever.
const MAX_EXTRAPOLATION: Hns = Hns::from_millis(100);
/// How long a blocked write waits before it re-checks for a close.
const POLL: Duration = Duration::from_millis(20);

/// `frames` at `rate` Hz as [`Hns`].
fn frames_to_hns(frames: u64, rate: u32) -> Hns {
    let hns = u128::from(frames) * Hns::PER_SECOND as u128 / u128::from(rate.max(1));
    Hns(i64::try_from(hns).unwrap_or(i64::MAX))
}

/// Whole frames at `rate` Hz in `hns` ticks.
fn hns_to_frames(hns: i64, rate: u32) -> u64 {
    (hns.max(0) as u64) * u64::from(rate) / Hns::PER_SECOND as u64
}

/// A fixed-capacity FIFO of interleaved samples, plus the stream's end and close flags.
struct Ring {
    buf: Vec<f32>,
    head: usize,
    len: usize,
    /// The writer has written everything it will.
    finished: bool,
    /// The player is gone or failed: writes are refused.
    closed: bool,
}

impl Ring {
    fn new(samples: usize) -> Self {
        Self { buf: vec![0.0; samples], head: 0, len: 0, finished: false, closed: false }
    }

    /// Appends as much of `src` as fits; returns how many samples were taken.
    fn push(&mut self, src: &[f32]) -> usize {
        let cap = self.buf.len();
        let n = src.len().min(cap - self.len);
        let tail = (self.head + self.len) % cap;
        let first = n.min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        self.buf[..n - first].copy_from_slice(&src[first..n]);
        self.len += n;
        n
    }

    /// Moves up to `dst.len()` samples out, oldest first; returns how many.
    fn pop(&mut self, dst: &mut [f32]) -> usize {
        let cap = self.buf.len();
        let n = dst.len().min(self.len);
        let first = n.min(cap - self.head);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        dst[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % cap;
        self.len -= n;
        n
    }
}

/// The last device reading, for [`PcmPlayer::played`].
#[derive(Clone, Copy, Default)]
struct Reading {
    /// The device has begun playing samples.
    started: bool,
    /// Audible position of the real audio, without inserted silence.
    audible: Hns,
    /// QPC time of that reading.
    at: Hns,
}

struct Shared {
    queue: Mutex<Ring>,
    /// Signalled when the render thread made room, or on close.
    room: Condvar,
    reading: Mutex<Reading>,
    /// Largest value `played` has returned, so the clock never steps back.
    last: AtomicI64,
    stop: AtomicBool,
    failed: AtomicBool,
}

impl Shared {
    fn new(queue_samples: usize) -> Self {
        Self {
            queue: Mutex::new(Ring::new(queue_samples)),
            room: Condvar::new(),
            reading: Mutex::new(Reading::default()),
            last: AtomicI64::new(0),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    fn queue(&self) -> MutexGuard<'_, Ring> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reading(&self) -> Reading {
        *self.reading.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuses further writes and wakes a blocked writer.
    fn close(&self) {
        self.queue().closed = true;
        self.room.notify_all();
    }
}

/// Plays PCM on the default render device. Dropping it stops the sound and joins its thread.
pub struct PcmPlayer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PcmPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmPlayer").field("failed", &self.failed()).finish_non_exhaustive()
    }
}

/// The producer's end of a [`PcmPlayer`]'s queue; cheap to clone, `Send`.
#[derive(Clone)]
pub struct PcmWriter(Arc<Shared>);

impl PcmPlayer {
    /// Starts a player. Returns at once: the device opens on the player's thread, so a slow or
    /// missing endpoint never blocks the caller. Watch [`started`](Self::started) and
    /// [`failed`](Self::failed).
    #[must_use]
    pub fn new() -> Self {
        let shared = Arc::new(Shared::new(QUEUE_FRAMES * PCM_CHANNELS));
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("zc-pcm-render".into())
            .spawn(move || {
                if let Err(e) = run(&worker) {
                    tracing::warn!(%e, "preview audio device failed; playing without sound");
                    worker.failed.store(true, Ordering::Release);
                    worker.close();
                }
            })
            .inspect_err(|e| tracing::warn!(%e, "couldn't start the preview audio thread"))
            .ok();
        if thread.is_none() {
            shared.failed.store(true, Ordering::Release);
            shared.close();
        }
        Self { shared, thread }
    }

    /// A handle to write PCM through.
    #[must_use]
    pub fn writer(&self) -> PcmWriter {
        PcmWriter(Arc::clone(&self.shared))
    }

    /// The audio heard so far, in output time (what the device has actually played, latency
    /// included, minus any silence inserted because the writer was late). Zero until
    /// [`started`](Self::started). Never goes backwards.
    #[must_use]
    pub fn played(&self) -> Hns {
        let reading = self.shared.reading();
        if !reading.started {
            return Hns::ZERO;
        }
        let age = (zc_win::clock::now() - reading.at).clamp(Hns::ZERO, MAX_EXTRAPOLATION);
        let now = (reading.audible + age).ticks();
        Hns(self.shared.last.fetch_max(now, Ordering::AcqRel).max(now))
    }

    /// `true` once the device has begun playing samples.
    #[must_use]
    pub fn started(&self) -> bool {
        self.shared.reading().started
    }

    /// `true` if the device couldn't be opened or died (unplugged, invalidated). There is no
    /// sound then, and [`played`](Self::played) no longer advances.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }
}

impl Default for PcmPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PcmPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.close();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the preview audio thread panicked");
        }
    }
}

impl PcmWriter {
    /// Queues interleaved stereo `f32` frames (a trailing odd sample is dropped), blocking
    /// while the queue is full: that back-pressure paces the decoder. Returns `false` once the
    /// player is gone or failed, which is the producer's cue to stop. Allocates nothing.
    pub fn write(&self, samples: &[f32]) -> bool {
        let mut rest = &samples[..samples.len() / PCM_CHANNELS * PCM_CHANNELS];
        let mut queue = self.0.queue();
        loop {
            if queue.closed {
                return false;
            }
            let n = queue.push(rest);
            rest = &rest[n..];
            if rest.is_empty() {
                return true;
            }
            queue = self.0.room.wait_timeout(queue, POLL).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Says nothing more will be written. The device then plays out the queue and keeps the
    /// clock running on silence.
    pub fn finish(&self) {
        self.0.queue().finished = true;
    }
}

/// An opened, not yet started, render stream.
struct Render {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: Event,
    buffer_frames: u32,
    period_frames: u32,
    /// `IAudioClock` position units per second.
    clock_rate: u64,
}

fn open() -> Result<Render, SourceError> {
    let format = WAVEFORMATEX {
        wFormatTag: FORMAT_IEEE_FLOAT,
        nChannels: PCM_CHANNELS as u16,
        nSamplesPerSec: PCM_RATE,
        nAvgBytesPerSec: PCM_RATE * PCM_CHANNELS as u32 * 4,
        nBlockAlign: PCM_CHANNELS as u16 * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let event = Event::new()?;
    // SAFETY: plain COM calls on this thread's apartment with a valid format and a handle that
    // outlives the client (both live in `Render`, which declares the client first).
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("MMDeviceEnumerator")?;
        let device = devices
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|_| SourceError::Unsupported("no audio output device".into()))?;
        let client: IAudioClient =
            device.Activate(CLSCTX_ALL, None).context("IMMDevice::Activate")?;
        let mut period = 0i64;
        client.GetDevicePeriod(Some(&mut period), None).context("GetDevicePeriod")?;
        let period = period.max(Hns::PER_MILLI * 3);
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                period * 3,
                0,
                &format,
                None,
            )
            .context("IAudioClient::Initialize")?;
        client.SetEventHandle(event.handle()).context("SetEventHandle")?;
        let render = client.GetService().context("IAudioRenderClient")?;
        let clock: IAudioClock = client.GetService().context("IAudioClock")?;
        let clock_rate = clock.GetFrequency().context("IAudioClock::GetFrequency")?.max(1);
        let buffer_frames = client.GetBufferSize().context("GetBufferSize")?;
        let period_frames = (hns_to_frames(period, PCM_RATE) as u32).clamp(1, buffer_frames);
        Ok(Render { client, render, clock, event, buffer_frames, period_frames, clock_rate })
    }
}

/// The render thread: open, wait for pre-roll, then feed the device on every event.
fn run(shared: &Shared) -> Result<(), SourceError> {
    let _apartment = Apartment::multithreaded()?;
    let r = open()?;
    let _mmcss = Mmcss::join();
    // Wait for a little audio (or the end of the stream) so the device starts with data.
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        {
            let q = shared.queue();
            if q.len >= PREROLL_FRAMES * PCM_CHANNELS || q.finished {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut silence_frames = 0u64;
    fill(&r, shared, &mut silence_frames)?;
    // SAFETY: plain COM call on a fully initialised client.
    unsafe { r.client.Start() }.context("IAudioClient::Start")?;
    let clock_rate = i64::try_from(r.clock_rate).unwrap_or(i64::MAX);
    let mut audible = Hns::ZERO;
    while !shared.stop.load(Ordering::Acquire) {
        r.event.wait(20);
        fill(&r, shared, &mut silence_frames)?;
        let (mut position, mut qpc) = (0u64, 0u64);
        // SAFETY: valid out-parameters for the call.
        unsafe { r.clock.GetPosition(&mut position, Some(&mut qpc)) }.context("GetPosition")?;
        let device =
            zc_win::clock::ticks_to_hns(i64::try_from(position).unwrap_or(i64::MAX), clock_rate);
        // The clock never steps back, even when silence is inserted ahead of the position.
        audible = audible.max((device - frames_to_hns(silence_frames, PCM_RATE)).max(Hns::ZERO));
        // u64QPCPosition is already in 100 ns units; fall back to now if it isn't given.
        let at = match i64::try_from(qpc) {
            Ok(q) if q > 0 => Hns(q),
            _ => zc_win::clock::now(),
        };
        *shared.reading.lock().unwrap_or_else(PoisonError::into_inner) =
            Reading { started: position > 0, audible, at };
    }
    // SAFETY: plain COM call; nothing to do if it fails.
    let _ = unsafe { r.client.Stop() };
    Ok(())
}

/// Moves queued PCM into the device buffer. If the device is about to run dry, pads silence up
/// to one period (counted in `silence_frames` unless the stream is finished, when silence is
/// simply the rest of the piece).
fn fill(r: &Render, shared: &Shared, silence_frames: &mut u64) -> Result<(), SourceError> {
    // SAFETY: plain COM call.
    let padding = unsafe { r.client.GetCurrentPadding() }.context("GetCurrentPadding")?;
    let avail = r.buffer_frames.saturating_sub(padding);
    let (queued, finished) = {
        let q = shared.queue();
        (q.len / PCM_CHANNELS, q.finished)
    };
    let real = avail.min(u32::try_from(queued).unwrap_or(u32::MAX));
    let silence = r.period_frames.saturating_sub(padding + real).min(avail - real);
    let total = real + silence;
    if total == 0 {
        return Ok(());
    }
    // SAFETY: GetBuffer hands out `total` frames of f32 stereo (our stream format), valid and
    // exclusively ours until ReleaseBuffer; the slice isn't used after it.
    unsafe {
        let ptr = r.render.GetBuffer(total).context("GetBuffer")?;
        let out = std::slice::from_raw_parts_mut(ptr.cast::<f32>(), total as usize * PCM_CHANNELS);
        let (data, pad) = out.split_at_mut(real as usize * PCM_CHANNELS);
        if real > 0 {
            shared.queue().pop(data);
            shared.room.notify_all();
        }
        pad.fill(0.0);
        let flags = if real == 0 { AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 } else { 0 };
        r.render.ReleaseBuffer(total, flags).context("ReleaseBuffer")?;
    }
    if !finished {
        *silence_frames += u64::from(silence);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_convert_to_hns_exactly() {
        assert_eq!(frames_to_hns(48_000, 48_000), Hns::from_secs(1));
        assert_eq!(frames_to_hns(480, 48_000), Hns::from_millis(10));
        assert_eq!(frames_to_hns(0, 48_000), Hns::ZERO);
        // A bad rate doesn't divide by zero.
        assert_eq!(frames_to_hns(10, 0), Hns(100_000_000));
        assert_eq!(hns_to_frames(Hns::from_millis(10).ticks(), 48_000), 480);
        assert_eq!(hns_to_frames(-5, 48_000), 0);
    }

    #[test]
    fn ring_keeps_order_across_the_wrap_and_refuses_overflow() {
        let mut ring = Ring::new(6);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0]), 4);
        let mut out = [0.0; 3];
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        // 1 sample is queued, 5 are free: the wrap is crossed and the sixth is refused.
        assert_eq!(ring.push(&[5.0, 6.0, 7.0, 8.0, 9.0, 10.0]), 5);
        assert_eq!(ring.push(&[11.0]), 0);
        let mut all = [0.0; 8];
        assert_eq!(ring.pop(&mut all), 6);
        assert_eq!(&all[..6], &[4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert_eq!(ring.pop(&mut all), 0);
    }

    #[test]
    fn writer_blocks_until_room_and_refuses_after_close() {
        let shared = Arc::new(Shared::new(8));
        let writer = PcmWriter(Arc::clone(&shared));
        assert!(writer.write(&[0.5; 8]));
        let blocked = {
            let writer = writer.clone();
            std::thread::spawn(move || writer.write(&[0.25; 4]))
        };
        std::thread::sleep(Duration::from_millis(60));
        let mut out = [0.0; 4];
        assert_eq!(shared.queue().pop(&mut out), 4);
        shared.room.notify_all();
        assert!(blocked.join().expect("writer thread"));
        shared.close();
        assert!(!writer.write(&[0.0; 2]));
    }
}
