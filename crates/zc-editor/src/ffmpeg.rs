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

    /// Like [`trim`](Self::trim) but frame-exact: `start` may be anywhere, so the video is
    /// re-encoded (visually lossless) as `codec`; audio and other streams are copied.
    ///
    /// # Errors
    /// See [`FfmpegError`]; AV1 isn't supported here.
    pub fn trim_exact(
        &self,
        input: &Path,
        start: Hns,
        end: Hns,
        output: &Path,
        codec: Codec,
    ) -> Result<(), FfmpegError> {
        run(&self.exe, &trim_exact_args(input, start, end, output, codec)?).map(drop)
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
        let audio_names = parse_audio_names(&String::from_utf8_lossy(&streams));
        // A second, small query: the format section of the first one would blur which `duration`
        // belongs to which stream. An unreadable span just means that track gets the whole clip.
        let spans = run(&ffprobe, &audio_span_args(path))
            .map(|out| parse_audio_spans(&String::from_utf8_lossy(&out)))
            .unwrap_or_default();
        let audio_spans = (0..usize::from(stream.audio_tracks))
            .map(|i| spans.get(i).copied().flatten().unwrap_or((Hns::ZERO, duration)))
            .collect();
        Ok(ProbeInfo { stream, duration, keyframes, audio_names, audio_spans })
    }

    /// Writes the frame of `input` at `at` to `output` as a small JPEG, for the clip list.
    ///
    /// # Errors
    /// See [`FfmpegError`]. A time past the end of the clip writes nothing and is not an error,
    /// so callers check that `output` exists.
    pub fn thumbnail(
        &self,
        input: &Path,
        at: Hns,
        width: u32,
        output: &Path,
    ) -> Result<(), FfmpegError> {
        run(&self.exe, &thumbnail_args(input, at, width, output)).map(drop)
    }

    /// Edits audio tracks and stream-copies the video (D38); see [`audio_edit_args`].
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn edit_audio(
        &self,
        input: &Path,
        output: &Path,
        tracks: &[AudioTrack],
    ) -> Result<(), FfmpegError> {
        run(&self.exe, &audio_edit_args(input, output, tracks)?).map(drop)
    }

    /// Decodes the first audio track to mono f32 samples at `sample_rate`, for the waveform.
    /// A clip without audio is an `Err`; callers treat that as "no waveform".
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn audio_samples(&self, path: &Path, sample_rate: u32) -> Result<Vec<f32>, FfmpegError> {
        self.track_samples(path, 0, sample_rate)
    }

    /// Like [`Self::audio_samples`] for audio track `track` (0-based), for a per-track waveform.
    ///
    /// # Errors
    /// See [`FfmpegError`].
    pub fn track_samples(
        &self,
        path: &Path,
        track: usize,
        sample_rate: u32,
    ) -> Result<Vec<f32>, FfmpegError> {
        let mut args = common_args();
        args.push("-i".into());
        args.push(os(path));
        args.push("-map".into());
        args.push(format!("0:a:{track}").into());
        args.extend(["-ac", "1", "-ar"].map(OsString::from));
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
    /// The name of each audio track (the recorder writes Master, Game, Mic, then app labels).
    /// Empty when the file has none, or only the muxer's default.
    pub audio_names: Vec<String>,
    /// Where each audio track starts in the file and how long it runs. The tracks start a few
    /// milliseconds late and can end well before the video, so a waveform is placed by these, not
    /// by the container's length.
    pub audio_spans: Vec<(Hns, Hns)>,
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
        "stream=codec_type,codec_name,width,height,avg_frame_rate:stream_tags=handler_name:format=duration,start_time"
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
/// the container duration and its start time. Each stream block starts with `codec_name`, then
/// `codec_type` (ffprobe's order, checked against real ffprobe 8 output).
fn parse_streams(text: &str) -> Result<(StreamInfo, Hns, Hns), String> {
    let mut video_seen = false;
    let mut in_first_video = false;
    let mut codec_name = None;
    // The codec of the stream block being read. ffprobe prints `codec_name` before
    // `codec_type`, so it is held here until the type says whether it is the first video.
    let mut block_codec = None;
    let mut width = None;
    let mut height = None;
    let mut fps_rate = None;
    let mut audio = 0usize;
    let mut duration = None;
    let mut start = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            // Each stream block starts with `codec_name`.
            "codec_name" => {
                block_codec = Some(value);
                in_first_video = false;
            }
            "codec_type" => {
                in_first_video = value == "video" && !video_seen;
                if in_first_video {
                    codec_name = block_codec;
                }
                video_seen |= value == "video";
                if value == "audio" {
                    audio += 1;
                }
            }
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

/// ffprobe arguments for each audio stream's start time and duration, in stream order.
fn audio_span_args(path: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> =
        ["-v", "error", "-select_streams", "a", "-show_entries"].map(OsString::from).to_vec();
    args.push("stream=start_time,duration".into());
    args.extend(["-of", "default=noprint_wrappers=1"].map(OsString::from));
    args.push(os(path));
    args
}

/// Start and length of each audio stream from [`audio_span_args`] output. `None` for a stream
/// whose values ffprobe reports as `N/A`.
fn parse_audio_spans(text: &str) -> Vec<Option<(Hns, Hns)>> {
    let mut spans: Vec<(Option<f64>, Option<f64>)> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "start_time" => spans.push((value.parse().ok(), None)),
            "duration" => {
                if let Some(last) = spans.last_mut() {
                    last.1 = value.parse().ok();
                }
            }
            _ => {}
        }
    }
    spans
        .into_iter()
        .map(|(start, len)| Some((Hns::from_secs_f64(start?), Hns::from_secs_f64(len?))))
        .collect()
}

