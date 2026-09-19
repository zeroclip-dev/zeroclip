//! Deterministic test fixtures (DESIGN §11: "write the disk writer and muxer
//! against synthetic frames before wiring real capture into them").
//!
//! Debugging a ring buffer is far easier when frames are numbered 1, 2, 3 than
//! when they're real video: every synthetic payload starts with its sequence
//! number, so a test can assert exactly which frames survived eviction.

use crate::media::{Packet, PacketMeta, TrackId};
use crate::time::Hns;

/// A synthetic A/V stream: numbered video frames with a fixed GOP length, plus
/// two audio tracks of fixed-size packets.
///
/// Defaults: 60 fps, 60-frame (1 s) GOPs, 1,000-byte frames; audio packets of
/// 20 ms and 480 bytes (192 kbps) on both [`TrackId::GameAudio`] and [`TrackId::Mic`].
///
/// Payload layout: bytes 0..4 hold the sequence number (little-endian `u32`, the
/// frame number for video), byte 4 the track index, the rest is filler.
#[derive(Clone, Debug)]
pub struct SyntheticStream {
    fps: u32,
    gop_frames: u32,
    frame_bytes: usize,
    audio_packet: Hns,
    audio_bytes: usize,
    next_frame: u32,
    next_audio: [u32; 2],
}

impl Default for SyntheticStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntheticStream {
    /// A stream with the defaults described above.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fps: 60,
            gop_frames: 60,
            frame_bytes: 1_000,
            audio_packet: Hns::from_millis(20),
            audio_bytes: 480,
            next_frame: 0,
            next_audio: [0; 2],
        }
    }

    /// Sets the frame rate.
    #[must_use]
    pub fn fps(mut self, fps: u32) -> Self {
        assert!(fps > 0, "fps must be positive");
        self.fps = fps;
        self
    }

    /// Sets the GOP length in frames.
    #[must_use]
    pub fn gop_frames(mut self, frames: u32) -> Self {
        assert!(frames > 0, "GOP length must be positive");
        self.gop_frames = frames;
        self
    }

    /// Sets the size of every video frame (at least 5 bytes).
    #[must_use]
    pub fn frame_bytes(mut self, bytes: usize) -> Self {
        self.frame_bytes = bytes.max(5);
        self
    }

    /// PTS of frame `n`. Computed from the frame number (not accumulated), so
    /// frame rates like 60 fps that don't divide 10⁷ never drift.
    #[must_use]
    pub fn frame_pts(&self, n: u32) -> Hns {
        Hns(i64::from(n) * Hns::PER_SECOND / i64::from(self.fps))
    }

    /// Duration of one GOP.
    #[must_use]
    pub fn gop_duration(&self) -> Hns {
        self.frame_pts(self.gop_frames)
    }

    /// PTS of the next video frame, i.e. how far the stream has progressed.
    #[must_use]
    pub fn now(&self) -> Hns {
        self.frame_pts(self.next_frame)
    }

    /// The next video frame. Every `gop_frames`-th frame is a keyframe, starting with frame 0.
    pub fn next_video(&mut self) -> Packet {
        let n = self.next_frame;
        self.next_frame += 1;
        let pts = self.frame_pts(n);
        Packet {
            meta: PacketMeta {
                track: TrackId::Video,
                pts,
                duration: self.frame_pts(n + 1) - pts,
                keyframe: n.is_multiple_of(self.gop_frames),
            },
            data: payload(n, TrackId::Video, self.frame_bytes),
        }
    }

    /// The next audio packet of `track` (`GameAudio` or `Mic`).
    ///
    /// # Panics
    /// If `track` is [`TrackId::Video`].
    pub fn next_audio(&mut self, track: TrackId) -> Packet {
        assert!(track.is_audio(), "next_audio needs an audio track");
        let slot = &mut self.next_audio[track.index() - 1];
        let n = *slot;
        *slot += 1;
        Packet {
            meta: PacketMeta {
                track,
                pts: Hns(self.audio_packet.ticks() * i64::from(n)),
                duration: self.audio_packet,
                keyframe: true,
            },
            data: payload(n, track, self.audio_bytes),
        }
    }

    /// Every packet (video and both audio tracks) with PTS before `until`, in
    /// presentation order; video first when timestamps tie. Continues from where
    /// the previous call stopped.
    pub fn until(&mut self, until: Hns) -> Vec<Packet> {
        let mut out = Vec::new();
        loop {
            let video_pts = self.now();
            let audio_pts =
                [0, 1].map(|i| Hns(self.audio_packet.ticks() * i64::from(self.next_audio[i])));
            let next = [video_pts, audio_pts[0], audio_pts[1]];
            let (which, pts) = next
                .iter()
                .copied()
                .enumerate()
                .min_by_key(|&(i, pts)| (pts, i))
                .unwrap_or((0, until));
            if pts >= until {
                return out;
            }
            out.push(match which {
                0 => self.next_video(),
                1 => self.next_audio(TrackId::GameAudio),
                _ => self.next_audio(TrackId::Mic),
            });
        }
    }

    /// The next `secs` seconds of packets. See [`until`](Self::until).
    pub fn take_secs(&mut self, secs: i64) -> Vec<Packet> {
        let until = self.now() + Hns::from_secs(secs);
        self.until(until)
    }

    /// The sequence number stamped into a synthetic payload.
    ///
    /// # Panics
    /// If `data` is shorter than 4 bytes (i.e. not a synthetic payload).
    #[must_use]
    pub fn sequence_number(data: &[u8]) -> u32 {
        let bytes: [u8; 4] = data[..4].try_into().expect("synthetic payloads start with 4 bytes");
        u32::from_le_bytes(bytes)
    }
}

