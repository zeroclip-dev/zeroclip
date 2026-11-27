//! [`GpuVideoSource`]: the pipeline behind the engine's `VideoSource` port.
//!
//! Threads:
//! - **WGC frame pool** ([`FrameHandler::on_frame`]): pace to the target fps, convert
//!   BGRA → NV12 on the GPU, hand the ring slot to the encoder thread. Never blocks.
//! - **`zc-video` encoder thread** ([`run`]): owns everything that isn't `Send` (the
//!   COM apartment, Media Foundation, the encoder) plus the capture session. It
//!   feeds the encoder when it asks for input, pushes its output into the sink, and
//!   repeats the last frame when the screen is static.
//! - **Engine thread**: only calls `start`, `stop`, `request_keyframe`, `screenshot`.
//!
//! Windows choices: frames come from [Windows.Graphics.Capture](https://learn.microsoft.com/en-us/windows/uwp/audio-video-camera/screen-capture),
//! which hands out GPU textures for a [`GraphicsCaptureItem`](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscaptureitem),
//! so frames never pass through system memory. They live on one Direct3D 11 device with
//! [multithread protection](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nn-d3d11-id3d11multithread),
//! because the capture callback and the encoder thread both use it. The encoder thread
//! joins the multithreaded apartment ([`Apartment`]) and starts Media Foundation
//! ([`MediaFoundation`]) for the hardware encoder.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Graphics::Capture::GraphicsCaptureItem;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use zc_core::Hns;
use zc_core::backoff::Backoff;
use zc_core::media::{Codec, TrackId, TrackInfo, TrackKind};
use zc_core::ports::{Emit, SourceError, VideoSource, VideoStart};
use zc_core::sink::SharedSink;
use zc_core::time::FramePacer;
use zc_core::watchdog::HealthEvent;
use zc_win::clock;
use zc_win::com::Apartment;
use zc_win::error::WinResultExt;
use zc_win::mf::MediaFoundation;

use crate::capture::{CapturedFrame, WgcCapture, capture_item, hdr_sdr_white};
use crate::convert::NvConverter;
use crate::device::create_device;
use crate::encoder::{EncoderEvent, HwEncoder, probe};
use crate::screenshot::{save_png, staging_copy};
use crate::tonemap::ToneMapper;

/// A staging copy of one frame, and its size.
type Shot = Result<(ID3D11Texture2D, (u32, u32)), SourceError>;

/// A GPU copy of the newest accepted frame: the texture, its buffer size, and the
/// content size within it.
type Latest = (ID3D11Texture2D, (u32, u32), (u32, u32));

/// Flags the engine thread sets and the pipeline threads read.
#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    screenshot_wanted: AtomicBool,
    screenshot: Mutex<Option<SyncSender<Shot>>>,
    /// Frames WGC delivered, before pacing. For the periodic health log.
    arrived: AtomicU64,
    /// Same, but never reset: the encoder thread watches it for stalls.
    seen: AtomicU64,
    /// Converted frames sitting in the hand-off channel, not yet submitted. The converter only
    /// reuses an NV12 slot after its frame left the channel, so a stalled encoder can't have
    /// queued slots overwritten under it.
    queued: AtomicUsize,
    /// Frames lost before the channel, per 5 s health log (one per frame interval at most).
    dropped: AtomicU64,
}

/// How many converted frames may wait for the encoder. Half the converter's 8-slot ring, the
/// other half covers the frames the encoder itself holds; the headroom rides out GPU contention
/// from a game without dropping.
const QUEUE_DEPTH: usize = 4;

/// How long WGC may send nothing before capture is restarted (see `run`).
const STALL: Duration = Duration::from_secs(3);

/// Minimum gap between "dropping frames" warnings.
const DROP_WARN_EVERY: Duration = Duration::from_secs(5);

