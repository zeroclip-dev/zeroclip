//! Driving `ffmpeg.exe` (DESIGN §9).
//!
//! Every job is a stream copy (`-c copy`): no decoding, no encoding, so a
//! 5-minute clip trims in well under a second. Output is written to a new file;
//! the source is never overwritten, and is only removed once the export has been
//! verified to play (DESIGN §9, "Worth adding").

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use zc_core::Hns;
use zc_core::media::Codec;

use crate::concat::StreamInfo;

/// The ffmpeg executable name on this platform.
const EXE: &str = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
/// The ffprobe executable name on this platform. It sits beside ffmpeg.
const PROBE_EXE: &str = if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" };

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
        let exe = match configured {
            // A configured path that's missing is an error, not a silent fallback.
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(_) => return Err(FfmpegError::NotFound),
            None => {
                beside_current_exe(EXE).or_else(|| search_path(EXE)).ok_or(FfmpegError::NotFound)?
            }
        };
        Ok(Self { exe })
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
        run(&self.exe, &trim_args(input, start, end, output)?).map(drop)
    }

    /// Joins clips with the concat demuxer (check `crate::concat::check_compatible` first).
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn concat(&self, inputs: &[PathBuf], output: &Path) -> Result<(), FfmpegError> {
        // The list lives in the temp dir under a pid+time name, so runs never collide.
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let list = std::env::temp_dir()
            .join(format!("zeroclip-concat-{}-{nanos}.txt", std::process::id()));
        let body: String = inputs.iter().map(|p| concat_line(p) + "\n").collect();
        std::fs::write(&list, body)?;
        let result = run(&self.exe, &concat_args(&list, output)).map(drop);
        // The list is scratch; failing to delete it must not fail the export.
        let _ = std::fs::remove_file(&list);
        result
    }

    /// Remuxes a fragmented recording into a plain MP4 with the index up front, for sharing.
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn remux_for_export(&self, input: &Path, output: &Path) -> Result<(), FfmpegError> {
        run(&self.exe, &remux_args(input, output)).map(drop)
    }

    /// Reads the first video stream, the audio track count, the duration and the keyframe
    /// times of `path` (ffprobe, demux only).
    ///
    /// # Errors
    /// See [`FfmpegError`]. A video codec other than H.264, HEVC or AV1 is `Failed`.
    pub fn probe(&self, path: &Path) -> Result<ProbeInfo, FfmpegError> {
        let ffprobe = self.exe.with_file_name(PROBE_EXE);
        let streams = run(&ffprobe, &stream_args(path))?;
        let (stream, duration, start) = parse_streams(&String::from_utf8_lossy(&streams))
            .map_err(|stderr| FfmpegError::Failed { stderr })?;
        let packets = run(&ffprobe, &keyframe_args(path))?;
        let keyframes = parse_keyframes(&String::from_utf8_lossy(&packets), start);
        Ok(ProbeInfo { stream, duration, keyframes })
    }

    /// Decodes the first audio track to mono f32 samples at `sample_rate`, for the waveform.
    /// A clip without audio is an `Err`; callers treat that as "no waveform".
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn audio_samples(&self, path: &Path, sample_rate: u32) -> Result<Vec<f32>, FfmpegError> {
        let mut args = common_args();
        args.push("-i".into());
        args.push(os(path));
        args.extend(["-map", "0:a:0", "-ac", "1", "-ar"].map(OsString::from));
        args.push(sample_rate.to_string().into());
        args.extend(["-f", "f32le", "-"].map(OsString::from));
        let bytes = run(&self.exe, &args)?;
        Ok(f32_samples(&bytes))
    }
}

/// What [`Ffmpeg::probe`] learned about a clip.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeInfo {
    /// The first video stream, plus the audio track count.
    pub stream: StreamInfo,
    /// Container duration.
    pub duration: Hns,
    /// Keyframe times, relative to the first frame (sorted).
    pub keyframes: Vec<Hns>,
}

