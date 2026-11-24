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

/// What the editor does to one audio track (D38). Anything but `Keep` re-encodes that track to
/// AAC; video is never re-encoded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioEdit {
    /// Stream-copy the track as it is.
    Keep,
    /// Change its volume by `db` decibels (negative is quieter).
    Gain {
        /// Decibels; finite.
        db: f64,
    },
    /// Silence the whole track.
    Mute,
    /// Silence `[start, end)`. The track keeps its length, so it stays in sync with the video.
    Silence {
        /// Where the silence starts.
        start: Hns,
        /// Where it ends; must be after `start`.
        end: Hns,
    },
    /// Cut `[start, end)` out of this track only. The track gets shorter, so it ends earlier
    /// than the video after the cut.
    Remove {
        /// Where the cut starts.
        start: Hns,
        /// Where the cut ends; must be after `start`.
        end: Hns,
    },
}

/// One audio track of the source, with its bitrate so a re-encode can match it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioTrack {
    /// What to do to this track.
    pub edit: AudioEdit,
    /// The source track's bitrate, bits per second. Used only when the track is re-encoded.
    pub bitrate_bps: u32,
}

/// Arguments that edit audio tracks and stream-copy the video (D38). `tracks` lists every audio
/// track of `input` in order. Untouched tracks are stream-copied. Each edited track gets its own
/// `-filter_complex` chain and is re-encoded to AAC at its source bitrate.
///
/// # Errors
/// [`FfmpegError::Failed`] for a cut that is empty or starts before zero, or a non-finite gain.
pub fn audio_edit_args(
    input: &Path,
    output: &Path,
    tracks: &[AudioTrack],
) -> Result<Vec<OsString>, FfmpegError> {
    let mut chains = Vec::new();
    for (i, track) in tracks.iter().enumerate() {
        if let Some(chain) = track_chain(i, track.edit)? {
            chains.push(chain);
        }
    }
    let mut args = common_args();
    args.push("-i".into());
    args.push(os(input));
    if !chains.is_empty() {
        args.push("-filter_complex".into());
        args.push(chains.join(";").into());
    }
    args.extend(["-map", "0:v:0", "-c:v", "copy"].map(OsString::from));
    // Output audio tracks are mapped in input order, so output index `i` is input track `i`.
    for (i, track) in tracks.iter().enumerate() {
        if track.edit == AudioEdit::Keep {
            args.push("-map".into());
            args.push(format!("0:a:{i}").into());
            args.push(format!("-c:a:{i}").into());
            args.push("copy".into());
        } else {
            args.push("-map".into());
            args.push(format!("[a{i}]").into());
            args.push(format!("-c:a:{i}").into());
            args.push("aac".into());
            args.push(format!("-b:a:{i}").into());
            args.push(track.bitrate_bps.to_string().into());
        }
    }
    args.extend(["-movflags", "+faststart"].map(OsString::from));
    args.push(os(output));
    Ok(args)
}

/// The filter chain for audio track `i`, labelled `[a{i}]`. `None` for `Keep`.
fn track_chain(i: usize, edit: AudioEdit) -> Result<Option<String>, FfmpegError> {
    let input = format!("[0:a:{i}]");
    let out = format!("[a{i}]");
    let chain = match edit {
        AudioEdit::Keep => return Ok(None),
        AudioEdit::Gain { db } => {
            if !db.is_finite() {
                return Err(bad_audio("gain must be a finite number of decibels"));
            }
            format!("{input}volume={db}dB{out}")
        }
        AudioEdit::Mute => format!("{input}volume=0{out}"),
        AudioEdit::Silence { start, end } => {
            let (a, b) = cut_times(start, end)?;
            // `\,` keeps the comma inside `between(...)` from ending the filter option.
            format!("{input}volume=0:enable='between(t\\,{a}\\,{b})'{out}")
        }
        AudioEdit::Remove { start, end } => {
            let (a, b) = cut_times(start, end)?;
            if start == Hns::ZERO {
                // Cutting from the start just drops the head, so no join is needed.
                format!("{input}atrim=start={b},asetpts=PTS-STARTPTS{out}")
            } else {
                // Split the track, keep the part before the cut and the part after it, and join.
                format!(
                    "{input}asplit=2[s{i}a][s{i}b];\
                     [s{i}a]atrim=end={a},asetpts=PTS-STARTPTS[p{i}a];\
                     [s{i}b]atrim=start={b},asetpts=PTS-STARTPTS[p{i}b];\
                     [p{i}a][p{i}b]concat=n=2:v=0:a=1{out}"
                )
            }
        }
    };
    Ok(Some(chain))
}

