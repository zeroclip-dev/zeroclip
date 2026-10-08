//! Codecs, tracks and encoded packets.

use serde::{Deserialize, Serialize};

use crate::time::Hns;

/// Hardware video codecs, best compression first (DESIGN §3, "encoder capability ladder").
///
/// Hardware AV1 encode needs Arc, RTX 40-series or RDNA3+; assuming it exists would
/// be a hard failure on launch for a large slice of users. At first run the video
/// adapter probes what the GPU can do, and the ladder picks the best available.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// AV1 — best compression, newest hardware only.
    Av1,
    /// HEVC / H.265 — broad support, good compression.
    Hevc,
    /// H.264 — the universal fallback.
    H264,
}

impl Codec {
    /// The capability ladder, best first.
    pub const LADDER: [Self; 3] = [Self::Av1, Self::Hevc, Self::H264];

    /// Human-readable name for the UI.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Av1 => "AV1",
            Self::Hevc => "HEVC",
            Self::H264 => "H.264",
        }
    }

    /// The next codec down the ladder, or `None` if this is already the fallback.
    /// The watchdog uses this when re-initialisation keeps failing (DESIGN §8).
    #[must_use]
    pub fn step_down(self) -> Option<Self> {
        let i = Self::LADDER.iter().position(|&c| c == self)?;
        Self::LADDER.get(i + 1).copied()
    }

    /// Chooses the codec to encode with, given what the hardware supports.
    ///
    /// A user preference wins when the hardware supports it; otherwise the best
    /// supported codec on the ladder is used. `None` means no hardware encoder at all.
    #[must_use]
    pub fn select(supported: &[Self], preferred: Option<Self>) -> Option<Self> {
        if let Some(p) = preferred
            && supported.contains(&p)
        {
            return Some(p);
        }
        Self::LADDER.into_iter().find(|c| supported.contains(c))
    }
}

/// Container formats that survive a crash mid-write (DESIGN §1, "crash-survivable containers").
///
/// Which one becomes the default is an open question (docs/OPEN_QUESTIONS.md):
/// MKV tooling is more forgiving; fragmented MP4 plays in browsers without a remux.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerFormat {
    /// Fragmented MP4 (`moof`/`mdat` fragments).
    #[default]
    #[serde(rename = "fmp4")]
    FragmentedMp4,
    /// Matroska.
    #[serde(rename = "mkv")]
    Matroska,
}

impl ContainerFormat {
    /// File extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::FragmentedMp4 => "mp4",
            Self::Matroska => "mkv",
        }
    }
}

/// Most audio tracks a recording can carry. Video is always one more track (DESIGN §6).
pub const MAX_AUDIO_TRACKS: u8 = 6;

/// An audio track's slot, below [`MAX_AUDIO_TRACKS`]. The field is private, so the only
/// ways to make one are [`TrackId::audio`] and the named constants on [`TrackId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioTrack(u8);

/// The tracks of a recording: one video track and up to [`MAX_AUDIO_TRACKS`] audio
/// tracks (DESIGN §6). Labels such as "Game" live in `zc_mux::TrackInfo`, not here,
/// so this type stays `Copy` and allocation-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrackId {
    /// Encoded video from the GPU pipeline.
    Video,
    /// An audio track; slot 0 and 1 are the game and mic tracks.
    Audio(AudioTrack),
}

impl TrackId {
    /// Track 1: game/system audio (process loopback), audio slot 0.
    // The CamelCase names are the old variant names, kept so call sites read the same.
    #[allow(non_upper_case_globals)]
    pub const GameAudio: Self = Self::Audio(AudioTrack(0));
    /// Track 2: microphone, gated by push-to-talk, audio slot 1.
    #[allow(non_upper_case_globals)]
    pub const Mic: Self = Self::Audio(AudioTrack(1));

    /// Number of tracks: video plus every audio slot.
    pub const COUNT: usize = 1 + MAX_AUDIO_TRACKS as usize;
    /// All tracks, in index order: video, then audio slots 0 and up.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Video,
        Self::Audio(AudioTrack(0)),
        Self::Audio(AudioTrack(1)),
        Self::Audio(AudioTrack(2)),
        Self::Audio(AudioTrack(3)),
        Self::Audio(AudioTrack(4)),
        Self::Audio(AudioTrack(5)),
    ];

    /// The audio track in slot `n`, or `None` if `n` is [`MAX_AUDIO_TRACKS`] or more.
    #[must_use]
    pub const fn audio(n: u8) -> Option<Self> {
        if n < MAX_AUDIO_TRACKS { Some(Self::Audio(AudioTrack(n))) } else { None }
    }

    /// Dense index, for per-track arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Video => 0,
            Self::Audio(AudioTrack(n)) => 1 + n as usize,
        }
    }

    /// Whether this is an audio track.
    #[must_use]
    pub const fn is_audio(self) -> bool {
        !matches!(self, Self::Video)
    }
}