/// What the encoder thread reports once it's running.
struct Started {
    codec: Codec,
    /// Encoded size, pixels (even, as the encoder was opened).
    size: (u32, u32),
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

/// Capture → convert → encode, on the GPU.
///
/// `start` wires [`device`](crate::device), [`capture`](crate::capture),
/// [`convert`](crate::convert) and [`encoder`](crate::encoder) together; `stop`
/// tears them down (and is what the watchdog's re-init calls before starting again).
#[derive(Debug, Default)]
pub struct GpuVideoSource {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    gpu: Option<(ID3D11Device, ID3D11DeviceContext)>,
    /// Set by `start` once the encoder exists, cleared by `stop`.
    format: Option<TrackInfo>,
}

impl GpuVideoSource {
    /// A stopped pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Drop for GpuVideoSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl VideoSource for GpuVideoSource {
    fn start(
        &mut self,
        config: &VideoStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<Codec, SourceError> {
        self.stop();
        self.shared = Arc::default();
        let (report, started) = mpsc::sync_channel(1);
        let (config, shared) = (*config, Arc::clone(&self.shared));
        let thread = thread::Builder::new()
            .name("zc-video".into())
            .spawn(move || run(&config, &sink, &health, &shared, &report))
            .map_err(|e| SourceError::Other(format!("couldn't start the video thread: {e}")))?;
        self.thread = Some(thread);
        match started.recv() {
            Ok(Ok(Started { codec, size, device, context })) => {
                self.gpu = Some((device, context));
                self.format = Some(TrackInfo {
                    track: TrackId::Video,
                    label: "Video".into(),
                    kind: TrackKind::Video {
                        codec,
                        width: size.0,
                        height: size.1,
                        fps: config.fps,
                    },
                    // Empty: the muxer takes the parameter sets from the first IDR.
                    codec_private: Vec::new(),
                });
                Ok(codec)
            }
            Ok(Err(e)) => {
                self.stop();
                Err(e)
            }
            Err(_) => {
                self.stop();
                Err(SourceError::Other("the video thread exited during start-up".into()))
            }
        }
    }

    fn format(&self) -> Option<TrackInfo> {
        self.format.clone()
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.shared.stop.store(true, Ordering::Release);
            if thread.join().is_err() {
                tracing::error!("the video thread panicked");
            }
        }
        self.gpu = None;
        self.format = None;
    }

    fn request_keyframe(&mut self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    fn screenshot(&mut self, path: &Path) -> Result<(), SourceError> {
        let Some((_, context)) = &self.gpu else {
            return Err(SourceError::Other("capture isn't running".into()));
        };
        let (tx, rx) = mpsc::sync_channel(1);
        *self.shared.screenshot.lock().map_err(|_| SourceError::Other("poisoned lock".into()))? =
            Some(tx);
        self.shared.screenshot_wanted.store(true, Ordering::Release);
        let Ok(shot) = rx.recv_timeout(Duration::from_secs(2)) else {
            // Withdraw the request, or a late frame would take a screenshot nobody waits for.
            self.shared.screenshot_wanted.store(false, Ordering::Release);
            if let Ok(mut slot) = self.shared.screenshot.lock() {
                *slot = None;
            }
            return Err(SourceError::Other("no frame captured yet".into()));
        };
        let (staging, size) = shot?;
        save_png(context, &staging, size, path)
    }
}

/// Runs on the WGC frame pool's thread for every captured frame (up to the display
/// refresh rate), and on the encoder thread for repeats.
struct FrameHandler {
    pacer: FramePacer,
    converter: NvConverter,
    frames: SyncSender<(usize, Hns)>,
    health: Emit<HealthEvent>,
    shared: Arc<Shared>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    last: Option<Hns>,
    dropping: bool,
    /// When the last drop was counted; at most one per frame interval, so a 200 Hz
    /// refresh doesn't flood the health channel.
    last_drop: Option<Hns>,
    /// When the last drop warning was logged; one per [`DROP_WARN_EVERY`].
    warned: Option<Instant>,
    /// Kept so a screenshot never waits for the screen to change (WGC sends nothing
    /// while it's still).
    latest: Option<Latest>,
    /// Made on the first HDR frame; turns float scRGB into the BGRA8 the rest expects.
    tonemap: Option<ToneMapper>,
}

impl FrameHandler {
    fn on_frame(&mut self, frame: &CapturedFrame) {
        self.shared.arrived.fetch_add(1, Ordering::Relaxed);
        self.shared.seen.fetch_add(1, Ordering::Relaxed);
        if self.shared.queued.load(Ordering::Acquire) >= QUEUE_DEPTH {
            // Converting now would advance the ring onto a slot still waiting in the channel.
            // Checked before the pacer and any GPU work, so a dropped refresh costs nothing and
            // doesn't use up the frame slot: the next refresh can still fill it.
            self.drop_frame(frame.time);
            return;
        }
        if !self.pacer.accept(frame.time) {
            return;
        }
        let slot = match self.sdr_frame(frame) {
            Ok(texture) => {
                self.keep_latest(&texture, frame.size);
                self.converter.convert(&texture, frame.size)
            }
            Err(e) => Err(e),
        };
        self.hand_off(slot, frame.time);
    }

