//! The retrospective DVR buffer: "save the last N minutes" (DESIGN §4, §5, §6).
//!
//! # Rules this buffer must follow
//!
//! 1. **Pre-allocated, allocation-free pushes.** All storage is reserved in
//!    [`DvrBuffer::new`]. After that, [`push`](DvrBuffer::push) only copies bytes
//!    into existing memory; it runs on encoder threads during gameplay.
//! 2. **GOP-aware eviction.** A clip must start at an IDR frame or it won't decode.
//!    The video ring's per-frame index *is* the GOP index: keyframe entries mark the
//!    GOP starts, and eviction removes whole GOPs from the front, never single frames.
//! 3. **Audio follows video.** Each audio track has its own ring. When a GOP is
//!    evicted, audio older than the new oldest GOP's start PTS goes with it, so a
//!    snapshot never contains audio without matching video (and DVR clips are
//!    never silent).
//! 4. **Snapshots start at a keyframe.** [`snapshot`](DvrBuffer::snapshot) walks back
//!    from the newest GOP until the requested window is covered, then starts at
//!    that GOP's IDR.
//!
//! The buffer lives in system RAM, never VRAM (DESIGN §4, "Why RAM, not VRAM"), and
//! the engine page-locks it (`VirtualLock`) after allocation. Size it with
//! [`DvrConfig::for_window`] against the *maximum* bitrate.

use crate::media::{Packet, PacketMeta, PacketRef, TrackId};
use crate::ring::PacketRing;
use crate::time::Hns;

/// How much memory the buffer reserves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvrConfig {
    /// Bytes reserved for encoded video.
    pub video_bytes: usize,
    /// Bytes reserved for *each* audio track.
    pub audio_bytes: usize,
}

impl DvrConfig {
    /// Extra footage reserved on top of the window. At most 2 s, so a full window still
    /// covers its oldest whole GOP after eviction (GOPs are 1 s by default).
    const HEADROOM: Hns = Hns::from_secs(2);

    /// Sizes the buffer to hold `window` of footage at the worst-case bitrates.
    ///
    /// The ring must fit the *maximum* VBR bitrate (DESIGN §4): 20 Mbps is 2.5 MB/s,
    /// so one minute needs ~150 MB. A little headroom (at most ~2 s) is fine and lets
    /// a full window survive whole-GOP eviction.
    #[must_use]
    pub fn for_window(window: Hns, max_video_bitrate_bps: u64, audio_bitrate_bps: u64) -> Self {
        let ticks = u128::try_from(window.ticks().max(0).saturating_add(Self::HEADROOM.ticks()))
            .unwrap_or(0);
        let ticks_per_second = u128::from(Hns::PER_SECOND.unsigned_abs());
        // Bits per second × seconds, then ÷ 8 for bytes. Widened to u128, so it can't overflow.
        let bytes = |bits_per_second: u64| {
            usize::try_from(ticks * u128::from(bits_per_second) / 8 / ticks_per_second)
                .unwrap_or(usize::MAX)
        };
        Self { video_bytes: bytes(max_video_bitrate_bps), audio_bytes: bytes(audio_bitrate_bps) }
    }
}

/// What [`DvrBuffer::push`] did with a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// Copied into the buffer.
    Stored,
    /// Deliberately dropped: video before the first keyframe (a clip can't start
    /// mid-GOP), or audio with no retained video to align to.
    Discarded,
}

/// Why the buffer couldn't do what was asked.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DvrError {
    /// A single GOP is larger than the whole video ring, so it can never be stored.
    /// Usually means the buffer is sized for a lower bitrate than the encoder produces.
    #[error("a {bytes}-byte GOP can never fit in a {capacity}-byte buffer")]
    GopTooLarge {
        /// Size of the offending GOP so far.
        bytes: usize,
        /// Video ring capacity.
        capacity: usize,
    },
    /// There's no video in the buffer yet.
    #[error("the DVR buffer is empty")]
    Empty,
    /// The buffer was cleared or resized while a snapshot was being copied.
    #[error("the buffer changed while saving; try again")]
    Changed,
}

