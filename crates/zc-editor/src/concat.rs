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
    /// Frame rate.
    pub fps: u32,
    /// Number of audio tracks.
    pub audio_tracks: u8,
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
            format!("frame rate {} vs {} fps", first.fps, clip.fps)
        } else if clip.audio_tracks != first.audio_tracks {
            format!("audio tracks {} vs {}", first.audio_tracks, clip.audio_tracks)
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

    const HEVC_1080P60: StreamInfo =
        StreamInfo { codec: Codec::Hevc, size: (1920, 1080), fps: 60, audio_tracks: 2 };

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
}