/// The track name of each audio stream, from the `handler_name` tag, in stream order. A stream
/// with no tag, or only the default `SoundHandler`, gets an empty name.
fn parse_audio_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut in_audio = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "codec_name" => in_audio = false,
            "codec_type" if value == "audio" => {
                in_audio = true;
                names.push(String::new());
            }
            "TAG:handler_name" if in_audio && value != "SoundHandler" => {
                if let Some(last) = names.last_mut() {
                    *last = value.to_owned();
                }
            }
            _ => {}
        }
    }
    names
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
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
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

/// Arguments for [`Ffmpeg::trim_exact`]: seek to `start` before the input (ffmpeg then decodes up
/// to the exact frame), re-encode the video, copy everything else.
///
/// # Errors
/// [`FfmpegError::Failed`] when `end <= start` or the codec can't be re-encoded here.
pub fn trim_exact_args(
    input: &Path,
    start: Hns,
    end: Hns,
    output: &Path,
    codec: Codec,
) -> Result<Vec<OsString>, FfmpegError> {
    if end <= start {
        return Err(FfmpegError::Failed { stderr: "empty selection".into() });
    }
    let video: &[&str] = match codec {
        Codec::Hevc => &["-c:v", "libx265", "-crf", "16", "-preset", "veryfast", "-tag:v", "hvc1"],
        Codec::H264 => &["-c:v", "libx264", "-crf", "16", "-preset", "veryfast"],
        Codec::Av1 => {
            return Err(FfmpegError::Failed {
                stderr: "AV1 clips can only be cut on a keyframe".into(),
            });
        }
    };
    let mut args = common_args();
    args.push("-ss".into());
    args.push(ffmpeg_time(start).into());
    args.push("-i".into());
    args.push(os(input));
    args.push("-t".into());
    args.push(ffmpeg_time(end - start).into());
    args.extend(["-map", "0", "-c", "copy"].map(OsString::from));
    args.extend(video.iter().map(OsString::from));
    args.extend(["-avoid_negative_ts", "make_zero", "-movflags", "+faststart"].map(OsString::from));
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

/// Arguments that decode the first video frame at `at` and write it to `output` (a `.jpg`),
/// scaled to `width` px wide. Seeking before `-i` is fast and lands on the nearest earlier keyframe.
#[must_use]
pub fn thumbnail_args(input: &Path, at: Hns, width: u32, output: &Path) -> Vec<OsString> {
    let mut args = common_args();
    args.push("-ss".into());
    args.push(ffmpeg_time(at).into());
    args.push("-i".into());
    args.push(os(input));
    args.extend(["-map", "0:v:0", "-frames:v", "1", "-vf"].map(OsString::from));
    args.push(format!("scale={width}:-2").into());
    args.extend(["-q:v", "5"].map(OsString::from));
    args.push(os(output));
    args
}

/// Arguments that decode video from `at` onward and write raw BGRA frames to stdout, `fps` a
/// second, each letterboxed to exactly `width` x `height`. `max_frames` stops after that many
/// frames (one makes a still). The editor's player reads these frames.
#[must_use]
pub fn frames_args(
    input: &Path,
    at: Hns,
    width: u32,
    height: u32,
    fps: u32,
    max_frames: Option<u32>,
) -> Vec<OsString> {
    let mut args = common_args();
    args.push("-ss".into());
    args.push(ffmpeg_time(at).into());
    args.push("-i".into());
    args.push(os(input));
    args.extend(["-an", "-sn", "-map", "0:v:0", "-vf"].map(OsString::from));
    args.push(
        format!(
            "fps={fps},scale={width}:{height}:force_original_aspect_ratio=decrease,\
             pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:black"
        )
        .into(),
    );
    if let Some(n) = max_frames {
        args.push("-frames:v".into());
        args.push(n.to_string().into());
    }
    args.extend(["-pix_fmt", "bgra", "-f", "rawvideo", "pipe:1"].map(OsString::from));
    args
}

/// What the editor does to one audio track (D38). Anything but `Keep` re-encodes that track to
/// AAC; video is never re-encoded.
#[derive(Clone, Debug, PartialEq)]
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
    /// A volume that follows a line through `points`: straight between them in decibels, flat
    /// before the first and after the last. At least two, in time order. A level at or below
    /// [`SILENT_DB`] is silence.
    Envelope {
        /// The points of the line.
        points: Vec<EnvelopePoint>,
    },
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

/// The level at and below which an [`AudioEdit::Envelope`] is silent, in decibels.
pub const SILENT_DB: f64 = -60.0;

/// One point of a volume line: a time into the piece (seconds) and a level (decibels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopePoint {
    /// Seconds from the start of the piece.
    pub secs: f64,
    /// Decibels; finite.
    pub db: f64,
}