/// Index of the video ring in [`DvrBuffer::rings`]. Audio rings follow it.
const VIDEO_RING: usize = TrackId::Video.index();
/// Bytes of video ring per index entry. Frames average at least 1 KiB at 2 Mbps and 240 fps.
// ponytail: a fixed entry size, not a computed one. Tiny frames fill the index before the
// bytes, so eviction starts early. Upgrade: size the index from the bitrate.
const VIDEO_BYTES_PER_ENTRY: usize = 1024;
/// Bytes of audio ring per index entry. MF's AAC encoder is CBR, so frames are at least 170 B.
// ponytail: same fixed-entry shortcut as video. CBR audio never fills its index first.
const AUDIO_BYTES_PER_ENTRY: usize = 128;
/// Smallest index, so a small ring still holds a 1 s GOP at 240 fps with room to spare.
// ponytail: a floor, not derived from the frame rate. A GOP with more frames than this is
// refused with GopTooLarge. Upgrade: derive it from the GOP length.
const MIN_ENTRIES: usize = 1024;

/// The retrospective ring buffer. See the [module docs](self) for the rules it follows.
#[derive(Debug)]
pub struct DvrBuffer {
    config: DvrConfig,
    /// One ring per track, indexed by [`TrackId::index`]: video first, then the audio tracks.
    rings: [PacketRing; TrackId::COUNT],
    /// Video GOPs held. Each stored keyframe starts one.
    gops: usize,
    /// Video bytes in the GOP being written, so an oversized GOP is refused early.
    current_gop_bytes: usize,
    /// End of the newest stored video frame. [`buffered`](Self::buffered) measures to here.
    newest_video_end: Hns,
    /// Set at creation, after a refused GOP and after `clear`: drop video until a keyframe.
    skip_until_keyframe: bool,
    /// Bumped by `clear` and `resize`. A plan from an older generation holds positions that
    /// no longer mean anything (see [`SnapshotCopy`]).
    generation: u64,
}

impl DvrBuffer {
    /// Allocates all storage up front. After this returns, `push` must not allocate.
    #[must_use]
    pub fn new(config: DvrConfig) -> Self {
        let rings = TrackId::ALL.map(|track| {
            let (bytes, bytes_per_entry) = if track.is_audio() {
                (config.audio_bytes, AUDIO_BYTES_PER_ENTRY)
            } else {
                (config.video_bytes, VIDEO_BYTES_PER_ENTRY)
            };
            PacketRing::new(bytes, (bytes / bytes_per_entry).max(MIN_ENTRIES))
        });
        Self {
            config,
            rings,
            gops: 0,
            current_gop_bytes: 0,
            newest_video_end: Hns::ZERO,
            skip_until_keyframe: true,
            generation: 0,
        }
    }

    /// The configuration the buffer was allocated with.
    #[must_use]
    pub fn config(&self) -> DvrConfig {
        self.config
    }

    /// Stores one packet, evicting whole GOPs (and the audio that goes with them)
    /// as needed to make room.
    ///
    /// # Errors
    /// [`DvrError::GopTooLarge`] if the current GOP alone exceeds the video ring.
    pub fn push(&mut self, packet: PacketRef<'_>) -> Result<PushOutcome, DvrError> {
        // A zero-sized video ring means DVR is off in settings, so nothing is kept.
        if self.rings[VIDEO_RING].capacity() == 0 {
            return Ok(PushOutcome::Discarded);
        }
        match packet.meta.track {
            TrackId::Video => self.push_video(packet),
            TrackId::Audio(_) => Ok(self.push_audio(packet)),
        }
    }

    /// Number of GOPs held, including the one currently being written.
    #[must_use]
    pub fn gop_count(&self) -> usize {
        self.gops
    }

    /// Start PTS of the oldest retained GOP, or `None` if empty.
    #[must_use]
    pub fn oldest_pts(&self) -> Option<Hns> {
        self.rings[VIDEO_RING].front().map(|meta| meta.pts)
    }

    /// Span of video currently held: from the oldest GOP's start to the end of the newest frame.
    #[must_use]
    pub fn buffered(&self) -> Hns {
        self.oldest_pts().map_or(Hns::ZERO, |oldest| self.newest_video_end.saturating_sub(oldest))
    }