/// The first existing `name` beside the running executable (a bundled ffmpeg).
fn beside_current_exe(name: &str) -> Option<PathBuf> {
    let candidate = std::env::current_exe().ok()?.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

/// The first existing `name` on `PATH`.
fn search_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// Runs `exe` with `args` and returns its stdout. Non-zero exit is `Failed` with the last
/// lines of stderr.
fn run(exe: &Path, args: &[OsString]) -> Result<Vec<u8>, FfmpegError> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (winbase.h): release builds are GUI-subsystem, so without this every
        // job would flash a console window.
        // https://learn.microsoft.com/windows/win32/procthread/process-creation-flags
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output()?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(FfmpegError::Failed { stderr: last_lines(&stderr, 20) })
    }
}

/// The last `n` lines of `text`.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// ffprobe arguments listing every stream's type and the first video stream's shape, plus the
/// container duration and start time. Parsed by [`parse_streams`].
fn stream_args(path: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-v", "error", "-show_entries"].map(OsString::from).to_vec();
    args.push(
        "stream=codec_type,codec_name,width,height,avg_frame_rate:format=duration,start_time"
            .into(),
    );
    args.extend(["-of", "default=noprint_wrappers=1"].map(OsString::from));
    args.push(os(path));
    args
}

/// ffprobe arguments listing the first video stream's packet times and flags. Packet level only,
/// so it demuxes without decoding and is fast even for long clips. Parsed by [`parse_keyframes`].
fn keyframe_args(path: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> =
        ["-v", "error", "-select_streams", "v:0", "-show_entries"].map(OsString::from).to_vec();
    args.push("packet=pts_time,flags".into());
    args.extend(["-of", "csv=print_section=0"].map(OsString::from));
    args.push(os(path));
    args
}

/// Parses `default=noprint_wrappers=1` output for [`STREAM_ENTRIES`]. Returns the stream info,
/// the container duration and its start time. `codec_type` comes first in each stream block.
fn parse_streams(text: &str) -> Result<(StreamInfo, Hns, Hns), String> {
    let mut video_seen = false;
    let mut in_first_video = false;
    let mut codec_name = None;
    let mut width = None;
    let mut height = None;
    let mut fps_rate = None;
    let mut audio = 0usize;
    let mut duration = None;
    let mut start = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "codec_type" => {
                in_first_video = value == "video" && !video_seen;
                video_seen |= value == "video";
                if value == "audio" {
                    audio += 1;
                }
            }
            "codec_name" if in_first_video => codec_name = Some(value),
            "width" if in_first_video => width = Some(value),
            "height" if in_first_video => height = Some(value),
            "avg_frame_rate" if in_first_video => fps_rate = Some(value),
            // Only the format section asks for these, and it comes after every stream block.
            "duration" => duration = value.parse::<f64>().ok(),
            "start_time" => start = value.parse::<f64>().ok(),
            _ => {}
        }
    }
    let codec = match codec_name.ok_or("no video stream")? {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "av1" => Codec::Av1,
        other => return Err(format!("unsupported video codec {other}")),
    };
    let size = (parse_dim(width)?, parse_dim(height)?);
    let fps = fps_rate.and_then(parse_rate).unwrap_or(0);
    let duration = Hns::from_secs_f64(duration.ok_or("no duration")?);
    // A missing start time (N/A) means the file starts at zero.
    let start = Hns::from_secs_f64(start.unwrap_or(0.0));
    let stream =
        StreamInfo { codec, size, fps, audio_tracks: u8::try_from(audio).unwrap_or(u8::MAX) };
    Ok((stream, duration, start))
}

fn parse_dim(value: Option<&str>) -> Result<u32, String> {
    value.and_then(|v| v.parse().ok()).ok_or_else(|| "missing video size".into())
}

/// Parses an ffprobe rate such as `60/1` or `30000/1001`, rounded to whole frames per second.
/// `0/0` (unknown) gives `None`.
fn parse_rate(rate: &str) -> Option<u32> {
    let (num, den) = rate.split_once('/')?;
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    if den == 0.0 {
        return None;
    }
    let fps = (num / den).round();
    // Frame rates are small positive numbers, so this conversion cannot lose anything real.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a frame rate is a small non-negative whole number"
    )]
    Some(fps as u32)
}