/// One audio track of the source, with its bitrate so a re-encode can match it.
#[derive(Clone, Debug, PartialEq)]
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
        if let Some(chain) = track_chain(i, &track.edit)? {
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
fn track_chain(i: usize, edit: &AudioEdit) -> Result<Option<String>, FfmpegError> {
    let input = format!("[0:a:{i}]");
    let out = format!("[a{i}]");
    let chain = match *edit {
        AudioEdit::Keep => return Ok(None),
        AudioEdit::Gain { db } => {
            if !db.is_finite() {
                return Err(bad_audio("gain must be a finite number of decibels"));
            }
            format!("{input}volume={db}dB{out}")
        }
        AudioEdit::Mute => format!("{input}volume=0{out}"),
        AudioEdit::Envelope { ref points } => {
            format!("{input}volume=volume='{}':eval=frame{out}", envelope_expr(points)?)
        }
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

/// The `volume` filter that follows a volume line, for a live preview (`t` starts at 0).
pub fn envelope_filter(points: &[EnvelopePoint]) -> Result<String, FfmpegError> {
    Ok(format!("volume=volume='{}':eval=frame", envelope_expr(points)?))
}

/// The `volume` filter expression for a volume line: the gain (linear) at time `t`. The level in
/// decibels is a chain of `if(lt(t,..))` straight pieces, and silence is written as exactly 0.
/// Commas are escaped (`\,`) so they don't end the filter option.
fn envelope_expr(points: &[EnvelopePoint]) -> Result<String, FfmpegError> {
    if points.len() < 2 {
        return Err(bad_audio("a volume line needs at least two points"));
    }
    if points.iter().any(|p| !p.secs.is_finite() || !p.db.is_finite() || p.secs < 0.0)
        || points.windows(2).any(|w| w[1].secs <= w[0].secs)
    {
        return Err(bad_audio("volume line points must be finite and in time order"));
    }
    let last = points[points.len() - 1];
    let mut db = format!("{:.3}", last.db);
    for w in points.windows(2).rev() {
        let (a, b) = (w[0], w[1]);
        db = format!(
            r"if(lt(t\,{:.4})\,{:.3}+({:.3})*(t-{:.4})/{:.4}\,{db})",
            b.secs,
            a.db,
            b.db - a.db,
            a.secs,
            b.secs - a.secs
        );
    }
    let first = points[0];
    let db = format!(r"if(lt(t\,{:.4})\,{:.3}\,{db})", first.secs, first.db);
    Ok(format!(r"if(lte({db}\,{SILENT_DB})\,0\,pow(10\,({db})/20))"))
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
    fn thumbnail_takes_one_scaled_frame_and_ends_with_the_output() {
        let args = strs(&thumbnail_args(
            Path::new("in.mp4"),
            Hns::from_millis(500),
            192,
            Path::new("out.jpg"),
        ));
        assert!(args.contains(&"scale=192:-2".to_string()));
        let at = args.iter().position(|a| a == "-ss").unwrap();
        assert_eq!(args[at + 1], "0.5000000");
        assert!(args.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert_eq!(args.last().map(String::as_str), Some("out.jpg"));
    }

    #[test]
    fn frames_are_raw_bgra_of_a_fixed_size_on_stdout() {
        let still =
            strs(&frames_args(Path::new("in.mp4"), Hns::from_millis(500), 640, 360, 24, Some(1)));
        assert!(still.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert!(still.windows(2).any(|w| w == ["-pix_fmt", "bgra"]));
        assert!(still.iter().any(|a| a.contains("pad=640:360")));
        assert_eq!(still.last().map(String::as_str), Some("pipe:1"));
        let open = strs(&frames_args(Path::new("in.mp4"), Hns::ZERO, 640, 360, 24, None));
        assert!(!open.iter().any(|a| a == "-frames:v"));
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
    fn an_envelope_becomes_a_timed_volume_expression() {
        let points =
            vec![EnvelopePoint { secs: 0.0, db: 0.0 }, EnvelopePoint { secs: 2.0, db: -60.0 }];
        let tracks = [AudioTrack { edit: AudioEdit::Envelope { points }, bitrate_bps: 128_000 }];
        let args = audio_edit_args(Path::new("in.mp4"), Path::new("o.mp4"), &tracks).unwrap();
        let chain = &strs(&args)[8];
        assert!(chain.starts_with("[0:a:0]volume=volume='if(lte("), "{chain}");
        assert!(chain.ends_with(":eval=frame[a0]"), "{chain}");
        assert!(chain.contains(r"if(lt(t\,2.0000)\,0.000+(-60.000)*(t-0.0000)/2.0000\,-60.000)"));
        let one = [EnvelopePoint { secs: 0.0, db: 0.0 }];
        assert!(envelope_expr(&one).is_err());
        let backwards =
            [EnvelopePoint { secs: 1.0, db: 0.0 }, EnvelopePoint { secs: 1.0, db: -3.0 }];
        assert!(envelope_expr(&backwards).is_err());
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
            let tracks = [AudioTrack { edit: edit.clone(), bitrate_bps: 128_000 }];
            assert!(audio_edit_args(Path::new("i"), Path::new("o"), &tracks).is_err(), "{edit:?}");
        }
    }

    #[test]
    fn concat_line_escapes_single_quotes() {
        assert_eq!(concat_line(Path::new(r"C:\clips\a.mp4")), r"file 'C:\clips\a.mp4'");
        assert_eq!(concat_line(Path::new("it's.mp4")), r"file 'it'\''s.mp4'");
    }

    /// Canned `stream_args` output in ffprobe's real order (`codec_name` first in each block):
    /// an audio track first, then video, then another audio track, then the format.
    const STREAMS: &str = "\
codec_name=aac
codec_type=audio
avg_frame_rate=0/0
codec_name=h264
codec_type=video
width=1920
height=1080
avg_frame_rate=60/1
codec_name=aac
codec_type=audio
avg_frame_rate=0/0
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
    fn audio_spans_pair_each_start_with_its_duration() {
        let text = "start_time=0.020904\nduration=2.132904\nstart_time=N/A\nduration=1.0\n";
        let spans = parse_audio_spans(text);
        assert_eq!(spans[0], Some((Hns::from_secs_f64(0.020904), Hns::from_secs_f64(2.132904))));
        assert_eq!(spans[1], None);
    }

    #[test]
    fn audio_track_names_come_from_the_handler_tags() {
        let text = [
            "codec_name=h264",
            "codec_type=video",
            "codec_name=aac",
            "codec_type=audio",
            "TAG:handler_name=Master",
            "codec_name=aac",
            "codec_type=audio",
            "TAG:handler_name=SoundHandler",
            "codec_name=aac",
            "codec_type=audio",
            "TAG:handler_name=Spotify",
            "duration=5.0",
        ]
        .join(
            "
",
        );
        assert_eq!(parse_audio_names(&text), ["Master", "", "Spotify"]);
    }

    #[test]
    fn parse_streams_rejects_unsupported_codecs_and_missing_video() {
        let vp9 = STREAMS.replace("h264", "vp9");
        assert_eq!(parse_streams(&vp9).unwrap_err(), "unsupported video codec vp9");
        let audio_only = "codec_name=aac\ncodec_type=audio\nduration=1.0\nstart_time=0.0\n";
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