    /// Bytes currently used by `track`.
    #[must_use]
    pub fn used_bytes(&self, track: TrackId) -> usize {
        self.rings[track.index()].used()
    }

    /// Copies out the last `window` of footage: video from the IDR of the newest GOP
    /// that covers the window, plus all audio from that point on.
    ///
    /// If less than `window` is buffered, returns everything.
    ///
    /// This is [`plan_snapshot`](Self::plan_snapshot) and a [`SnapshotCopy`] run in one go,
    /// so the whole copy happens here. The engine copies in short holds instead (D29).
    ///
    /// # Errors
    /// [`DvrError::Empty`] if there's no video yet.
    pub fn snapshot(&self, window: Hns) -> Result<DvrClip, DvrError> {
        let mut copy = SnapshotCopy::new(self.plan_snapshot(window)?);
        // One chunk holds the whole clip. Nothing can change while `&self` is borrowed, so
        // the loop runs once.
        while !copy.is_done() {
            copy.copy_chunk(self, usize::MAX)?;
        }
        Ok(copy.finish())
    }

    /// Decides what [`snapshot`](Self::snapshot) copies for `window`, without copying any
    /// bytes. This is quick, so the engine can call it under the lock.
    ///
    /// # Errors
    /// [`DvrError::Empty`] if there's no video yet.
    pub fn plan_snapshot(&self, window: Hns) -> Result<SnapshotPlan, DvrError> {
        let oldest = self.oldest_pts().ok_or(DvrError::Empty)?;
        let target = self.newest_video_end.saturating_sub(window);
        // The newest GOP that starts at or before `target` covers the window. If none does,
        // the buffer holds less than `window`, so start at the oldest GOP.
        let start = self.rings[VIDEO_RING]
            .positions()
            .rev()
            .find(|(meta, _, _)| meta.keyframe && meta.pts <= target)
            .map_or(oldest, |(meta, _, _)| meta.pts);

        let mut packets = Vec::with_capacity(self.rings.iter().map(PacketRing::len).sum());
        for ring in &self.rings {
            for (meta, abs, len) in ring.positions() {
                if meta.pts >= start {
                    packets.push(PlannedPacket { meta, abs, len });
                }
            }
        }
        Ok(SnapshotPlan { generation: self.generation, packets })
    }

    /// Copies each planned packet's bytes into the matching pre-sized packet in `out`, in
    /// order, and stops at the first one the writer has overwritten. Call it under the lock.
    fn copy_planned(
        &self,
        generation: u64,
        planned: &[PlannedPacket],
        out: &mut [Packet],
    ) -> CopyOutcome {
        debug_assert_eq!(planned.len(), out.len());
        if generation != self.generation {
            return CopyOutcome::Stale;
        }
        for (index, (plan, packet)) in planned.iter().zip(out.iter_mut()).enumerate() {
            let ring = &self.rings[plan.meta.track.index()];
            if !ring.copy_if_intact(plan.abs, plan.len, &mut packet.data) {
                return CopyOutcome::Overwritten { index };
            }
        }
        CopyOutcome::Copied
    }

    /// Changes capacity when a game profile asks for a different window (DESIGN §7).
    /// Growing keeps everything; shrinking evicts the oldest GOPs first. The engine
    /// pauses the writer around this call, so it may allocate.
    ///
    /// It copies the buffer out, rebuilds it at the new size and pushes everything back,
    /// so it needs about twice the window's memory for a moment.
    pub fn resize(&mut self, config: DvrConfig) {
        let was_skipping = self.skip_until_keyframe;
        let generation = self.generation + 1;
        // `Hns(i64::MAX)` reaches back past the oldest GOP, so this copies everything.
        let clip = self.snapshot(Hns(i64::MAX)).unwrap_or_default();
        // Free the old rings before allocating the new ones, so the peak is the copy plus
        // one buffer, not three.
        *self = Self::new(DvrConfig { video_bytes: 0, audio_bytes: 0 });
        *self = Self::new(config);
        self.generation = generation;
        for packet in &clip.packets {
            // Whatever the new size can't hold is evicted or refused, as on any push.
            let _ = self.push(packet.view());
        }
        // A refused GOP must stay refused until its keyframe, or the rest of it would be stored
        // after a gap.
        self.skip_until_keyframe |= was_skipping;
    }