    /// The frame as BGRA8: itself, or tone-mapped when it came from an HDR display.
    fn sdr_frame(&mut self, frame: &CapturedFrame) -> Result<ID3D11Texture2D, SourceError> {
        let Some(white) = frame.sdr_white else { return Ok(frame.texture.clone()) };
        if self.tonemap.is_none() {
            self.tonemap = Some(ToneMapper::new(&self.device, &self.context)?);
        }
        let mapper =
            self.tonemap.as_mut().ok_or_else(|| SourceError::Other("no tone mapper".into()))?;
        mapper.map(&frame.texture, white).cloned()
    }

    /// One GPU copy per encoded frame into a reused texture (~8 MB of VRAM at 1080p).
    /// Failures only cost screenshots, so they're logged and skipped.
    fn keep_latest(&mut self, texture: &ID3D11Texture2D, content: (u32, u32)) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter.
        unsafe { texture.GetDesc(&mut desc) };
        let dims = (desc.Width, desc.Height);
        if self.latest.as_ref().is_none_or(|(_, d, _)| *d != dims) {
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            let mut kept = None;
            // SAFETY: a valid descriptor and out-parameter.
            if let Err(e) = unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut kept)) } {
                tracing::debug!(%e, "can't keep a frame for screenshots");
                return;
            }
            self.latest = kept.map(|t| (t, dims, content));
        }
        if let Some((kept, _, size)) = &mut self.latest {
            // SAFETY: same size and format; a GPU copy on a multithread-protected device.
            unsafe { self.context.CopyResource(&*kept, texture) };
            *size = content;
        }
    }

    /// Called by the encoder thread about every millisecond: answers screenshot
    /// requests from the latest frame, and on a static screen (WGC delivers nothing)
    /// repeats the last frame to keep GOPs on time.
    fn tick(&mut self, now: Hns) {
        if let Some((texture, _, size)) = &self.latest
            && self.shared.screenshot_wanted.swap(false, Ordering::Acquire)
            && let Ok(mut slot) = self.shared.screenshot.try_lock()
            && let Some(tx) = slot.take()
        {
            let shot = staging_copy(&self.device, &self.context, texture).map(|t| (t, *size));
            let _ = tx.try_send(shot);
        }
        if self.shared.queued.load(Ordering::Acquire) < QUEUE_DEPTH
            && let Some(time) = self.pacer.repeat_due(now)
            && let Some(slot) = self.converter.repeat_last()
        {
            self.hand_off(Ok(slot), time);
        }
    }

    /// Counts a frame lost because the encoder's queue is full, at most once per frame interval.
    fn drop_frame(&mut self, time: Hns) {
        if self.last_drop.is_some_and(|last| time - last < self.pacer.interval()) {
            return;
        }
        self.last_drop = Some(time);
        if self.warn_due() {
            tracing::warn!("dropping frames: the encoder isn't keeping up");
        }
        self.note_drop();
    }

    /// Whether a drop warning may be logged now (one per [`DROP_WARN_EVERY`]); the total is in
    /// the periodic health log anyway.
    fn warn_due(&mut self) -> bool {
        let due = self.warned.is_none_or(|at| at.elapsed() >= DROP_WARN_EVERY);
        if due {
            self.warned = Some(Instant::now());
        }
        due
    }

    /// Books one drop: the 5 s counter, and the watchdog's event.
    fn note_drop(&mut self) {
        self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        (self.health)(HealthEvent::FrameDropped);
        self.dropping = true;
    }

    fn hand_off(&mut self, slot: Result<usize, SourceError>, time: Hns) {
        let sent = slot.map(|slot| self.frames.try_send((slot, time)));
        match sent {
            Ok(Ok(())) => {
                self.shared.queued.fetch_add(1, Ordering::AcqRel);
                // ponytail: FrameDelivered only on leaving a drop run. The engine's
                // health channel allocates as it grows, and the Phase 4 watchdog only
                // needs it to reset its drop count.
                if self.dropping
                    && let Some(last) = self.last
                {
                    (self.health)(HealthEvent::FrameDelivered { interval: time - last });
                }
                self.dropping = false;
                self.last = Some(time);
            }
            Ok(Err(TrySendError::Disconnected(_))) => {}
            Ok(Err(TrySendError::Full(_))) | Err(_) => {
                if let Err(e) = &sent {
                    if self.warn_due() {
                        tracing::warn!(%e, "dropping frames: conversion failed");
                    }
                    // SAFETY: plain COM call on a live device.
                    if let Err(lost) = unsafe { self.device.GetDeviceRemovedReason() } {
                        (self.health)(HealthEvent::EncoderFault { hresult: lost.code().0 });
                    }
                } else if self.warn_due() {
                    tracing::warn!("dropping frames: the encoder isn't keeping up");
                }
                self.note_drop();
            }
        }
    }
}

