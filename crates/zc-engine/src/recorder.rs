//! The recorder: where every packet lands.
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

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use zc_core::dvr::{DvrBuffer, DvrClip, DvrConfig, DvrError, SnapshotCopy, SnapshotPlan};
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::ports::{LockMemory, SourceError};
use zc_core::sink::{PacketSink, PacketWriter, SinkId, Tee};
use zc_core::time::{Hns, MasterClock};

use crate::disk_dvr::{DiskClip, DiskPlan, DiskRing};

/// Payload copied per lock hold by [`Recorder::snapshot`]. Each hold stays well under the
/// 5 ms target, so encoder and audio threads never wait long for the lock.
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
    /// Failed writes to the DVR ring file (disk mode).
    pub ring_errors: u64,
}

struct State {
    /// Page locks on the DVR rings. Declared before `dvr`, so they drop first: fields drop in
    /// declaration order, and a ring must be unpinned before its memory is freed.
    locks: Vec<Box<dyn Send>>,
    /// The last page-lock request, so a resize can pin the new rings the same way.
    pin: Option<(LockMemory, Vec<TrackId>)>,
    clock: MasterClock,
    /// The whole buffer in RAM mode. In disk mode only the staging area in front of `disk`.
    dvr: DvrBuffer,
    /// The ring file that holds the older footage, in disk mode.
    disk: Option<Arc<DiskRing>>,
    tee: Tee,
    stats: RecorderStats,
}

/// What a clip copies: the staged part from RAM and, in disk mode, the older records from the
/// ring file. Chosen under the lock by [`Recorder::plan_clip`].
pub(crate) struct ClipPlan {
    staged: SnapshotPlan,
    disk: Option<(Arc<DiskRing>, DiskPlan)>,
    /// Where the clip ends, if it isn't at the newest frame.
    until: Option<Hns>,
}

/// A planned clip after its staged part was copied.
pub(crate) enum Copied {
    /// All of it is in RAM: RAM mode, or a window that staging holds.
    Ram(DvrClip),
    /// The older part is still in the ring file and is read as the clip is written.
    Disk(DiskClip),
}

impl Copied {
    /// The whole clip in RAM, reading the ring part in too. For [`Recorder::snapshot`]: a save
    /// writes a [`DiskClip`] as a stream instead, so a long window doesn't fill the RAM.
    pub(crate) fn into_clip(self) -> Result<DvrClip, DvrError> {
        match self {
            Self::Ram(clip) => Ok(clip),
            Self::Disk(clip) => clip.collect(),
        }
    }
}

