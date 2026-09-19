//! Driving `ffmpeg.exe` (DESIGN §9).
//!
//! Every job is a stream copy (`-c copy`): no decoding, no encoding, so a
//! 5-minute clip trims in well under a second. Output is written to a new file;
//! the source is never overwritten, and is only removed once the export has been
//! verified to play (DESIGN §9, "Worth adding").

use std::path::{Path, PathBuf};

use zc_core::Hns;

/// A located `ffmpeg.exe`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ffmpeg {
    /// Path to the executable.
    pub exe: PathBuf,
}

/// Why an ffmpeg job failed.
#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    /// No `ffmpeg.exe` on `PATH` or in the configured location.
    #[error("ffmpeg was not found; install it or set its location in Settings")]
    NotFound,
    /// Couldn't start the process.
    #[error("couldn't run ffmpeg: {0}")]
    Io(#[from] std::io::Error),
    /// ffmpeg exited with an error; `stderr` is its last lines.
    #[error("ffmpeg failed: {stderr}")]
    Failed {
        /// The tail of ffmpeg's stderr.
        stderr: String,
    },
}

impl Ffmpeg {
    /// Finds `ffmpeg.exe`: the configured path if given, else on `PATH`.
    ///
    /// # Errors
    /// [`FfmpegError::NotFound`].
    pub fn locate(configured: Option<&Path>) -> Result<Self, FfmpegError> {
        let _ = configured;
        todo!("Phase 5: check `configured`, then search PATH for ffmpeg.exe")
    }

    /// Stream-copies `[start, end)` of `input` into `output`. `start` must already be
    /// snapped to a keyframe (`crate::snap::snap_in_point`).
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn trim(
        &self,
        input: &Path,
        start: Hns,
        end: Hns,
        output: &Path,
    ) -> Result<(), FfmpegError> {
        let _ = (input, start, end, output);
        todo!(
            "Phase 5: ffmpeg -hide_banner -ss <start> -to <end> -i <input> -map 0 -c copy \
             -avoid_negative_ts make_zero <output>"
        )
    }

    /// Joins clips with the concat demuxer (check `crate::concat::check_compatible` first).
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn concat(&self, inputs: &[PathBuf], output: &Path) -> Result<(), FfmpegError> {
        let _ = (inputs, output);
        todo!(
            "Phase 5: write a concat list file, ffmpeg -f concat -safe 0 -i list.txt -map 0 -c copy <output>"
        )
    }

    /// Remuxes a fragmented recording into a plain MP4 with the index up front, for sharing.
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn remux_for_export(&self, input: &Path, output: &Path) -> Result<(), FfmpegError> {
        let _ = (input, output);
        todo!("Phase 5: ffmpeg -i <input> -map 0 -c copy -movflags +faststart <output>")
    }
}
