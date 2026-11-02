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
use std::time::{Duration, Instant};

use zc_core::dvr::{DvrBuffer, DvrClip, DvrConfig, DvrError, SnapshotCopy, SnapshotPlan};
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::ports::{LockMemory, SourceError};
use zc_core::sink::{PacketSink, PacketWriter, SinkId, Tee};
use zc_core::time::{Hns, MasterClock};

/// Payload copied per lock hold by [`Recorder::snapshot`]. Each hold stays well under the
/// 5 ms target from D29, so encoder and audio threads never wait long for the lock.
const SNAPSHOT_CHUNK_BYTES: usize = 4 << 20;

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
    /// Page locks on the DVR rings. Declared before `dvr`, so they drop first: fields drop in
    /// declaration order, and a ring must be unpinned before its memory is freed.
    locks: Vec<Box<dyn Send>>,
    /// The last page-lock request, so a resize can pin the new rings the same way (P4.6).
    pin: Option<(LockMemory, Vec<TrackId>)>,
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
                locks: Vec::new(),
                pin: None,
                clock: MasterClock::new(),
                dvr: DvrBuffer::new(config),
                tee: Tee::new(),
                stats: RecorderStats::default(),
            }),
        }
    }

    /// Copies out the last `window` of the DVR buffer (see [`DvrBuffer::snapshot`]).
    ///
    /// The lock is held only to plan the copy and then for each chunk of at most
    /// `SNAPSHOT_CHUNK_BYTES`, so sources keep pushing in between. A packet the writer
    /// laps during the copy is left out, and the clip starts at the next keyframe (D29).
    /// The longest hold and the total time are logged at debug level.
    ///
    /// # Errors
    /// [`DvrError::Empty`] if nothing has been recorded yet, and [`DvrError::Changed`] if
    /// the buffer was cleared or resized during the copy.
    pub fn snapshot(&self, window: Hns) -> Result<DvrClip, DvrError> {
        self.copy_out(|dvr| dvr.plan_snapshot(window))
    }

    /// [`snapshot`](Self::snapshot) for a clip that ends at `end`, cut on every track (see
    /// [`DvrBuffer::plan_snapshot_until`]).
    ///
    /// # Errors
    /// As for [`snapshot`](Self::snapshot).
    pub fn snapshot_until(&self, window: Hns, end: Hns) -> Result<DvrClip, DvrError> {
        self.copy_out(|dvr| dvr.plan_snapshot_until(window, end))
    }

    /// Where a clip cut now would end: the end of the newest video frame. Zero when the buffer
    /// holds no video. A save takes this at the press, so the clip ends there (D30).
    #[must_use]
    pub fn video_end(&self) -> Hns {
        self.lock().dvr.newest_video_end()
    }

    /// Plans a clip under the lock and copies it out in short holds.
    fn copy_out(
        &self,
        make_plan: impl FnOnce(&DvrBuffer) -> Result<SnapshotPlan, DvrError>,
    ) -> Result<DvrClip, DvrError> {
        let started = Instant::now();
        let (plan, held) = self.held(|state| make_plan(&state.dvr));
        let mut longest_hold = held;
        // Allocating (and pre-faulting) the clip's buffers happens outside the lock.
        let mut copy = SnapshotCopy::new(plan?);
        while !copy.is_done() {
            let (result, held) =
                self.held(|state| copy.copy_chunk(&state.dvr, SNAPSHOT_CHUNK_BYTES));
            longest_hold = longest_hold.max(held);
            result?;
        }
        let clip = copy.finish();
        tracing::debug!(
            total = ?started.elapsed(),
            longest_hold = ?longest_hold,
            packets = clip.packets.len(),
            "DVR snapshot copied in short lock holds"
        );
        Ok(clip)
    }

    /// Span of video the DVR buffer holds, for the status bar (see [`DvrBuffer::buffered`]).
    #[must_use]
    pub fn buffered(&self) -> Hns {
        self.lock().dvr.buffered()
    }

    /// Empties the DVR buffer, for when the video format changes or the watchdog resumes after a
    /// re-init: GOPs from the old encoder can't share a clip with the new one (DESIGN §8, "buffer
    /// restarts from empty"). The [`MasterClock`] stays anchored, so the new encoder's packets
    /// carry on the session timeline instead of starting again at zero.
    pub fn clear(&self) {
        self.lock().dvr.clear();
    }

    /// Pins the DVR rings of `tracks` in RAM with `lock` (P3.25, DESIGN §4). Replaces any pins
    /// taken before.
    ///
    /// Only the tracks named are pinned, so pass the video track and the audio tracks that are
    /// running. An unused ring stays unpinned and uncommitted. The call is all or nothing: if
    /// one ring can't be pinned, the rings pinned so far are released too. The state lock is
    /// held while the pages are pinned, so pushes wait for it. The engine calls this at start
    /// and after a video restart, not per packet.
    ///
    /// # Errors
    /// The first region `lock` refuses. Recording goes on, just unpinned.
    pub fn lock_memory(&self, lock: LockMemory, tracks: &[TrackId]) -> Result<usize, SourceError> {
        let mut state = self.lock();
        // Release the old pins before taking new ones, so the same pages are never pinned twice.
        state.locks.clear();
        state.pin = Some((lock, tracks.to_vec()));
        let (guards, bytes) = pin_rings(&state.dvr, lock, tracks)?;
        state.locks = guards;
        Ok(bytes)
    }

    /// Changes the DVR buffer to `config` when a game profile asks for another window (D21). The
    /// lock is held for the whole resize, so pushes wait: that is the brief writer pause. The time
    /// it took is logged, to check it against the watchdog's limit.
    ///
    /// The old rings are freed by the resize, so their page locks are released first, and the new
    /// rings are locked again the way [`lock_memory`](Self::lock_memory) did.
    pub fn resize(&self, config: DvrConfig) {
        let started = Instant::now();
        let mut state = self.lock();
        let State { locks, dvr, pin, .. } = &mut *state;
        locks.clear();
        dvr.resize(config);
        if let Some((lock, tracks)) = pin.as_ref() {
            match pin_rings(dvr, *lock, tracks) {
                Ok((guards, _)) => *locks = guards,
                Err(error) => tracing::warn!(%error, "resized DVR buffer not page-locked"),
            }
        }
        let elapsed = started.elapsed();
        tracing::info!(
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ?config,
            "DVR buffer resized"
        );
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

    /// Runs `f` on the state under the lock. Returns its result and how long the lock was held.
    fn held<T>(&self, f: impl FnOnce(&mut State) -> T) -> (T, Duration) {
        let mut state = self.lock();
        let since = Instant::now();
        let result = f(&mut state);
        (result, since.elapsed())
    }
}