    /// Drops everything. The next stored packet must again be a keyframe.
    /// Used after a watchdog re-init (DESIGN §8: "buffer restarts from empty").
    pub fn clear(&mut self) {
        for ring in &mut self.rings {
            ring.clear();
        }
        self.gops = 0;
        self.current_gop_bytes = 0;
        self.newest_video_end = Hns::ZERO;
        self.skip_until_keyframe = true;
        self.generation += 1;
    }

    /// The video half of [`push`](Self::push). Keeps GOPs whole and starts at a keyframe.
    fn push_video(&mut self, packet: PacketRef<'_>) -> Result<PushOutcome, DvrError> {
        let meta = packet.meta;
        let len = packet.data.len();
        if meta.keyframe {
            self.skip_until_keyframe = false;
            self.current_gop_bytes = 0;
        } else if self.skip_until_keyframe {
            return Ok(PushOutcome::Discarded);
        }

        let capacity = self.rings[VIDEO_RING].capacity();
        let gop_bytes = self.current_gop_bytes + len;
        if gop_bytes > capacity {
            // This GOP can never fit. Drop the rest of it; the next keyframe starts afresh.
            self.skip_until_keyframe = true;
            return Err(DvrError::GopTooLarge { bytes: gop_bytes, capacity });
        }

        while !self.rings[VIDEO_RING].has_room_for(len) {
            // A keyframe may evict every older GOP. Any other frame must leave the GOP
            // being written alone, so it can only evict down to one GOP.
            let evictable = if meta.keyframe { self.gops } else { self.gops.saturating_sub(1) };
            if evictable == 0 {
                // The index is full inside one GOP: too many frames, not too many bytes.
                self.skip_until_keyframe = true;
                return Err(DvrError::GopTooLarge { bytes: gop_bytes, capacity });
            }
            self.evict_oldest_gop();
        }

        let stored = self.rings[VIDEO_RING].push(packet);
        debug_assert!(stored, "the eviction loop made room for the packet");
        if meta.keyframe {
            self.gops += 1;
        }
        self.current_gop_bytes = gop_bytes;
        self.newest_video_end = meta.end();
        self.trim_audio();
        Ok(PushOutcome::Stored)
    }

    /// The audio half of [`push`](Self::push). Audio is kept only if it starts at or after
    /// the oldest retained video frame. When its own ring is full, its oldest packets go.
    fn push_audio(&mut self, packet: PacketRef<'_>) -> PushOutcome {
        let oldest = self.oldest_pts();
        let len = packet.data.len();
        let ring = &mut self.rings[packet.meta.track.index()];
        // A packet bigger than the whole ring is refused up front, rather than emptying the ring.
        if oldest.is_none_or(|start| packet.meta.pts < start) || len > ring.capacity() {
            return PushOutcome::Discarded;
        }
        while !ring.has_room_for(len) {
            if ring.pop_front().is_none() {
                return PushOutcome::Discarded;
            }
        }
        let stored = ring.push(packet);
        debug_assert!(stored, "the eviction loop made room for the packet");
        PushOutcome::Stored
    }

    /// Drops the oldest GOP: its keyframe, then every frame up to the next keyframe.
    fn evict_oldest_gop(&mut self) {
        let ring = &mut self.rings[VIDEO_RING];
        ring.pop_front();
        while ring.front().is_some_and(|meta| !meta.keyframe) {
            ring.pop_front();
        }
        self.gops -= 1;
    }

    /// Drops audio that starts before the oldest retained video frame, so every kept audio
    /// packet has video to play against. With no video retained, all audio goes.
    fn trim_audio(&mut self) {
        let oldest = self.oldest_pts();
        for ring in &mut self.rings[VIDEO_RING + 1..] {
            while ring.front().is_some_and(|meta| oldest.is_none_or(|o| meta.pts < o)) {
                ring.pop_front();
            }
        }
    }
}

/// An owned copy of a DVR window, ready to be written to a file off the hot path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DvrClip {
    /// All packets of all tracks, in presentation order. For equal PTS, video comes first.
    pub packets: Vec<Packet>,
}