/// The oldest GOP the ring file doesn't have yet, copied out of the staging area.
pub(crate) enum GopCopy {
    Ready {
        clip: DvrClip,
        end: Hns,
        epoch: u64,
    },
    /// The copy failed (the writer lapped it, or the buffer was cleared). The GOP is skipped.
    Lost {
        end: Hns,
        epoch: u64,
    },
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
                disk: None,
                tee: Tee::new(),
                stats: RecorderStats::default(),
            }),
        }
    }

    /// Copies out the last `window` of the DVR buffer (see [`DvrBuffer::snapshot`]).
    ///
    /// The lock is held only to plan the copy and then for each chunk of at most
    /// `SNAPSHOT_CHUNK_BYTES`, so sources keep pushing in between. A packet the writer
    /// laps during the copy is left out, and the clip starts at the next keyframe.
    /// The longest hold and the total time are logged at debug level.
    ///
    /// # Errors
    /// [`DvrError::Empty`] if nothing has been recorded yet, and [`DvrError::Changed`] if
    /// the buffer was cleared or resized during the copy.
    pub fn snapshot(&self, window: Hns) -> Result<DvrClip, DvrError> {
        self.copy_out(window, None)
    }

    /// [`snapshot`](Self::snapshot) for a clip that ends at `end`, cut on every track (see
    /// [`DvrBuffer::plan_snapshot_until`]).
    ///
    /// # Errors
    /// As for [`snapshot`](Self::snapshot).
    pub fn snapshot_until(&self, window: Hns, end: Hns) -> Result<DvrClip, DvrError> {
        self.copy_out(window, Some(end))
    }

    /// Where a clip cut now would end: the end of the newest video frame. Zero when the buffer
    /// holds no video. A save takes this at the press, so the clip ends there.
    #[must_use]
    pub fn video_end(&self) -> Hns {
        self.lock().dvr.newest_video_end()
    }

    /// Plans a clip under the lock and copies it out in short holds.
    fn copy_out(&self, window: Hns, until: Option<Hns>) -> Result<DvrClip, DvrError> {
        self.copy_plan(self.plan_clip(window, until)?)?.into_clip()
    }

    /// The first half of a copy: decides, under the lock, what the clip is made of.
    pub(crate) fn plan_clip(&self, window: Hns, until: Option<Hns>) -> Result<ClipPlan, DvrError> {
        let (plan, held) = self.held(|state| state.plan_clip(window, until));
        tracing::trace!(?held, "DVR clip planned");
        plan
    }

    /// The second half: copies the staged footage out in short lock holds. That is all of
    /// it in RAM mode. In disk mode the older footage stays in the ring file for the caller to
    /// stream, so this copies at most the staging area, and the ring isn't read at all here.
    /// A packet the writer laps during the copy is left out, and the clip starts at the next
    /// keyframe.
    pub(crate) fn copy_plan(&self, plan: ClipPlan) -> Result<Copied, DvrError> {
        let started = Instant::now();
        let ClipPlan { staged, disk, until } = plan;
        let first_staged = staged.packets.iter().find(|p| p.meta.track == TrackId::Video);
        let first_staged = first_staged.map(|p| p.meta.pts);
        let mut longest_hold = Duration::ZERO;
        // Allocating (and pre-faulting) the clip's buffers happens outside the lock.
        let mut copy = SnapshotCopy::new(staged);
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
        // If the staged part lost its oldest GOP, the ring's footage would end before the clip
        // resumes. Leave it out rather than cut a clip with a hole in it.
        let staged_whole = clip.video().next().map(|p| p.meta.pts) == first_staged;
        Ok(match disk {
            Some((ring, plan)) if staged_whole && !plan.is_empty() => {
                Copied::Disk(DiskClip::new(clip, ring, plan, until))
            }
            _ => Copied::Ram(clip),
        })
    }

    /// Disk mode: the oldest finished GOP at or after `after` that the ring file doesn't have,
    /// once `lag` of newer video has arrived for its audio to catch up. The persister calls this.
    pub(crate) fn next_gop(&self, after: Option<Hns>, lag: Hns) -> Option<GopCopy> {
        let (planned, _) = self.held(|state| {
            let epoch = state.disk.as_ref()?.epoch();
            let mut starts = state.dvr.gop_starts().filter(|&s| after.is_none_or(|a| s >= a));
            let (start, end) = (starts.next()?, starts.next()?);
            (state.dvr.newest_video_end() >= end + lag).then_some(())?;
            // The window counted back from `end` reaches exactly to the keyframe at `start`.
            let plan = state.dvr.plan_snapshot_until(end - start, end).ok()?;
            Some((plan, start, end, epoch))
        });
        let (plan, start, end, epoch) = planned?;
        let mut copy = SnapshotCopy::new(plan);
        let mut whole = true;
        while !copy.is_done() {
            let (result, _) = self.held(|state| copy.copy_chunk(&state.dvr, SNAPSHOT_CHUNK_BYTES));
            if result.is_err() {
                whole = false;
                break;
            }
        }
        let clip = copy.finish();
        let intact = whole && clip.video().next().is_some_and(|p| p.meta.pts == start);
        Some(if intact {
            GopCopy::Ready { clip, end, epoch }
        } else {
            GopCopy::Lost { end, epoch }
        })
    }

    /// Switches to disk mode: `ring` holds the footage older than the staging area.
    /// [`Persister::start`](crate::disk_dvr::Persister::start) calls this and starts the thread
    /// that fills the ring.
    pub(crate) fn attach_ring(&self, ring: Arc<DiskRing>) {
        self.lock().disk = Some(ring);
    }

    /// Grows the ring file in disk mode to `bytes`, if it is smaller (see [`DiskRing::grow`]).
    /// Returns whether it grew. Does nothing in RAM mode.
    ///
    /// # Errors
    /// The file couldn't be resized. The old size stays.
    pub fn grow_ring(&self, bytes: u64) -> std::io::Result<bool> {
        let ring = self.lock().disk.clone();
        ring.map_or(Ok(false), |ring| ring.grow(bytes))
    }

    /// Span of video the DVR buffer holds, for the status bar (see [`DvrBuffer::buffered`]). In
    /// disk mode it counts from the oldest record in the ring, not just the staging area.
    #[must_use]
    pub fn buffered(&self) -> Hns {
        let state = self.lock();
        let staged = state.dvr.buffered();
        // Recorder lock, then the ring's index lock: the order `clear` and `next_gop` take them.
        match state.disk.as_ref().and_then(|ring| ring.oldest_start()) {
            Some(oldest) => staged.max(state.dvr.newest_video_end().saturating_sub(oldest)),
            None => staged,
        }
    }

    /// Empties the DVR buffer, for when the video format changes or the watchdog resumes after a
    /// re-init: GOPs from the old encoder can't share a clip with the new one. The [`MasterClock`] stays anchored, so the new encoder's packets
    /// carry on the session timeline instead of starting again at zero.
    pub fn clear(&self) {
        let mut state = self.lock();
        state.dvr.clear();
        // Under the same lock, so the persister can't copy fresh footage and then lose it.
        if let Some(ring) = &state.disk {
            ring.clear();
        }
    }

    /// Pins the DVR rings of `tracks` in RAM with `lock`. Replaces any pins
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

    /// Changes the DVR buffer to `config` when a game profile asks for another window. The
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

    /// Capture was stopped with the buffer kept (the game lost focus). Packets after the restart
    /// continue the timeline from the end of the footage, so the pause leaves no hole in a clip
    /// and doesn't use up the DVR window. Every track shares the one offset, so they stay aligned.
    /// Call it once the sources have stopped.
    pub fn pause(&self) {
        let mut state = self.lock();
        let end = state.dvr.newest_video_end();
        state.clock.pause(end);
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
        let state = self.lock();
        let ring_errors = state.disk.as_ref().map_or(0, |ring| ring.write_errors());
        RecorderStats { ring_errors, ..state.stats }
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

impl State {
    /// In RAM mode this is the plain DVR plan. In disk mode, a window that the staging area holds
    /// is planned the same way. A longer one takes everything staged, plus the ring records back
    /// to the keyframe that covers the window.
    fn plan_clip(&self, window: Hns, until: Option<Hns>) -> Result<ClipPlan, DvrError> {
        let plan = |window| match until {
            Some(end) => self.dvr.plan_snapshot_until(window, end),
            None => self.dvr.plan_snapshot(window),
        };
        let Some(ring) = &self.disk else {
            return Ok(ClipPlan { staged: plan(window)?, disk: None, until });
        };
        let oldest = self.dvr.oldest_pts().ok_or(DvrError::Empty)?;
        let end = until.unwrap_or_else(|| self.dvr.newest_video_end());
        let target = end.saturating_sub(window);
        if oldest <= target {
            return Ok(ClipPlan { staged: plan(window)?, disk: None, until });
        }
        // `i64::MAX` reaches back to the oldest staged GOP, as `DvrBuffer::resize` does.
        let disk = ring.plan(oldest, target);
        Ok(ClipPlan { staged: plan(Hns(i64::MAX))?, disk: Some((Arc::clone(ring), disk)), until })
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
        if clock.is_paused() {
            // Capture restarted after a pause: the timeline can only resume on a video keyframe,
            // so nothing else (a stray audio buffer, a delta frame) gets a PTS before it.
            if meta.track == TrackId::Video && meta.keyframe {
                clock.resume(meta.pts);
            } else {
                stats.early_packets += 1;
                return;
            }
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
    fn a_pause_leaves_no_hole_and_keeps_audio_aligned_with_video() {
        let recorder = Recorder::new(DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 });
        let mut stream = SyntheticStream::new();
        for p in stream.take_secs(3) {
            recorder.push(p.view());
        }
        recorder.pause();
        // Capture restarts 50 s later: the same stream, on a clock that kept running. Audio that
        // arrives before the first keyframe is dropped, like the real sources' first buffers.
        let gap = Hns::from_secs(50);
        for mut p in stream.take_secs(3) {
            p.meta.pts += gap;
            recorder.push(p.view());
        }
        let clip = recorder.snapshot(Hns::from_secs(10)).expect("clip");
        let video: Vec<Hns> = clip.video().map(|p| p.meta.pts).collect();
        assert_eq!(video.len(), 360);
        // Video runs on without a step, even across the pause.
        let step = SyntheticStream::new().frame_pts(1);
        // (A frame's PTS is rounded, so a step may differ by a tick.)
        assert!(
            video.windows(2).all(|w| (w[1] - w[0] - step).ticks().abs() <= 1),
            "a hole where capture was paused"
        );
        // Audio carries the same offset: its packets stay inside the video's span.
        let (first, last) = (video[0], clip.video().last().unwrap().meta.end());
        assert!(clip.packets.iter().all(|p| p.meta.pts >= first && p.meta.pts <= last));
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
