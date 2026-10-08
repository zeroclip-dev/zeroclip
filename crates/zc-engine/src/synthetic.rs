//! Synthetic sources: the synthetic-load test harness (DESIGN §12).
//!
//! A [`SyntheticVideoSource`] implements the same [`VideoSource`] port as the real
//! GPU pipeline, but produces numbered [`SyntheticStream`] packets on its own
//! thread — optionally with injected faults (dropped frames, encoder errors). That
//! lets the recording modes and the watchdog be tested deterministically, on any
//! OS, instead of by hoping a real crash happens while you're watching.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use zc_core::media::{Codec, TrackId, TrackInfo, TrackKind};
use zc_core::ports::{Emit, SourceError, VideoSource, VideoStart};
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
    #[expect(dead_code, reason = "scaffold: used once the synthetic source is implemented")]
    stream: SyntheticStream,
    faults: FaultPlan,
    /// Pace packets in real time (for manual testing); otherwise as fast as possible.
    realtime: bool,
    #[expect(dead_code, reason = "scaffold: used once the synthetic source is implemented")]
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
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

impl VideoSource for SyntheticVideoSource {
    fn start(
        &mut self,
        config: &VideoStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<Codec, SourceError> {
        let _ = (config, sink, health);
        todo!(
            "Phase 3: spawn a thread pushing `self.stream` packets into `sink`, honouring `self.faults`"
        )
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
            let _ = worker;
            todo!("Phase 3: signal `self.stop`, then join the worker")
        }
    }

    fn request_keyframe(&mut self) {
        todo!("Phase 4: start a new GOP on the next frame")
    }

    fn screenshot(&mut self, _path: &Path) -> Result<(), SourceError> {
        Err(SourceError::Unsupported("the synthetic source has no pixels".into()))
    }
}

#[cfg(test)]
mod tests {
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
}