impl DvrClip {
    /// Packets of one track, in order.
    pub fn track(&self, track: TrackId) -> impl Iterator<Item = &Packet> + '_ {
        self.packets.iter().filter(move |p| p.meta.track == track)
    }

    /// Video packets, in order.
    pub fn video(&self) -> impl Iterator<Item = &Packet> + '_ {
        self.track(TrackId::Video)
    }

    /// From the first video packet's start to the last video packet's end.
    #[must_use]
    pub fn duration(&self) -> Hns {
        let mut video = self.video();
        match (video.next(), video.last()) {
            (Some(first), Some(last)) => last.meta.end() - first.meta.pts,
            (Some(only), None) => only.meta.duration,
            _ => Hns::ZERO,
        }
    }
}

/// One packet that a snapshot will copy: its metadata, and where its payload sits in the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedPacket {
    /// Timing and track. The track also names the ring that holds the payload.
    pub meta: PacketMeta,
    /// Absolute byte position of the payload in that ring (see [`PacketRing`]).
    pub abs: u64,
    /// Payload length in bytes.
    pub len: usize,
}

/// What a snapshot copies, chosen under the lock by [`DvrBuffer::plan_snapshot`]: metadata
/// and byte positions only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotPlan {
    /// The buffer's generation when the plan was made. A copy from another generation is refused.
    pub generation: u64,
    /// Video from the start keyframe on, then each audio track. Each track's packets are
    /// oldest first.
    pub packets: Vec<PlannedPacket>,
}

/// How far [`DvrBuffer::copy_planned`] got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyOutcome {
    /// Every packet in the slice was copied.
    Copied,
    /// The packet at `index` in the slice was overwritten. Those before it were copied.
    Overwritten { index: usize },
    /// The buffer was cleared or resized since planning.
    Stale,
}

/// A snapshot copy spread over several short lock holds (D29).
///
/// Plan under the lock with [`DvrBuffer::plan_snapshot`], then call [`copy_chunk`](Self::copy_chunk)
/// once per hold, releasing the lock between calls. Pushes may happen in between. The writer
/// can lap a planned packet before it is copied. The copy then gives that packet up, and the
/// clip starts at the next keyframe instead. Torn bytes are never copied.
pub struct SnapshotCopy {
    plan: SnapshotPlan,
    /// One pre-sized packet per planned packet, in plan order.
    packets: Vec<Packet>,
    /// Index of the first planned packet not yet copied or given up on.
    next: usize,
    /// Packets older than this are left out of the clip. It rises past each lost packet.
    floor: Hns,
}

impl SnapshotCopy {
    /// Allocates the clip's packets, zero-filled so that every page is touched now. Call it
    /// outside the lock.
    #[must_use]
    pub fn new(plan: SnapshotPlan) -> Self {
        let packets = plan
            .packets
            .iter()
            .map(|p| {
                // `vec![0; n]` may hand back untouched zero pages, so write the zeros instead.
                #[expect(clippy::slow_vector_initialization, reason = "the writes fault pages in")]
                let mut data = Vec::with_capacity(p.len);
                data.resize(p.len, 0);
                Packet { meta: p.meta, data }
            })
            .collect();
        Self { plan, packets, next: 0, floor: Hns(i64::MIN) }
    }