/// Everything about an encoded packet except its bytes.
///
/// There is no separate decode timestamp: encoders are configured without
/// B-frames (`CODECAPI_AVEncMPVDefaultBPictureCount = 0`), so decode order equals
/// presentation order. See docs/DECISIONS.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketMeta {
    /// Which track the packet belongs to.
    pub track: TrackId,
    /// Presentation timestamp on the session timeline (see [`MasterClock`](crate::time::MasterClock)).
    pub pts: Hns,
    /// How long the packet plays for.
    pub duration: Hns,
    /// Video: an IDR frame that starts a GOP. Audio: always `true` (every AAC frame is independently decodable).
    pub keyframe: bool,
}

impl PacketMeta {
    /// Presentation end: `pts + duration`.
    #[must_use]
    pub fn end(&self) -> Hns {
        self.pts + self.duration
    }
}

/// A borrowed encoded packet. This is what flows through the hot path: the bytes
/// stay wherever the encoder put them until a sink copies them into its own
/// pre-allocated storage.
#[derive(Clone, Copy, Debug)]
pub struct PacketRef<'a> {
    /// Timing and track.
    pub meta: PacketMeta,
    /// Encoded bytes (e.g. an Annex-B access unit, or one AAC frame).
    pub data: &'a [u8],
}

impl PacketRef<'_> {
    /// Copies into an owned [`Packet`]. Allocates — never call this on the hot path.
    #[must_use]
    pub fn to_packet(&self) -> Packet {
        Packet { meta: self.meta, data: self.data.to_vec() }
    }
}

/// An owned encoded packet. Used off the hot path: clip snapshots, tests, file writers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Timing and track.
    pub meta: PacketMeta,
    /// Encoded bytes.
    pub data: Vec<u8>,
}

impl Packet {
    /// Borrows as a [`PacketRef`].
    #[must_use]
    pub fn view(&self) -> PacketRef<'_> {
        PacketRef { meta: self.meta, data: &self.data }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_indices_are_dense() {
        for (i, t) in TrackId::ALL.iter().enumerate() {
            assert_eq!(t.index(), i);
        }
        assert!(!TrackId::Video.is_audio());
        assert!(TrackId::Mic.is_audio());
    }

    #[test]
    fn track_indices_are_dense_for_all_audio_tracks() {
        for (i, t) in TrackId::ALL.iter().enumerate() {
            assert_eq!(t.index(), i);
        }
        for n in 0..MAX_AUDIO_TRACKS {
            let track = TrackId::audio(n).expect("slot below the maximum");
            assert!(track.is_audio());
            assert_eq!(track.index(), 1 + usize::from(n));
        }
        assert_eq!(TrackId::audio(0), Some(TrackId::GameAudio));
        assert_eq!(TrackId::audio(1), Some(TrackId::Mic));
        assert_eq!(TrackId::audio(MAX_AUDIO_TRACKS), None);
    }

    #[test]
    fn packet_round_trip() {
        let meta = PacketMeta {
            track: TrackId::Video,
            pts: Hns::from_millis(10),
            duration: Hns::from_millis(16),
            keyframe: true,
        };
        let owned = PacketRef { meta, data: &[1, 2, 3] }.to_packet();
        assert_eq!(owned.view().data, &[1, 2, 3]);
        assert_eq!(owned.meta.end(), Hns::from_millis(26));
    }

    #[test]
    fn codec_ladder_steps_down_to_h264() {
        assert_eq!(Codec::Av1.step_down(), Some(Codec::Hevc));
        assert_eq!(Codec::Hevc.step_down(), Some(Codec::H264));
        assert_eq!(Codec::H264.step_down(), None);
    }

    #[test]
    fn codec_select_prefers_best_supported() {
        assert_eq!(Codec::select(&[Codec::H264, Codec::Hevc], None), Some(Codec::Hevc));
        assert_eq!(Codec::select(&Codec::LADDER, None), Some(Codec::Av1));
        assert_eq!(Codec::select(&[], None), None);
    }

    #[test]
    fn codec_select_honours_supported_preference_only() {
        let supported = [Codec::Hevc, Codec::H264];
        assert_eq!(Codec::select(&supported, Some(Codec::H264)), Some(Codec::H264));
        // AV1 was asked for but the GPU can't do it: fall back down the ladder.
        assert_eq!(Codec::select(&supported, Some(Codec::Av1)), Some(Codec::Hevc));
    }
}
