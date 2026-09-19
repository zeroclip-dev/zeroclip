//! The retrospective DVR buffer: "save the last N minutes" (DESIGN §4, §5, §6).
//!
//! # Rules this buffer must follow
//!
//! 1. **Pre-allocated, allocation-free pushes.** All storage is reserved in
//!    [`DvrBuffer::new`]. After that, [`push`](DvrBuffer::push) only copies bytes
//!    into existing memory; it runs on encoder threads during gameplay.
//! 2. **GOP-aware eviction.** A clip must start at an IDR frame or it won't decode.
//!    The video ring keeps an index of GOPs (start PTS, byte offset, length) and
//!    evicts whole GOPs from the front — never individual frames.
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

use crate::media::{Packet, PacketRef, TrackId};
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
    /// Sizes the buffer to hold `window` of footage at the worst-case bitrates.
    ///
    /// The ring must fit the *maximum* VBR bitrate (DESIGN §4): 20 Mbps is 2.5 MB/s,
    /// so one minute needs ~150 MB. A little headroom (at most ~2 s) is fine and lets
    /// a full window survive whole-GOP eviction.
    #[must_use]
    pub fn for_window(window: Hns, max_video_bitrate_bps: u64, audio_bitrate_bps: u64) -> Self {
        let _ = (window, max_video_bitrate_bps, audio_bitrate_bps);
        todo!("Phase 3: size the rings (see the `dvr_config_*` spec)")
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
}

/// The retrospective ring buffer. See the [module docs](self) for the rules it follows.
#[derive(Debug)]
pub struct DvrBuffer {
    config: DvrConfig,
    // Phase 3: your storage goes here — pre-allocated byte rings (one per track)
    // plus the GOP index. `VecDeque<Packet>` would allocate on every push; don't.
}

impl DvrBuffer {
    /// Allocates all storage up front. After this returns, `push` must not allocate.
    #[must_use]
    pub fn new(config: DvrConfig) -> Self {
        let _ = config;
        todo!("Phase 3: pre-allocate the rings and the GOP index")
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
        let _ = packet;
        todo!("Phase 3: GOP-aware push (see the `dvr_*` specs)")
    }

    /// Number of GOPs held, including the one currently being written.
    #[must_use]
    pub fn gop_count(&self) -> usize {
        todo!("Phase 3")
    }

    /// Start PTS of the oldest retained GOP, or `None` if empty.
    #[must_use]
    pub fn oldest_pts(&self) -> Option<Hns> {
        todo!("Phase 3")
    }

    /// Span of video currently held: from the oldest GOP's start to the end of the newest frame.
    #[must_use]
    pub fn buffered(&self) -> Hns {
        todo!("Phase 3")
    }

    /// Bytes currently used by `track`.
    #[must_use]
    pub fn used_bytes(&self, track: TrackId) -> usize {
        let _ = track;
        todo!("Phase 3")
    }

    /// Copies out the last `window` of footage: video from the IDR of the newest GOP
    /// that covers the window, plus all audio from that point on.
    ///
    /// If less than `window` is buffered, returns everything.
    ///
    /// # Errors
    /// [`DvrError::Empty`] if there's no video yet.
    pub fn snapshot(&self, window: Hns) -> Result<DvrClip, DvrError> {
        let _ = window;
        todo!("Phase 3: snapshot from the covering GOP (see the `dvr_snapshot_*` specs)")
    }

    /// Changes capacity when a game profile asks for a different window (DESIGN §7).
    /// Growing keeps everything; shrinking evicts the oldest GOPs first. The engine
    /// pauses the writer around this call, so it may allocate.
    pub fn resize(&mut self, config: DvrConfig) {
        let _ = config;
        todo!("Phase 3: resize (see the `dvr_resize_*` spec)")
    }

    /// Drops everything. The next stored packet must again be a keyframe.
    /// Used after a watchdog re-init (DESIGN §8: "buffer restarts from empty").
    pub fn clear(&mut self) {
        todo!("Phase 3")
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
    #[ignore = "spec: DvrConfig::for_window (Phase 3)"]
    fn dvr_config_sizes_for_the_worst_case_bitrate() {
        let cfg = DvrConfig::for_window(Hns::from_secs(60), 20_000_000, 192_000);
        // 20 Mbps = 2.5 MB/s → 150 MB for one minute (DESIGN §4 table), plus ≤ 2 s headroom.
        assert!((150_000_000..=155_000_000).contains(&cfg.video_bytes), "{cfg:?}");
        // 192 kbps = 24 KB/s → 1.44 MB per track, plus ≤ 2 s headroom.
        assert!((1_440_000..=1_488_000).contains(&cfg.audio_bytes), "{cfg:?}");
    }

    #[test]
    #[ignore = "spec: DvrBuffer::push (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::push (Phase 3)"]
    fn dvr_discards_audio_that_has_no_video_to_align_to() {
        let mut dvr = DvrBuffer::new(roomy());
        let mut s = SyntheticStream::new();
        let audio = s.next_audio(TrackId::GameAudio);
        assert_eq!(dvr.push(audio.view()), Ok(PushOutcome::Discarded));
        assert_eq!(dvr.used_bytes(TrackId::GameAudio), 0);
    }

    #[test]
    #[ignore = "spec: DvrBuffer::push eviction (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::push eviction (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::push (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::snapshot (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::snapshot (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::snapshot (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::snapshot (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::snapshot (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::resize (Phase 3)"]
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
    #[ignore = "spec: DvrBuffer::clear (Phase 3)"]
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
}