    /// Whether every planned packet has been copied or given up on.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.next >= self.plan.packets.len()
    }

    /// Copies the next packets from `buffer`: at most `max_bytes` of payload, and at least one
    /// packet. Call it under the lock, once per hold.
    ///
    /// # Errors
    /// [`DvrError::Changed`] if the buffer was cleared or resized since planning.
    /// [`DvrError::Empty`] if the writer lapped the planned packets and no keyframe is left
    /// to start from.
    pub fn copy_chunk(&mut self, buffer: &DvrBuffer, max_bytes: usize) -> Result<(), DvrError> {
        let end = self.next + chunk_len(&self.plan.packets[self.next..], max_bytes);
        let outcome = buffer.copy_planned(
            self.plan.generation,
            &self.plan.packets[self.next..end],
            &mut self.packets[self.next..end],
        );
        match outcome {
            CopyOutcome::Copied => self.next = end,
            CopyOutcome::Stale => return Err(DvrError::Changed),
            CopyOutcome::Overwritten { index } => {
                // The lost packet and everything before the next keyframe after it go: that GOP
                // is broken, and audio must not outlive the video it plays against.
                let lost = self.next + index;
                let lost_pts = self.plan.packets[lost].meta.pts;
                let key = self
                    .plan
                    .packets
                    .iter()
                    .find(|p| {
                        p.meta.track == TrackId::Video && p.meta.keyframe && p.meta.pts > lost_pts
                    })
                    .ok_or(DvrError::Empty)?;
                self.floor = self.floor.max(key.meta.pts);
                self.next = lost + 1;
            }
        }
        Ok(())
    }

    /// The clip: every copied packet at or after the floor, in presentation order. Call it
    /// once [`is_done`](Self::is_done).
    #[must_use]
    pub fn finish(mut self) -> DvrClip {
        let floor = self.floor;
        self.packets.retain(|p| p.meta.pts >= floor);
        // Stable sort, and TrackId orders video first, so equal PTS keep video ahead of audio.
        self.packets.sort_by_key(|p| (p.meta.pts, p.meta.track));
        DvrClip { packets: self.packets }
    }
}