fn payload(seq: u32, track: TrackId, len: usize) -> Vec<u8> {
    let mut data = vec![0xAB; len.max(5)];
    data[..4].copy_from_slice(&seq.to_le_bytes());
    #[expect(clippy::cast_possible_truncation, reason = "track index is 0..3")]
    let track_byte = track.index() as u8;
    data[4] = track_byte;
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_numbered_with_fixed_gops() {
        let mut s = SyntheticStream::new().gop_frames(3);
        let frames: Vec<Packet> = (0..7).map(|_| s.next_video()).collect();
        let keys: Vec<bool> = frames.iter().map(|p| p.meta.keyframe).collect();
        assert_eq!(keys, [true, false, false, true, false, false, true]);
        assert_eq!(SyntheticStream::sequence_number(&frames[5].data), 5);
        assert_eq!(frames[0].data.len(), 1_000);
    }

    #[test]
    fn frame_timing_does_not_drift() {
        let s = SyntheticStream::new();
        assert_eq!(s.frame_pts(60), Hns::from_secs(1));
        assert_eq!(s.frame_pts(6_000), Hns::from_secs(100));
        assert_eq!(s.gop_duration(), Hns::from_secs(1));
    }

    #[test]
    fn until_interleaves_in_presentation_order() {
        let mut s = SyntheticStream::new();
        let packets = s.take_secs(2);
        assert!(packets.windows(2).all(|w| w[0].meta.pts <= w[1].meta.pts));
        let count = |t| packets.iter().filter(|p| p.meta.track == t).count();
        assert_eq!(count(TrackId::Video), 120);
        assert_eq!(count(TrackId::GameAudio), 100);
        assert_eq!(count(TrackId::Mic), 100);
        assert_eq!(packets[0].meta.track, TrackId::Video, "video first on ties");
        assert_eq!(s.now(), Hns::from_secs(2));
        // Continues where it stopped.
        let more = s.take_secs(1);
        assert_eq!(SyntheticStream::sequence_number(&more[0].data), 120);
    }
}
