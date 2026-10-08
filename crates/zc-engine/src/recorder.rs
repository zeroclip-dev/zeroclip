//! The recorder: where every packet lands (DESIGN §4, §5).
//!
//! Sources push packets through the [`PacketSink`] impl. Under one short lock the
//! recorder:
//!
//! 1. anchors the [`MasterClock`] on the first video keyframe and maps every
//!    packet's raw capture timestamp onto the session timeline;
//! 2. copies the packet into the [`DvrBuffer`];
//! 3. fans it out through the [`Tee`] to any active disk writers (manual or
//!    session recording).
//!
//! Nothing in here may allocate or touch the disk: the DVR buffer is
//! pre-allocated, and disk writers only copy into the recording spool (the IO
//! thread does the actual writes).

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use zc_core::dvr::{DvrBuffer, DvrClip, DvrConfig, DvrError};
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::sink::{PacketSink, PacketWriter, SinkId, Tee};
use zc_core::time::{Hns, MasterClock};

/// Counters for things that went wrong on the hot path. Read by the engine and
/// turned into health events / UI warnings off the hot path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecorderStats {
    /// Packets dropped because they predate the session or the clock isn't anchored yet.
    pub early_packets: u64,
    /// Packets the DVR buffer rejected.
    pub dvr_errors: u64,
    /// Failed writes to disk writers.
    pub writer_errors: u64,
}

struct State {
    clock: MasterClock,
    dvr: DvrBuffer,
    tee: Tee,
    stats: RecorderStats,
}

/// Owns the DVR buffer, the master clock and the disk-writer tee.
pub struct Recorder {
    state: Mutex<State>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

impl Recorder {
    /// A recorder with a DVR buffer of `config` (allocated here, up front).
    #[must_use]
    pub fn new(config: DvrConfig) -> Self {
        Self {
            state: Mutex::new(State {
                clock: MasterClock::new(),
                dvr: DvrBuffer::new(config),
                tee: Tee::new(),
                stats: RecorderStats::default(),
            }),
        }
    }

    /// Copies out the last `window` of the DVR buffer (see [`DvrBuffer::snapshot`]).
    ///
    /// The copy runs under the recorder lock, so every source waits for it. Its
    /// duration is logged at debug level (D19 asks for this measurement).
    ///
    /// # Errors
    /// [`DvrError::Empty`] if nothing has been recorded yet.
    pub fn snapshot(&self, window: Hns) -> Result<DvrClip, DvrError> {
        let started = Instant::now();
        let result = self.lock().dvr.snapshot(window);
        // The guard was a temporary of the statement above, so the lock is free by now.
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis(),
            packets = result.as_ref().map_or(0, |clip| clip.packets.len()),
            "DVR snapshot copied (recorder lock held)"
        );
        result
    }

    /// Span of video the DVR buffer holds, for the status bar (see [`DvrBuffer::buffered`]).
    #[must_use]
    pub fn buffered(&self) -> Hns {
        self.lock().dvr.buffered()
    }

    /// Starts feeding a disk writer (manual or session recording).
    pub fn attach(&self, writer: Box<dyn PacketWriter>) -> SinkId {
        self.lock().tee.attach(writer)
    }

    /// Stops feeding a disk writer and hands it back for finalising.
    pub fn detach(&self, id: SinkId) -> Option<Box<dyn PacketWriter>> {
        self.lock().tee.detach(id)
    }

    /// Hot-path counters.
    #[must_use]
    pub fn stats(&self) -> RecorderStats {
        self.lock().stats
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock leaves the state usable (worst case: one
        // half-written packet in a ring), so recover instead of cascading the panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PacketSink for Recorder {
    fn push(&self, packet: PacketRef<'_>) {
        let mut guard = self.lock();
        let State { clock, dvr, tee, stats } = &mut *guard;

        let meta = packet.meta;
        if meta.track == TrackId::Video && meta.keyframe && !clock.is_anchored() {
            clock.anchor(meta.pts);
        }
        let Some(pts) = clock.to_pts(meta.track, meta.pts) else {
            stats.early_packets += 1;
            return;
        };
        let packet = PacketRef { meta: PacketMeta { pts, ..meta }, data: packet.data };

        if dvr.push(packet).is_err() {
            stats.dvr_errors += 1;
        }
        tee.write(packet, |_, _| stats.writer_errors += 1);
    }
}

#[cfg(test)]
mod tests {
    use zc_core::testing::SyntheticStream;

    use super::*;

    #[test]
    fn recorder_maps_capture_time_onto_the_session_timeline() {
        // Sources stamp packets with raw QPC-based capture time; the session starts
        // at the first video keyframe.
        let recorder = Recorder::new(DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 });
        let offset = Hns::from_secs(5_000); // machine uptime at capture start
        for mut p in SyntheticStream::new().take_secs(3) {
            p.meta.pts += offset;
            recorder.push(p.view());
        }
        let clip = recorder.snapshot(Hns::from_secs(60)).unwrap();
        assert_eq!(clip.video().next().unwrap().meta.pts, Hns::ZERO);
        assert_eq!(clip.duration(), Hns::from_secs(3));
        assert_eq!(recorder.stats(), RecorderStats::default());
    }
}
