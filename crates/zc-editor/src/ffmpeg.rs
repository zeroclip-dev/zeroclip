//! Driving `ffmpeg.exe` (DESIGN §9).
//!
//! Every job is a stream copy (`-c copy`): no decoding, no encoding, so a
//! 5-minute clip trims in well under a second. Output is written to a new file;
//! the source is never overwritten, and is only removed once the export has been
//! verified to play (DESIGN §9, "Worth adding").

use std::ffi::OsString;
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

/// Flags every job starts with: quiet, never read stdin, and never overwrite. Exports are
/// always new files (DESIGN §9), so `-n` makes a name clash an error instead of data loss.
fn common_args() -> Vec<OsString> {
    ["-hide_banner", "-nostdin", "-loglevel", "error", "-n"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn os(path: &Path) -> OsString {
    path.as_os_str().to_os_string()
}

/// Formats a time as ffmpeg's `SS.fffffff` seconds. Seven digits keep every 100 ns tick exact.
/// Negative times clamp to zero.
#[must_use]
pub fn ffmpeg_time(t: Hns) -> String {
    let ticks = t.ticks().max(0);
    format!("{}.{:07}", ticks / Hns::PER_SECOND, ticks % Hns::PER_SECOND)
}

/// One line of an ffmpeg concat list: `file '<path>'`, with `'` escaped as `'\''`.
#[must_use]
pub fn concat_line(path: &Path) -> String {
    format!("file '{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Arguments for a stream-copy trim of `[start, end)`. `-t` goes after `-i`: input seeking
/// resets timestamps, so a duration is what's needed (works on every ffmpeg version).
///
/// # Errors
/// [`FfmpegError::Failed`] when `end <= start`; nothing is run.
pub fn trim_args(
    input: &Path,
    start: Hns,
    end: Hns,
    output: &Path,
) -> Result<Vec<OsString>, FfmpegError> {
    if end <= start {
        return Err(FfmpegError::Failed { stderr: "empty selection".into() });
    }
    let mut args = common_args();
    args.push("-ss".into());
    args.push(ffmpeg_time(start).into());
    args.push("-i".into());
    args.push(os(input));
    args.push("-t".into());
    args.push(ffmpeg_time(end - start).into());
    args.extend(["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"].map(OsString::from));
    args.extend(["-movflags", "+faststart"].map(OsString::from));
    args.push(os(output));
    Ok(args)
}

/// Arguments for joining the clips listed in `list` (see [`concat_line`]).
#[must_use]
pub fn concat_args(list: &Path, output: &Path) -> Vec<OsString> {
    let mut args = common_args();
    args.extend(["-f", "concat", "-safe", "0", "-i"].map(OsString::from));
    args.push(os(list));
    args.extend(["-map", "0", "-c", "copy", "-movflags", "+faststart"].map(OsString::from));
    args.push(os(output));
    args
}

/// Arguments for a stream-copy remux to a plain MP4 with the index up front.
#[must_use]
pub fn remux_args(input: &Path, output: &Path) -> Vec<OsString> {
    let mut args = common_args();
    args.push("-i".into());
    args.push(os(input));
    args.extend(["-map", "0", "-c", "copy", "-movflags", "+faststart"].map(OsString::from));
    args.push(os(output));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn ffmpeg_time_is_exact_to_the_tick() {
        assert_eq!(ffmpeg_time(Hns::from_millis(2_700)), "2.7000000");
        assert_eq!(ffmpeg_time(Hns(1)), "0.0000001");
        assert_eq!(ffmpeg_time(Hns(-5)), "0.0000000");
    }

    #[test]
    fn trim_is_a_stream_copy_with_duration_after_input() {
        let args = trim_args(
            Path::new("in.mp4"),
            Hns::from_millis(2_700),
            Hns::from_secs(5),
            Path::new("out.mp4"),
        )
        .unwrap();
        assert_eq!(
            strs(&args),
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-n",
                "-ss",
                "2.7000000",
                "-i",
                "in.mp4",
                "-t",
                "2.3000000",
                "-map",
                "0",
                "-c",
                "copy",
                "-avoid_negative_ts",
                "make_zero",
                "-movflags",
                "+faststart",
                "out.mp4",
            ]
        );
    }

    #[test]
    fn trim_refuses_an_empty_selection() {
        let err = trim_args(
            Path::new("in.mp4"),
            Hns::from_secs(3),
            Hns::from_secs(3),
            Path::new("o.mp4"),
        )
        .unwrap_err();
        assert!(matches!(err, FfmpegError::Failed { ref stderr } if stderr == "empty selection"));
    }

    #[test]
    fn concat_reads_the_list_and_copies_streams() {
        let args = concat_args(Path::new("list.txt"), Path::new("out.mp4"));
        assert_eq!(
            strs(&args),
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                "list.txt",
                "-map",
                "0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "out.mp4",
            ]
        );
    }

    #[test]
    fn remux_copies_streams_with_faststart() {
        let args = remux_args(Path::new("in.mp4"), Path::new("out.mp4"));
        assert_eq!(
            strs(&args),
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-n",
                "-i",
                "in.mp4",
                "-map",
                "0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "out.mp4",
            ]
        );
    }

    #[test]
    fn concat_line_escapes_single_quotes() {
        assert_eq!(concat_line(Path::new(r"C:\clips\a.mp4")), r"file 'C:\clips\a.mp4'");
        assert_eq!(concat_line(Path::new("it's.mp4")), r"file 'it'\''s.mp4'");
    }
}
