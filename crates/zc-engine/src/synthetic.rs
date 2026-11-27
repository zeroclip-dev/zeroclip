//! Synthetic sources: the synthetic-load test harness.
//!
//! A [`SyntheticVideoSource`] implements the same [`VideoSource`] port as the real
//! GPU pipeline, but produces numbered [`SyntheticStream`] packets on its own
//! thread — optionally with injected faults (dropped frames, encoder errors). That
//! lets the recording modes and the watchdog be tested deterministically, on any
//! OS, instead of by hoping a real crash happens while you're watching.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use zc_core::media::{Codec, TrackId, TrackInfo, TrackKind};
use zc_core::ports::{Emit, InputSource, OsEvent, SourceError, VideoSource, VideoStart};
use zc_core::sink::SharedSink;
use zc_core::testing::SyntheticStream;
use zc_core::watchdog::HealthEvent;

/// Faults to inject, by frame number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaultPlan {
    /// `(first_frame, count)`: report `count` consecutive `FrameDropped` events
    /// starting at `first_frame`, instead of producing those frames.
    pub dropped_frames: Vec<(u32, u32)>,
    /// Report an `EncoderFault` at this frame, then stop producing until re-initialised.
    pub encoder_fault_at: Option<u32>,
}

/// A fake GPU pipeline producing [`SyntheticStream`] video packets.
#[derive(Debug)]
pub struct SyntheticVideoSource {
    /// The next frame to produce. While running, the worker owns it; `stop` gets it back,
    /// so a restart continues the frame numbers and PTS (as a real QPC clock does).
    stream: SyntheticStream,
    /// Also owned by the worker while running, and returned by `stop`, so an injected fault
    /// that has fired stays fired across a restart.
    faults: FaultPlan,
    /// Pace packets in real time (for manual testing); otherwise as fast as possible.
    realtime: bool,
    /// Set by `request_keyframe`; the worker clears it when it makes the next frame an IDR.
    keyframe_requested: Arc<AtomicBool>,
    /// Tells the worker to exit. Replaced on every `start`.
    stop: Arc<AtomicBool>,
    /// The worker thread, which returns the stream and fault plan when joined.
    worker: Option<JoinHandle<(SyntheticStream, FaultPlan)>>,
}

impl Default for SyntheticVideoSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntheticVideoSource {
    /// A fault-free source, running as fast as possible.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stream: SyntheticStream::new(),
            faults: FaultPlan::default(),
            realtime: false,
            keyframe_requested: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    /// Injects `faults`.
    #[must_use]
    pub fn with_faults(mut self, faults: FaultPlan) -> Self {
        self.faults = faults;
        self
    }

    /// Paces output in real time.
    #[must_use]
    pub fn realtime(mut self, realtime: bool) -> Self {
        self.realtime = realtime;
        self
    }
}

/// The worker's loop: pushes frames into `sink` until `stop` is set or an encoder fault
/// is injected. Returns the stream and fault plan, so the next `start` carries on from where
/// this one stopped.
fn run(
    mut stream: SyntheticStream,
    mut faults: FaultPlan,
    realtime: bool,
    keyframe_requested: &AtomicBool,
    stop: &AtomicBool,
    sink: &SharedSink,
    health: &Emit<HealthEvent>,
) -> (SyntheticStream, FaultPlan) {
    let started = Instant::now();
    let first = stream.now();
    let mut dropping = false;
    while !stop.load(Ordering::Relaxed) {
        let forced = keyframe_requested.swap(false, Ordering::AcqRel);
        if forced {
            stream.force_keyframe();
        }
        let packet = stream.next_video();
        let n = SyntheticStream::sequence_number(&packet.data);
        // `take_if` consumes the fault, so a restarted source doesn't fault again.
        if faults.encoder_fault_at.take_if(|at| *at == n).is_some() {
            health(HealthEvent::EncoderFault { hresult: 0x8000_4005_u32.cast_signed() });
            // Like a dead encoder: stay running, produce nothing, until stopped.
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(1));
            }
            break;
        }
        if realtime {
            // Sleep until this frame's PTS comes round on the wall clock.
            let due = (packet.meta.pts - first).to_duration().unwrap_or_default();
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                thread::sleep(wait);
            }
        }
        if faults.dropped_frames.iter().any(|&(f, c)| (f..f.saturating_add(c)).contains(&n)) {
            health(HealthEvent::FrameDropped);
            dropping = true;
            // A dropped frame can't carry the keyframe; ask again for the next one.
            if forced {
                keyframe_requested.store(true, Ordering::Release);
            }
        } else {
            sink.push(packet.view());
            if forced {
                health(HealthEvent::KeyframeEmitted);
            }
            if dropping {
                // One event when delivery resumes, not one per frame (the actor's inbox must not flood).
                health(HealthEvent::FrameDelivered { interval: packet.meta.duration });
                dropping = false;
            }
        }
        if !realtime {
            // The pusher would otherwise starve the engine thread on the Recorder mutex.
            thread::yield_now();
        }
    }
    (stream, faults)
}