/// The encoder thread. Reports start-up through `report`, then runs until `stop`.
fn run(
    config: &VideoStart,
    sink: &SharedSink,
    health: &Emit<HealthEvent>,
    shared: &Arc<Shared>,
    report: &SyncSender<Result<Started, SourceError>>,
) {
    // Guards first, so they drop last.
    let guards = Apartment::multithreaded().and_then(|a| Ok((a, MediaFoundation::start()?)));
    let _guards = match guards {
        Ok(guards) => guards,
        Err(e) => {
            let _ = report.send(Err(e));
            return;
        }
    };
    let pipeline = match Pipeline::open(config, health, shared) {
        Ok(pipeline) => pipeline,
        Err(e) => {
            let _ = report.send(Err(e));
            return;
        }
    };
    let Pipeline { device, context, codec, size, mut encoder, handler, frames, item } = pipeline;

    // Protected content (HDCP) can block starting capture; retry with backoff.
    let mut backoff = Backoff::hdcp();
    let try_capture = |item: &GraphicsCaptureItem| {
        let handler = Arc::clone(&handler);
        // Asked afresh on every (re)start, so switching Windows HDR on or off mid-session works.
        let hdr = hdr_sdr_white(config.target);
        if let Some(white) = hdr {
            tracing::info!(white, "HDR display: capturing scRGB and tone-mapping to SDR");
        }
        WgcCapture::start(&device, item, config.capture_cursor, hdr, config.fps, move |frame| {
            if let Ok(mut handler) = handler.lock() {
                handler.on_frame(&frame);
            }
        })
    };
    let mut retry_at = Instant::now();
    let mut capture = match try_capture(&item) {
        Ok(capture) => Some(capture),
        Err(SourceError::Protected) => {
            retry_at += backoff.next_delay().to_duration().unwrap_or_default();
            None
        }
        Err(e) => {
            let _ = report.send(Err(e));
            return;
        }
    };
    let _ =
        report.send(Ok(Started { codec, size, device: device.clone(), context: context.clone() }));

    let mut credits = 0u32;
    let mut next_report = Instant::now() + Duration::from_secs(5);
    let mut submitted = 0u64;
    let (mut seen, mut last_frame_at) = (0u64, Instant::now());
    let result = (|| -> Result<(), SourceError> {
        while !shared.stop.load(Ordering::Acquire) {
            if Instant::now() >= next_report {
                next_report += Duration::from_secs(5);
                // SAFETY: plain COM call on a live device.
                let device_ok = unsafe { device.GetDeviceRemovedReason() };
                tracing::info!(
                    wgc_frames = shared.arrived.swap(0, Ordering::Relaxed),
                    submitted = std::mem::take(&mut submitted),
                    dropped = shared.dropped.swap(0, Ordering::Relaxed),
                    capture_open = capture.as_ref().is_some_and(|c| !c.is_closed()),
                    device = ?device_ok,
                    "video health (last 5 s)"
                );
                // A removed or reset GPU never recovers by itself: end the loop so the fault
                // below reaches the watchdog, which rebuilds the device and encoder.
                device_ok.context("D3D11 device removed")?;
            }
            let now_seen = shared.seen.load(Ordering::Relaxed);
            if now_seen != seen {
                (seen, last_frame_at) = (now_seen, Instant::now());
            }
            // Changing the main display can leave WGC open but silent (seen on
            // 2026-10-05: 0 frames for 30 s, capture "open", device fine). A still
            // screen is silent too, so restarting costs it one cheap re-open every
            // few seconds and gets a fresh frame either way.
            // ponytail: stall timer; react to WM_DISPLAYCHANGE once the Phase 2
            // message thread exists.
            if capture.is_some() && last_frame_at.elapsed() >= STALL {
                tracing::debug!("no frames from WGC for {STALL:?}; restarting capture");
                capture = None;
                retry_at = Instant::now();
                last_frame_at = Instant::now();
            }
            if capture.as_ref().is_some_and(WgcCapture::is_closed) {
                // The monitor went away (display switch, Win+P). The last frame keeps
                // repeating until capture reopens, so the stream never stops.
                tracing::warn!("capture target closed; reopening");
                capture = None;
                retry_at = Instant::now();
            }
            if capture.is_none() && Instant::now() >= retry_at {
                // Resolve the target again: after a display change the old item is dead.
                match capture_item(config.target).and_then(|item| try_capture(&item)) {
                    Ok(c) => {
                        tracing::debug!("capture running");
                        capture = Some(c);
                        backoff.reset();
                    }
                    // Protected content, or the display is still settling.
                    Err(e) => {
                        tracing::debug!(%e, "capture not available yet; retrying");
                        retry_at =
                            Instant::now() + backoff.next_delay().to_duration().unwrap_or_default();
                    }
                }
            }
            while let Some(event) = encoder.next_event()? {
                match event {
                    EncoderEvent::NeedInput => credits += 1,
                    EncoderEvent::HaveOutput => {
                        if encoder.take_output(&**sink)? {
                            health(HealthEvent::KeyframeEmitted);
                        }
                    }
                    EncoderEvent::DrainComplete | EncoderEvent::Other => {}
                }
            }
            if let Ok(mut handler) = handler.lock() {
                handler.tick(clock::now());
            }
            // ponytail: a 1 ms poll; switch to IMFAsyncCallback/BeginGetEvent if it
            // shows up in profiles.
            if credits == 0 {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            match frames.recv_timeout(Duration::from_millis(1)) {
                Ok((slot, pts)) => {
                    shared.queued.fetch_sub(1, Ordering::AcqRel);
                    if shared.keyframe.swap(false, Ordering::Relaxed)
                        && let Err(e) = encoder.force_keyframe()
                    {
                        tracing::warn!(%e, "couldn't force a keyframe");
                    }
                    encoder.submit(slot, pts)?;
                    credits -= 1;
                    submitted += 1;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        tracing::error!(%e, "video encoder fault");
        let hresult = match e {
            SourceError::Platform { code, .. } => code,
            _ => 0x8000_4005_u32 as i32, // E_FAIL
        };
        health(HealthEvent::EncoderFault { hresult });
    }
    // Stop capture first so nothing new arrives, then flush what's in the encoder.
    drop(capture);
    encoder.drain(&**sink);
}

/// Everything the encoder thread builds before capture starts.
struct Pipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    codec: Codec,
    /// The encoder's input and output size, pixels.
    size: (u32, u32),
    encoder: HwEncoder,
    handler: Arc<Mutex<FrameHandler>>,
    frames: mpsc::Receiver<(usize, Hns)>,
    item: windows::Graphics::Capture::GraphicsCaptureItem,
}

impl Pipeline {
    fn open(
        config: &VideoStart,
        health: &Emit<HealthEvent>,
        shared: &Arc<Shared>,
    ) -> Result<Self, SourceError> {
        let (device, context) = create_device()?;
        let item = capture_item(config.target)?;
        let size = item
            .Size()
            .map_err(|e| SourceError::Other(format!("GraphicsCaptureItem::Size: {e}")))?;
        let size_in =
            (u32::try_from(size.Width).unwrap_or(0), u32::try_from(size.Height).unwrap_or(0));
        // Even (NV12 and the encoders need it), and scaled down to the chosen resolution with
        // the same aspect ratio; the video processor does the scaling in the same blit.
        let size_out = config.resolution.output_size(size_in);
        if size_out.0 == 0 || size_out.1 == 0 {
            return Err(SourceError::Other(format!(
                "the capture target has no size ({}x{}); is the window minimised?",
                size_in.0, size_in.1
            )));
        }
        let converter = NvConverter::new(&device, size_in, size_out)?;

        // The codec ladder: the preference if the GPU has it, else the best it has;
        // if an encoder lists but won't open (e.g. too wide for H.264), step down.
        let supported = probe(&device);
        let mut codec = Codec::select(&supported, Some(config.codec))
            .ok_or_else(|| SourceError::Unsupported("no hardware video encoder".into()))?;
        let encoder = loop {
            match HwEncoder::new(
                &device,
                &VideoStart { codec, ..*config },
                size_out,
                converter.textures(),
            ) {
                Ok(encoder) => break encoder,
                Err(e) => {
                    let next = std::iter::successors(codec.step_down(), |c| c.step_down())
                        .find(|c| supported.contains(c))
                        .ok_or(e)?;
                    tracing::warn!(
                        from = codec.name(),
                        to = next.name(),
                        "encoder failed; stepping down"
                    );
                    codec = next;
                }
            }
        };

        let (frames_tx, frames) = mpsc::sync_channel(QUEUE_DEPTH);
        let handler = Arc::new(Mutex::new(FrameHandler {
            pacer: FramePacer::new(config.fps),
            converter,
            frames: frames_tx,
            health: Arc::clone(health),
            shared: Arc::clone(shared),
            device: device.clone(),
            context: context.clone(),
            last: None,
            dropping: false,
            last_drop: None,
            warned: None,
            latest: None,
            tonemap: None,
        }));
        Ok(Self { device, context, codec, size: size_out, encoder, handler, frames, item })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No device is created here, so this runs on CI's GPU-less runners.
    #[test]
    fn a_stopped_source_declares_no_format() {
        assert_eq!(GpuVideoSource::new().format(), None);
    }
}