/// Pins the rings of `tracks` with `lock`. All or nothing: on the first refusal the guards taken
/// so far are dropped, which unpins them. Returns the guards and the bytes pinned.
fn pin_rings(
    dvr: &DvrBuffer,
    lock: LockMemory,
    tracks: &[TrackId],
) -> Result<(Vec<Box<dyn Send>>, usize), SourceError> {
    let mut guards = Vec::with_capacity(tracks.len());
    let mut bytes = 0;
    for &track in tracks {
        let region = dvr.memory(track);
        guards.push(lock(region)?);
        bytes += region.len();
    }
    Ok((guards, bytes))
}

impl PacketSink for Recorder {
    fn push(&self, packet: PacketRef<'_>) {
        let mut guard = self.lock();
        let State { clock, dvr, tee, stats, .. } = &mut *guard;

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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use zc_core::media::Packet;
    use zc_core::testing::SyntheticStream;

    use super::*;

    /// Checks a clip the way a player would: it starts on a keyframe, its video runs on
    /// without a gap, and every packet is the one the stream made, unchanged.
    fn assert_clip_is_intact(clip: &DvrClip) {
        let stream = SyntheticStream::new();
        let video: Vec<&Packet> = clip.video().collect();
        assert!(video.first().is_some_and(|p| p.meta.keyframe), "a clip starts on a keyframe");
        for pair in video.windows(2) {
            let (before, after) = (&pair[0].data, &pair[1].data);
            assert_eq!(
                SyntheticStream::sequence_number(after),
                SyntheticStream::sequence_number(before) + 1,
                "no video frame may be missing"
            );
        }
        for packet in &clip.packets {
            let n = SyntheticStream::sequence_number(&packet.data);
            let expected = match packet.meta.track {
                TrackId::Video => stream.frame_pts(n),
                _ => Hns(Hns::from_millis(20).ticks() * i64::from(n)),
            };
            assert_eq!(
                packet.meta.pts, expected,
                "{:?} packet {n} is misplaced",
                packet.meta.track
            );
            // Payload layout: sequence number, track index, then filler that must survive.
            assert_eq!(packet.data[4], packet.meta.track.index() as u8, "packet {n}: track byte");
            assert!(packet.data[5..].iter().all(|&b| b == 0xAB), "packet {n} was overwritten");
        }
        assert!(clip.packets.windows(2).all(|w| w[0].meta.pts <= w[1].meta.pts));
    }

    #[test]
    fn snapshot_clip_stays_intact_while_another_thread_pushes() {
        // Tight rings, so the writer laps the oldest packets of a snapshot now and then.
        let recorder = Recorder::new(DvrConfig { video_bytes: 200_000, audio_bytes: 50_000 });
        let mut stream = SyntheticStream::new();
        for p in stream.take_secs(4) {
            recorder.push(p.view());
        }
        let stop = AtomicBool::new(false);
        let mut clips = 0;
        thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    for p in stream.take_secs(1) {
                        recorder.push(p.view());
                        // Pacing: a writer that never sleeps can starve the snapshot of the lock.
                        thread::sleep(Duration::from_micros(200));
                    }
                }
            });
            for _ in 0..30 {
                match recorder.snapshot(Hns::from_secs(1)) {
                    Ok(clip) => {
                        assert_clip_is_intact(&clip);
                        clips += 1;
                    }
                    // The writer lapped every keyframe in the plan. Nothing to start from.
                    Err(DvrError::Empty) => {}
                    Err(other) => panic!("snapshot failed: {other}"),
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert!(clips > 0, "no snapshot succeeded, so nothing was checked");
    }

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

    /// A page lock that pins nothing. Tests must not pin real memory.
    fn pin(_: &[u8]) -> Result<Box<dyn Send>, SourceError> {
        Ok(Box::new(()))
    }

    /// Refuses the audio rings (1 MB each in the test below) and pins everything else.
    fn refuse_audio(region: &[u8]) -> Result<Box<dyn Send>, SourceError> {
        if region.len() == 1_000_000 {
            Err(SourceError::Other("refused".into()))
        } else {
            Ok(Box::new(()))
        }
    }

    #[test]
    fn lock_memory_pins_only_the_named_tracks() {
        let recorder = Recorder::new(DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 });
        // The DVR reserves six audio rings, but only the named ones are pinned.
        let video = recorder.lock_memory(pin, &[TrackId::Video]).expect("video pins");
        assert_eq!(video, 10_000_000);
        let all = [TrackId::Video, TrackId::GameAudio, TrackId::Mic];
        assert_eq!(recorder.lock_memory(pin, &all).expect("three rings pin"), 12_000_000);
        assert_eq!(recorder.lock().locks.len(), 3);
        // A new call replaces the earlier pins rather than adding to them.
        assert_eq!(recorder.lock_memory(pin, &[]).expect("nothing to pin"), 0);
        assert!(recorder.lock().locks.is_empty());
    }

    #[test]
    fn a_refused_ring_releases_the_rings_pinned_before_it() {
        let recorder = Recorder::new(DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 });
        let tracks = [TrackId::Video, TrackId::GameAudio];
        assert!(recorder.lock_memory(refuse_audio, &tracks).is_err());
        // Video was pinned first. It must be unpinned again, so nothing is half locked.
        assert!(recorder.lock().locks.is_empty());
    }

    #[test]
    fn resize_changes_the_ring_size_and_pins_the_new_rings() {
        let recorder = Recorder::new(DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 });
        recorder.lock_memory(pin, &[TrackId::Video]).expect("video pins");
        recorder.resize(DvrConfig { video_bytes: 20_000_000, audio_bytes: 1_000_000 });
        let state = recorder.lock();
        assert_eq!(state.dvr.memory(TrackId::Video).len(), 20_000_000, "the ring grew");
        assert_eq!(state.locks.len(), 1, "the new ring is pinned again");
    }
}