impl VideoSource for SyntheticVideoSource {
    fn start(
        &mut self,
        _config: &VideoStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<Codec, SourceError> {
        if self.worker.is_some() {
            return Err(SourceError::Other("already started".into()));
        }
        self.stop = Arc::new(AtomicBool::new(false));
        let stream = std::mem::take(&mut self.stream);
        let faults = std::mem::take(&mut self.faults);
        let realtime = self.realtime;
        let keyframe_requested = Arc::clone(&self.keyframe_requested);
        let stop = Arc::clone(&self.stop);
        let worker = thread::Builder::new()
            .name("zc-synthetic-video".into())
            .spawn(move || {
                run(stream, faults, realtime, &keyframe_requested, &stop, &sink, &health)
            })
            .map_err(|e| SourceError::Other(e.to_string()))?;
        self.worker = Some(worker);
        Ok(Codec::H264)
    }

    /// Always `Some`: the synthetic format never changes, so there is nothing to wait for.
    fn format(&self) -> Option<TrackInfo> {
        Some(TrackInfo {
            track: TrackId::Video,
            label: "Video".into(),
            kind: TrackKind::Video { codec: Codec::H264, width: 1920, height: 1080, fps: 60 },
            codec_private: Vec::new(),
        })
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stop.store(true, Ordering::Relaxed);
            match worker.join() {
                Ok((stream, faults)) => {
                    self.stream = stream;
                    self.faults = faults;
                }
                Err(_) => tracing::error!("synthetic video thread panicked"),
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    fn screenshot(&mut self, _path: &Path) -> Result<(), SourceError> {
        Err(SourceError::Unsupported("the synthetic source has no pixels".into()))
    }
}

/// A fake OS input thread: the test injects [`OsEvent`]s the way Raw Input and
/// the foreground hook would. Clones share state, so the test keeps one clone and gives the
/// engine another.
#[derive(Clone, Default)]
pub struct SyntheticInput {
    /// The engine's emitter, set by `start` and cleared by `stop`.
    emit: Arc<Mutex<Option<Emit<OsEvent>>>>,
    /// Pids passed to `watch_process`, in order.
    watched: Arc<Mutex<Vec<u32>>>,
}

impl SyntheticInput {
    /// Sends `event` to the engine as if the OS had produced it. Returns `false` if the
    /// source isn't started.
    pub fn inject(&self, event: OsEvent) -> bool {
        // Clone the emitter out, so the lock isn't held while the engine handles the event.
        let emit = self.emit.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match emit {
            Some(emit) => {
                emit(event);
                true
            }
            None => false,
        }
    }

    /// The pids passed to `watch_process` so far, in order.
    #[must_use]
    pub fn watched(&self) -> Vec<u32> {
        self.watched.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl InputSource for SyntheticInput {
    fn start(&mut self, events: Emit<OsEvent>) -> Result<(), SourceError> {
        *self.emit.lock().unwrap_or_else(PoisonError::into_inner) = Some(events);
        Ok(())
    }

    fn stop(&mut self) {
        *self.emit.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn watch_process(&mut self, pid: u32) -> Result<(), SourceError> {
        self.watched.lock().unwrap_or_else(PoisonError::into_inner).push(pid);
        Ok(())
    }
}

// Manual, because the emitter is a closure and has no `Debug`.
impl fmt::Debug for SyntheticInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntheticInput").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    use zc_core::media::PacketRef;
    use zc_core::ports::CaptureTarget;
    use zc_core::sink::PacketSink;
    use zc_core::time::Hns;

    use super::*;

    #[test]
    fn declares_the_fixed_synthetic_video_track() {
        let format = SyntheticVideoSource::new().format().expect("the synthetic format is fixed");
        assert_eq!(format.track, TrackId::Video);
        assert_eq!(
            format.kind,
            TrackKind::Video { codec: Codec::H264, width: 1920, height: 1080, fps: 60 }
        );
        assert!(format.codec_private.is_empty());
    }

    /// Records the sequence number of every frame it receives.
    #[derive(Default)]
    struct Collect(Mutex<Vec<u32>>);

    impl PacketSink for Collect {
        fn push(&self, packet: PacketRef<'_>) {
            let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            seen.push(SyntheticStream::sequence_number(packet.data));
        }
    }

    #[test]
    fn synthetic_source_drops_the_planned_frames() {
        let sink = Arc::new(Collect::default());
        let drops = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&drops);
        let health: Emit<HealthEvent> = Arc::new(move |event| {
            if matches!(event, HealthEvent::FrameDropped) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        let faults = FaultPlan { dropped_frames: vec![(5, 3)], ..FaultPlan::default() };
        let mut source = SyntheticVideoSource::new().with_faults(faults).realtime(true);
        let config = VideoStart {
            target: CaptureTarget::PrimaryMonitor,
            codec: Codec::H264,
            fps: 60,
            min_bitrate_bps: 0,
            max_bitrate_bps: 0,
            keyframe_interval: Hns::from_secs(1),
            capture_cursor: false,
            resolution: Default::default(),
        };

        source.start(&config, sink.clone(), health).expect("the synthetic source starts");
        // Wait until the run is past the dropped frames, not for a fixed time: a busy machine
        // (CI, a parallel build) can starve the worker for longer than any fixed sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while sink.0.lock().unwrap_or_else(PoisonError::into_inner).last().is_none_or(|&n| n < 10)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        source.stop();

        let seen = sink.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let last = *seen.last().expect("frames were delivered");
        assert!(last >= 8, "the run should pass the dropped frames, but stopped at {last}");
        // Frames 5, 6 and 7 are never delivered; every other frame arrives in order, from 0.
        let expected: Vec<u32> = (0..=last).filter(|n| !(5..8).contains(n)).collect();
        assert_eq!(seen, expected);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn request_keyframe_reports_keyframe_emitted_and_stop_returns() {
        let sink = Arc::new(Collect::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let health: Emit<HealthEvent> = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let config = VideoStart {
            target: CaptureTarget::PrimaryMonitor,
            codec: Codec::H264,
            fps: 60,
            min_bitrate_bps: 0,
            max_bitrate_bps: 0,
            keyframe_interval: Hns::from_secs(1),
            capture_cursor: false,
            resolution: Default::default(),
        };
        let mut source = SyntheticVideoSource::new();
        source.start(&config, sink.clone(), health).expect("the synthetic source starts");
        source.request_keyframe();

        // The worker runs flat out, so the event arrives long before the timeout.
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(HealthEvent::KeyframeEmitted) => break,
                Ok(_) => {}
                Err(e) => panic!("KeyframeEmitted never arrived: {e}"),
            }
        }
        // Returns (does not hang) even though the worker is mid-loop.
        source.stop();
        assert!(!sink.0.lock().unwrap_or_else(PoisonError::into_inner).is_empty());
    }
}