/// Checks a cut range and formats both ends as ffmpeg seconds.
fn cut_times(start: Hns, end: Hns) -> Result<(String, String), FfmpegError> {
    if start < Hns::ZERO || end <= start {
        return Err(bad_audio("audio cut must start at or after zero and end after it"));
    }
    Ok((ffmpeg_time(start), ffmpeg_time(end)))
}

fn bad_audio(message: &str) -> FfmpegError {
    FfmpegError::Failed { stderr: message.into() }
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

    const KEEP: AudioTrack = AudioTrack { edit: AudioEdit::Keep, bitrate_bps: 128_000 };

    #[test]
    fn audio_edit_copies_untouched_tracks_and_the_video() {
        let tracks =
            [KEEP, AudioTrack { edit: AudioEdit::Gain { db: -6.0 }, bitrate_bps: 160_000 }];
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("out.mp4"), &tracks).unwrap();
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
                "-filter_complex",
                "[0:a:1]volume=-6dB[a1]",
                "-map",
                "0:v:0",
                "-c:v",
                "copy",
                "-map",
                "0:a:0",
                "-c:a:0",
                "copy",
                "-map",
                "[a1]",
                "-c:a:1",
                "aac",
                "-b:a:1",
                "160000",
                "-movflags",
                "+faststart",
                "out.mp4",
            ]
        );
    }

    #[test]
    fn audio_edit_with_no_changes_has_no_filter_graph() {
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("o.mp4"), &[KEEP]).unwrap();
        let s = strs(&args);
        assert!(!s.contains(&"-filter_complex".to_string()));
        assert!(s.windows(2).any(|w| w == ["-c:a:0", "copy"]));
    }

    #[test]
    fn mute_silences_the_whole_track() {
        let tracks = [AudioTrack { edit: AudioEdit::Mute, bitrate_bps: 96_000 }];
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("o.mp4"), &tracks).unwrap();
        let s = strs(&args);
        assert_eq!(s[7], "-filter_complex");
        assert_eq!(s[8], "[0:a:0]volume=0[a0]");
        assert!(s.windows(2).any(|w| w == ["-b:a:0", "96000"]));
    }

    #[test]
    fn silence_uses_a_timed_volume_filter_with_escaped_commas() {
        let edit = AudioEdit::Silence { start: Hns::from_millis(2_500), end: Hns::from_secs(4) };
        let tracks = [AudioTrack { edit, bitrate_bps: 128_000 }];
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("o.mp4"), &tracks).unwrap();
        assert_eq!(
            strs(&args)[8],
            r"[0:a:0]volume=0:enable='between(t\,2.5000000\,4.0000000)'[a0]"
        );
    }

    #[test]
    fn remove_splits_joins_and_cuts_the_middle() {
        let edit = AudioEdit::Remove { start: Hns::from_secs(1), end: Hns::from_secs(2) };
        let tracks = [AudioTrack { edit, bitrate_bps: 128_000 }];
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("o.mp4"), &tracks).unwrap();
        assert_eq!(
            strs(&args)[8],
            "[0:a:0]asplit=2[s0a][s0b];[s0a]atrim=end=1.0000000,asetpts=PTS-STARTPTS[p0a];\
             [s0b]atrim=start=2.0000000,asetpts=PTS-STARTPTS[p0b];[p0a][p0b]concat=n=2:v=0:a=1[a0]"
        );
    }

    #[test]
    fn remove_from_zero_only_drops_the_head() {
        let edit = AudioEdit::Remove { start: Hns::ZERO, end: Hns::from_secs(2) };
        let tracks = [AudioTrack { edit, bitrate_bps: 128_000 }];
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("o.mp4"), &tracks).unwrap();
        assert_eq!(strs(&args)[8], "[0:a:0]atrim=start=2.0000000,asetpts=PTS-STARTPTS[a0]");
    }

    #[test]
    fn audio_edit_refuses_bad_ranges_and_gains() {
        let bad_cut = AudioEdit::Silence { start: Hns::from_secs(3), end: Hns::from_secs(3) };
        let negative = AudioEdit::Remove { start: Hns(-1), end: Hns::from_secs(1) };
        let nan = AudioEdit::Gain { db: f64::NAN };
        for edit in [bad_cut, negative, nan] {
            let tracks = [AudioTrack { edit, bitrate_bps: 128_000 }];
            assert!(audio_edit_args(Path::new("i"), Path::new("o"), &tracks).is_err(), "{edit:?}");
        }
    }

    #[test]
    fn concat_line_escapes_single_quotes() {
        assert_eq!(concat_line(Path::new(r"C:\clips\a.mp4")), r"file 'C:\clips\a.mp4'");
        assert_eq!(concat_line(Path::new("it's.mp4")), r"file 'it'\''s.mp4'");
    }
}