/// Parses `pts_time,flags` CSV lines from [`KEYFRAME_ENTRIES`]. Keeps keyframes (flags start with
/// `K`), subtracts `start` (ffmpeg's `-ss` counts from the first frame), and sorts.
fn parse_keyframes(text: &str, start: Hns) -> Vec<Hns> {
    let mut keys: Vec<Hns> = text
        .lines()
        .filter_map(|line| {
            let (pts, flags) = line.split_once(',')?;
            if !flags.starts_with('K') {
                return None;
            }
            let secs: f64 = pts.parse().ok()?;
            Some(Hns::from_secs_f64(secs) - start)
        })
        .collect();
    keys.sort();
    keys
}

/// Converts little-endian f32 PCM bytes to samples. A trailing partial sample is dropped.
fn f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
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

    /// Canned `stream_args` output: an audio track first, then video, then the format.
    const STREAMS: &str = "\
codec_type=audio
codec_type=video
codec_name=h264
width=1920
height=1080
avg_frame_rate=60/1
codec_type=audio
duration=12.500000
start_time=0.250000
";

    #[test]
    fn parse_streams_reads_the_first_video_and_the_container() {
        let (stream, duration, start) = parse_streams(STREAMS).unwrap();
        assert_eq!(stream.codec, Codec::H264);
        assert_eq!(stream.size, (1920, 1080));
        assert_eq!(stream.fps, 60);
        assert_eq!(stream.audio_tracks, 2);
        assert_eq!(duration, Hns::from_millis(12_500));
        assert_eq!(start, Hns::from_millis(250));
    }

    #[test]
    fn parse_streams_rejects_unsupported_codecs_and_missing_video() {
        let vp9 = STREAMS.replace("h264", "vp9");
        assert_eq!(parse_streams(&vp9).unwrap_err(), "unsupported video codec vp9");
        let audio_only = "codec_type=audio\nduration=1.0\nstart_time=0.0\n";
        assert_eq!(parse_streams(audio_only).unwrap_err(), "no video stream");
    }

    #[test]
    fn parse_rate_rounds_ntsc_and_rejects_unknown() {
        assert_eq!(parse_rate("60/1"), Some(60));
        assert_eq!(parse_rate("30000/1001"), Some(30));
        assert_eq!(parse_rate("0/0"), None);
    }

    #[test]
    fn keyframes_are_relative_to_the_start_time_and_sorted() {
        // Packets arrive in decode order; only the `K` ones count, and the 0.25 s start is removed.
        let packets = "1.250000,K__\n1.500000,___\n0.250000,K_\n2.250000,K__\n";
        let keys = parse_keyframes(packets, Hns::from_millis(250));
        assert_eq!(keys, [Hns::ZERO, Hns::from_secs(1), Hns::from_secs(2)]);
    }

    #[test]
    fn f32_samples_decodes_little_endian_and_drops_a_partial_tail() {
        let mut bytes = Vec::new();
        bytes.extend(0.5_f32.to_le_bytes());
        bytes.extend((-0.25_f32).to_le_bytes());
        bytes.push(0xff);
        assert_eq!(f32_samples(&bytes), [0.5, -0.25]);
    }

    #[test]
    fn locate_refuses_a_missing_configured_path() {
        let missing = std::env::temp_dir().join("zc-no-such-ffmpeg-for-tests.exe");
        assert!(matches!(Ffmpeg::locate(Some(&missing)), Err(FfmpegError::NotFound)));
    }

    #[test]
    fn locate_accepts_a_configured_file() {
        let path = std::env::temp_dir().join(format!("zc-fake-ffmpeg-{}.exe", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        let found = Ffmpeg::locate(Some(&path));
        let _ = std::fs::remove_file(&path);
        assert_eq!(found.unwrap().exe, path);
    }
}