/// How many packets, from the front of `planned`, fit in one lock hold: as many as keep the
/// payload within `max_bytes`, and always at least one.
fn chunk_len(planned: &[PlannedPacket], max_bytes: usize) -> usize {
    let mut bytes = 0;
    let mut count = 0;
    for packet in planned {
        if count > 0 && bytes + packet.len > max_bytes {
            break;
        }
        bytes += packet.len;
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::SyntheticStream;

    // The default synthetic stream: 60 fps, 1 s GOPs of 60 × 1,000-byte frames
    // (60 KB per GOP), plus two audio tracks of 20 ms / 480-byte packets.

    fn roomy() -> DvrConfig {
        DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 }
    }

    fn push_all(dvr: &mut DvrBuffer, packets: &[Packet]) {
        for p in packets {
            dvr.push(p.view()).expect("push");
        }
    }

    #[test]
    fn dvr_config_sizes_for_the_worst_case_bitrate() {
        let cfg = DvrConfig::for_window(Hns::from_secs(60), 20_000_000, 192_000);
        // 20 Mbps = 2.5 MB/s → 150 MB for one minute (DESIGN §4 table), plus ≤ 2 s headroom.
        assert!((150_000_000..=155_000_000).contains(&cfg.video_bytes), "{cfg:?}");
        // 192 kbps = 24 KB/s → 1.44 MB per track, plus ≤ 2 s headroom.
        assert!((1_440_000..=1_488_000).contains(&cfg.audio_bytes), "{cfg:?}");
    }

    #[test]
    fn dvr_discards_video_until_the_first_keyframe() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        let video: Vec<Packet> = (0..120).map(|_| s.next_video()).collect();
        // Start listening mid-GOP, at frame 10: nothing is decodable until frame 60.
        for p in &video[10..60] {
            assert_eq!(dvr.push(p.view()), Ok(PushOutcome::Discarded));
        }
        assert_eq!(dvr.push(video[60].view()), Ok(PushOutcome::Stored));
        assert_eq!(dvr.oldest_pts(), Some(video[60].meta.pts));
        assert_eq!(dvr.gop_count(), 1);
    }

    #[test]
    fn dvr_discards_audio_that_has_no_video_to_align_to() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        let audio = s.next_audio(TrackId::GameAudio);
        assert_eq!(dvr.push(audio.view()), Ok(PushOutcome::Discarded));
        assert_eq!(dvr.used_bytes(TrackId::GameAudio), 0);
    }

    #[test]
    fn dvr_evicts_whole_gops_and_only_as_many_as_needed() {
        // 60 KB GOPs into a 200 KB ring: exactly three whole GOPs fit.
        let mut dvr = DvrBuffer::new(DvrConfig { video_bytes: 200_000, audio_bytes: 1_000_000 });
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));
        assert_eq!(dvr.gop_count(), 3);
        assert_eq!(dvr.oldest_pts(), Some(Hns::from_secs(7)), "must stop on a GOP boundary");
        assert_eq!(dvr.used_bytes(TrackId::Video), 180_000);
    }

    #[test]
    fn dvr_evicts_audio_in_step_with_video() {
        // Video keeps 7 s..10 s; audio older than 7 s must go with the evicted GOPs,
        // even though the audio ring itself has plenty of room.
        let mut dvr = DvrBuffer::new(DvrConfig { video_bytes: 200_000, audio_bytes: 1_000_000 });
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));
        // 3 s × 50 packets/s × 480 bytes
        assert_eq!(dvr.used_bytes(TrackId::GameAudio), 72_000);
        assert_eq!(dvr.used_bytes(TrackId::Mic), 72_000);
    }

    #[test]
    fn dvr_rejects_a_gop_larger_than_the_ring() {
        let mut dvr = DvrBuffer::new(DvrConfig { video_bytes: 50_000, audio_bytes: 10_000 });
        let mut s = SyntheticStream::new();
        let results: Vec<_> = (0..60).map(|_| dvr.push(s.next_video().view())).collect();
        assert!(
            results.iter().any(|r| matches!(r, Err(DvrError::GopTooLarge { .. }))),
            "a 60 KB GOP can't fit in 50 KB: {results:?}"
        );
    }

    #[test]
    fn dvr_snapshot_starts_at_the_keyframe_covering_the_window() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        let packets = s.take_secs(10);
        push_all(&mut dvr, &packets);

        let clip = dvr.snapshot(Hns::from_millis(3_500)).unwrap();
        // Footage ends at 10 s. Covering 3.5 s means starting at or before 6.5 s,
        // and the newest GOP start that does that is 6 s.
        let first = clip.video().next().unwrap();
        assert!(first.meta.keyframe);
        assert_eq!(first.meta.pts, Hns::from_secs(6));
        let last_pushed = packets.iter().rev().find(|p| p.meta.track == TrackId::Video).unwrap();
        assert_eq!(clip.video().last().unwrap(), last_pushed);
        assert_eq!(clip.duration(), Hns::from_secs(4));
    }

    #[test]
    fn dvr_snapshot_has_every_frame_in_order() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));
        let clip = dvr.snapshot(Hns::from_millis(3_500)).unwrap();
        let frames: Vec<u32> =
            clip.video().map(|p| SyntheticStream::sequence_number(&p.data)).collect();
        assert_eq!(frames, (360..600).collect::<Vec<u32>>());
    }

    #[test]
    fn dvr_snapshot_audio_is_time_aligned_with_video() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));
        let clip = dvr.snapshot(Hns::from_millis(3_500)).unwrap();
        for track in [TrackId::GameAudio, TrackId::Mic] {
            let audio: Vec<&Packet> = clip.track(track).collect();
            assert_eq!(audio.len(), 200, "{track:?}: 4 s × 50 packets/s");
            assert!(audio.iter().all(|p| p.meta.pts >= Hns::from_secs(6)));
            assert!(audio.iter().all(|p| p.meta.pts < Hns::from_secs(10)));
        }
        // Presentation order across tracks.
        assert!(clip.packets.windows(2).all(|w| w[0].meta.pts <= w[1].meta.pts));
    }

    #[test]
    fn dvr_snapshot_longer_than_the_buffer_returns_everything() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));
        assert_eq!(dvr.buffered(), Hns::from_secs(10));
        let clip = dvr.snapshot(Hns::from_secs(3_600)).unwrap();
        assert_eq!(clip.video().next().unwrap().meta.pts, Hns::ZERO);
        assert_eq!(clip.video().count(), 600);
    }

    #[test]
    fn dvr_snapshot_of_an_empty_buffer_is_an_error() {
        let mut dvr = DvrBuffer::new(roomy());
        assert_eq!(dvr.snapshot(Hns::from_secs(1)), Err(DvrError::Empty));
        let mut s = SyntheticStream::new();
        let _ = dvr.push(s.next_audio(TrackId::Mic).view());
        assert_eq!(
            dvr.snapshot(Hns::from_secs(1)),
            Err(DvrError::Empty),
            "audio alone isn't a clip"
        );
    }

    #[test]
    fn dvr_resize_shrinks_from_the_oldest_end_and_grows_losslessly() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));

        dvr.resize(DvrConfig { video_bytes: 20_000_000, audio_bytes: 2_000_000 });
        assert_eq!(dvr.gop_count(), 10, "growing keeps everything");

        dvr.resize(DvrConfig { video_bytes: 200_000, audio_bytes: 1_000_000 });
        assert_eq!(dvr.gop_count(), 3);
        assert_eq!(dvr.oldest_pts(), Some(Hns::from_secs(7)));
        assert_eq!(dvr.used_bytes(TrackId::GameAudio), 72_000);
    }

    #[test]
    fn dvr_clear_requires_a_new_keyframe() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(2));
        dvr.clear();
        assert_eq!(dvr.gop_count(), 0);
        assert_eq!(dvr.buffered(), Hns::ZERO);
        let _keyframe = s.next_video(); // frame 120 starts a GOP, but we skip it…
        let mid_gop = s.next_video(); // …so frame 121 has nothing to decode from.
        assert_eq!(dvr.push(mid_gop.view()), Ok(PushOutcome::Discarded));
    }

    #[test]
    fn dvr_chunked_copy_matches_one_shot_snapshot() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(10));
        let one_shot = dvr.snapshot(Hns::from_millis(3_500)).unwrap();

        let mut copy = SnapshotCopy::new(dvr.plan_snapshot(Hns::from_millis(3_500)).unwrap());
        let mut later = s.take_secs(2).into_iter();
        let mut chunks = 0;
        while !copy.is_done() {
            copy.copy_chunk(&dvr, 2_500).unwrap();
            chunks += 1;
            // New footage between chunks neither overwrites nor changes the plan.
            if let Some(p) = later.next() {
                dvr.push(p.view()).unwrap();
            }
        }
        assert!(chunks > 10, "2.5 KB chunks should split the copy: {chunks} chunks");
        assert_eq!(copy.finish(), one_shot);
    }

    #[test]
    fn dvr_chunked_copy_skips_to_the_next_keyframe_when_overwritten() {
        // Three 60 KB GOPs fill the 200 KB video ring, with 20 KB to spare.
        let mut dvr = DvrBuffer::new(DvrConfig { video_bytes: 200_000, audio_bytes: 1_000_000 });
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(3));
        let mut copy = SnapshotCopy::new(dvr.plan_snapshot(Hns::from_millis(3_500)).unwrap());

        // The next second evicts GOP 0 and writes over its first 40 KB, keyframe included.
        push_all(&mut dvr, &s.take_secs(1));
        while !copy.is_done() {
            copy.copy_chunk(&dvr, 2_500).unwrap();
        }
        let clip = copy.finish();

        // The clip starts at the keyframe at 1 s, frame 60, and runs to the end of the plan.
        let frames: Vec<u32> =
            clip.video().map(|p| SyntheticStream::sequence_number(&p.data)).collect();
        assert_eq!(frames, (60..180).collect::<Vec<u32>>());
        assert!(clip.video().next().is_some_and(|p| p.meta.keyframe));
        // Audio from before 1 s is still intact in its ring, but it goes with the lost GOP.
        assert_eq!(clip.track(TrackId::Mic).count(), 100, "2 s × 50 packets/s");
        assert!(clip.packets.iter().all(|p| p.meta.pts >= Hns::from_secs(1)));
    }

    #[test]
    fn dvr_copy_is_stale_after_resize_or_clear() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        push_all(&mut dvr, &s.take_secs(2));

        let mut copy = SnapshotCopy::new(dvr.plan_snapshot(Hns::from_secs(1)).unwrap());
        dvr.resize(DvrConfig { video_bytes: 20_000_000, audio_bytes: 2_000_000 });
        assert_eq!(copy.copy_chunk(&dvr, usize::MAX), Err(DvrError::Changed));

        let mut copy = SnapshotCopy::new(dvr.plan_snapshot(Hns::from_secs(1)).unwrap());
        dvr.clear();
        assert_eq!(copy.copy_chunk(&dvr, usize::MAX), Err(DvrError::Changed));
    }
}
