//! Stitch compatibility (DESIGN §9, "Stitching").
//!
//! FFmpeg's concat demuxer joins clips instantly only when their streams match.
//! Clips from different profiles, or from after a codec-ladder fallback, won't —
//! so check up front and refuse with a clear reason (or offer a one-off re-encode)
//! instead of producing a broken file.

use zc_core::media::Codec;

/// The stream parameters that must match for a stream-copy concat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    /// Video codec.
    pub codec: Codec,
    /// Frame size, pixels.
    pub size: (u32, u32),
    /// Frame rate as a reduced fraction `(numerator, denominator)`, so 60000/1001 and 60/1 differ.
    /// `(0, 0)` when unknown.
    pub fps: (u32, u32),
    /// Number of audio tracks.
    pub audio_tracks: u8,
    /// Sample rate of the first audio track, Hz; `0` when there is no audio.
    pub audio_rate: u32,
    /// Channel count of the first audio track; `0` when there is no audio.
    pub audio_channels: u8,
}

/// A frame rate for people: `60`, `59.94`.
fn fps_text(fps: (u32, u32)) -> String {
    if fps.1 == 0 {
        return "unknown".into();
    }
    let value = f64::from(fps.0) / f64::from(fps.1);
    if fps.1 == 1 { format!("{}", fps.0) } else { format!("{value:.2}") }
}

/// Why clips can't be stitched without re-encoding.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConcatError {
    /// Nothing to stitch.
    #[error("select at least two clips to stitch")]
    TooFew,
    /// Clip `index` differs from the first clip.
    #[error("clip {index} doesn't match the first clip: {what}")]
    Mismatch {
        /// Position of the first mismatching clip.
        index: usize,
        /// What differs, for the UI ("codec HEVC vs H.264").
        what: String,
    },
}

/// Checks that every clip matches the first.
///
/// # Errors
/// [`ConcatError::TooFew`] for fewer than two clips; [`ConcatError::Mismatch`]
/// naming the first clip that differs.
pub fn check_compatible(clips: &[StreamInfo]) -> Result<(), ConcatError> {
    let Some((first, rest)) = clips.split_first() else {
        return Err(ConcatError::TooFew);
    };
    if rest.is_empty() {
        return Err(ConcatError::TooFew);
    }
    for (index, clip) in clips.iter().enumerate().skip(1) {
        let what = if clip.codec != first.codec {
            format!("codec {} vs {}", first.codec.name(), clip.codec.name())
        } else if clip.size != first.size {
            format!("size {}×{} vs {}×{}", first.size.0, first.size.1, clip.size.0, clip.size.1)
        } else if clip.fps != first.fps {
            format!("frame rate {} vs {} fps", fps_text(first.fps), fps_text(clip.fps))
        } else if clip.audio_tracks != first.audio_tracks {
            format!("audio tracks {} vs {}", first.audio_tracks, clip.audio_tracks)
        // A side with no audio (rate 0) has nothing to compare; the track count covers that.
        } else if clip.audio_rate != 0
            && first.audio_rate != 0
            && clip.audio_rate != first.audio_rate
        {
            format!("audio sample rate {} vs {} Hz", first.audio_rate, clip.audio_rate)
        } else if clip.audio_channels != 0
            && first.audio_channels != 0
            && clip.audio_channels != first.audio_channels
        {
            format!("audio channels {} vs {}", first.audio_channels, clip.audio_channels)
        } else {
            continue;
        };
        return Err(ConcatError::Mismatch { index, what });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEVC_1080P60: StreamInfo = StreamInfo {
        codec: Codec::Hevc,
        size: (1920, 1080),
        fps: (60, 1),
        audio_tracks: 2,
        audio_rate: 48_000,
        audio_channels: 2,
    };

    #[test]
    fn concat_accepts_matching_clips() {
        assert_eq!(check_compatible(&[HEVC_1080P60; 3]), Ok(()));
    }

    #[test]
    fn concat_names_the_first_mismatch() {
        let fallback = StreamInfo { codec: Codec::H264, ..HEVC_1080P60 };
        let err = check_compatible(&[HEVC_1080P60, HEVC_1080P60, fallback]).unwrap_err();
        assert!(matches!(err, ConcatError::Mismatch { index: 2, .. }), "{err:?}");
    }

    #[test]
    fn concat_needs_two_clips() {
        assert_eq!(check_compatible(&[HEVC_1080P60]), Err(ConcatError::TooFew));
    }

    #[test]
    fn concat_tells_ntsc_from_whole_frame_rates() {
        let ntsc = StreamInfo { fps: (60_000, 1001), ..HEVC_1080P60 };
        let err = check_compatible(&[HEVC_1080P60, ntsc]).unwrap_err();
        assert!(matches!(err, ConcatError::Mismatch { index: 1, .. }), "{err:?}");
    }

    #[test]
    fn concat_compares_audio_format_but_not_a_missing_track() {
        let rate = StreamInfo { audio_rate: 44_100, ..HEVC_1080P60 };
        assert!(check_compatible(&[HEVC_1080P60, rate]).is_err());
        let mono = StreamInfo { audio_channels: 1, ..HEVC_1080P60 };
        assert!(check_compatible(&[HEVC_1080P60, mono]).is_err());
        let silent = StreamInfo { audio_rate: 0, audio_channels: 0, ..HEVC_1080P60 };
        assert_eq!(check_compatible(&[HEVC_1080P60, silent]), Ok(()));
    }
}
