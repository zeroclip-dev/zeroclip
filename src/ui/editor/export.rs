//! Turning pieces of the timeline into files with ffmpeg. Runs on a background thread. Video is
//! stream-copied; only a piece with an audio edit has that track re-encoded.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use zc_core::Hns;
use zc_core::media::Codec;
use zc_editor::concat::{StreamInfo, check_compatible};
use zc_editor::ffmpeg::{Ffmpeg, FfmpegError, GapSpec, trim_args, trim_exact_args};

use super::model::{PiecePlan, scratch_name};

/// One piece made into a file. A `scratch` file is ours to delete; the source clip never is.
struct Made {
    path: PathBuf,
    scratch: bool,
}

/// Converts single-audio-track trim args (`-map 0:a:0?`) into all-audio-tracks trim args
/// (`-map 0:a?`) so an intermediate trimmed file preserves tracks `1..` before
/// [`Ffmpeg::edit_audio_rebuilding_master`] mixes them into a new Master track.
fn preserve_all_audio_tracks(mut args: Vec<OsString>) -> Vec<OsString> {
    for arg in &mut args {
        if arg == "0:a:0?" {
            *arg = OsString::from("0:a?");
        }
    }
    args
}

/// Runs `ffmpeg` directly for a multi-track trim scratch step when rebuilding Master.
fn run_ffmpeg_args(ffmpeg: &Ffmpeg, args: &[OsString]) -> Result<(), FfmpegError> {
    let mut cmd = Command::new(&ffmpeg.exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (winbase.h) so release GUI builds never flash a console window.
        // https://learn.microsoft.com/windows/win32/procthread/process-creation-flags
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines[lines.len().saturating_sub(20)..].join("\n");
        Err(FfmpegError::Failed { stderr: tail })
    }
}

/// Trims `source` to `[start, end)` preserving every audio track (`0:a?`) so a subsequent
/// [`Ffmpeg::edit_audio_rebuilding_master`] pass still has tracks `1..` available.
fn trim_preserving_all_audio(
    ffmpeg: &Ffmpeg,
    source: &Path,
    start: Hns,
    end: Hns,
    output: &Path,
    exact: Option<Codec>,
) -> Result<(), FfmpegError> {
    let args = match exact {
        Some(codec) => trim_exact_args(source, start, end, output, codec)?,
        None => trim_args(source, start, end, output)?,
    };
    run_ffmpeg_args(ffmpeg, &preserve_all_audio_tracks(args))
}

/// What the ffmpeg-made gap of `plan` must look like to join the clip pieces beside it: the
/// clip's codec, size and frame rate, and as many silent audio tracks as the pieces carry.
fn gap_spec(plan: &PiecePlan, info: &StreamInfo) -> GapSpec {
    GapSpec {
        codec: info.codec,
        size: info.size,
        fps: info.fps,
        audio_tracks: plan.tracks.len(),
        audio_bitrate_bps: plan.tracks.first().map_or(160_000, |t| t.bitrate_bps),
        audio_rate: info.audio_rate,
        audio_channels: info.audio_channels,
        duration: plan.range.1 - plan.range.0,
    }
}

/// Makes one piece into a file next to `output`. A piece that is the whole clip with no edits is
/// the source file itself.
///
/// `exact` is the video codec when the video is re-encoded to start on the exact frame. Every
/// scratch path is pushed to `created` before ffmpeg writes it, so a failure leaves nothing behind
/// that the caller can't delete.
#[expect(clippy::too_many_arguments, reason = "one call site; a struct would only rename them")]
fn make(
    ffmpeg: &Ffmpeg,
    plan: &PiecePlan,
    source: &Path,
    output: &Path,
    info: &StreamInfo,
    tag: &str,
    exact: Option<Codec>,
    created: &mut Vec<PathBuf>,
) -> Result<Made> {
    if plan.gap {
        // Black and silent from ffmpeg's own generators; no source file is read.
        let path = scratch_name(output, &format!("{tag}g"));
        created.push(path.clone());
        ffmpeg.make_gap(&gap_spec(plan, info), &path)?;
        return Ok(Made { path, scratch: true });
    }
    let keep_all_audio = plan.rebuild_master && plan.tracks.len() > 1;
    let trimmed = match (exact, plan.trim) {
        (Some(codec), _) => {
            let path = scratch_name(output, &format!("{tag}t"));
            created.push(path.clone());
            if keep_all_audio {
                trim_preserving_all_audio(
                    ffmpeg,
                    source,
                    plan.range.0,
                    plan.range.1,
                    &path,
                    Some(codec),
                )?;
            } else {
                ffmpeg.trim_exact(source, plan.range.0, plan.range.1, &path, codec)?;
            }
            Made { path, scratch: true }
        }
        (None, Some((start, end))) => {
            let path = scratch_name(output, &format!("{tag}t"));
            created.push(path.clone());
            if keep_all_audio {
                trim_preserving_all_audio(ffmpeg, source, start, end, &path, None)?;
            } else {
                ffmpeg.trim(source, start, end, &path)?;
            }
            Made { path, scratch: true }
        }
        (None, None) => Made { path: source.to_owned(), scratch: false },
    };
    if !plan.has_audio_edits() {
        return Ok(trimmed);
    }
    let path = scratch_name(output, &format!("{tag}a"));
    created.push(path.clone());
    let edited = if keep_all_audio {
        let master_bitrate_bps = plan.tracks[0].bitrate_bps;
        ffmpeg.edit_audio_rebuilding_master(&trimmed.path, &path, &plan.tracks, master_bitrate_bps)
    } else {
        ffmpeg.edit_audio(&trimmed.path, &path, &plan.tracks)
    };
    if trimmed.scratch {
        let _ = std::fs::remove_file(&trimmed.path);
    }
    edited?;
    Ok(Made { path, scratch: true })
}

/// Writes `plans` (in order) as one file at `output`. `sources` and `infos` are indexed by
/// `PiecePlan::source`. A failed run leaves no half-written output behind.
///
/// # Errors
/// An ffmpeg or file error; pieces from clips whose streams differ can't be joined without re-encoding.
pub fn run(
    ffmpeg: &Ffmpeg,
    plans: &[PiecePlan],
    sources: &[PathBuf],
    infos: &[StreamInfo],
    output: &Path,
) -> Result<Duration> {
    // Never touch a file that is already there: the cleanup below deletes `output` on failure and
    // the final rename would overwrite it. Callers pick a free name, so this is only a race.
    if output.exists() {
        anyhow::bail!("{} already exists", output.display());
    }
    let started = Instant::now();
    // Scratch names are unique per run, so a leftover from an earlier attempt never blocks `-n`.
    static RUNS: AtomicU32 = AtomicU32::new(0);
    let run = RUNS.fetch_add(1, Ordering::Relaxed);
    // A gap is made from scratch by ffmpeg, so only re-encoding every clip piece the same way
    // (same codec, pixel format, parameter sets) gives one set of video parameters at the seams.
    // The same goes for a piece off a keyframe. A plain stream copy of the rest is safe.
    let exact = plans.iter().any(|p| p.reencode || p.gap);
    let mut created = Vec::new();
    let result = build(ffmpeg, plans, sources, infos, output, exact, run, &mut created);
    let failed = result.is_err();
    discard(&mut created, failed.then_some(output));
    result.map(|()| started.elapsed())
}

/// Deletes our scratch files, and `output` when a failed run left it half-written.
fn discard(created: &mut Vec<PathBuf>, output: Option<&Path>) {
    for path in created.drain(..) {
        // The scratch files are ours (one may already be renamed to `output`); failing to delete
        // one must not fail the export.
        let _ = std::fs::remove_file(path);
    }
    if let Some(output) = output {
        let _ = std::fs::remove_file(output);
    }
}

/// One go at writing `plans` as `output`; `exact` re-encodes every clip piece's video.
#[expect(clippy::too_many_arguments, reason = "one call site; a struct would only rename them")]
fn build(
    ffmpeg: &Ffmpeg,
    plans: &[PiecePlan],
    sources: &[PathBuf],
    infos: &[StreamInfo],
    output: &Path,
    exact: bool,
    run: u32,
    created: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut made: Vec<Made> = Vec::new();
    for (n, plan) in plans.iter().enumerate() {
        let info = &infos[plan.source];
        let codec = exact.then_some(info.codec);
        let tag = format!("{run}-{n}");
        made.push(make(ffmpeg, plan, &sources[plan.source], output, info, &tag, codec, created)?);
    }
    match made.as_slice() {
        [] => anyhow::bail!("nothing to save"),
        [only] if only.scratch => std::fs::rename(&only.path, output)?,
        [only] => ffmpeg.remux_for_export(&only.path, output)?,
        many => {
            // Each piece carries only the audio tracks its plan lists, so those are the ones compared.
            let used: Vec<StreamInfo> = plans
                .iter()
                .map(|p| StreamInfo { audio_tracks: p.tracks.len() as u8, ..infos[p.source] })
                .collect();
            check_compatible(&used)?;
            let paths: Vec<PathBuf> = many.iter().map(|m| m.path.clone()).collect();
            ffmpeg.concat(&paths, output)?;
        }
    }
    // Opening the output proves it is a real file before we report success.
    ffmpeg.probe(output).map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_all_audio_tracks_rewrites_master_only_map() {
        let raw = trim_args(
            Path::new("in.mp4"),
            Hns::from_secs(1),
            Hns::from_secs(5),
            Path::new("out.mp4"),
        )
        .unwrap();
        let all = preserve_all_audio_tracks(raw);
        let strs: Vec<String> = all.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(strs.windows(2).any(|w| w == ["-map", "0:a?"]));
        assert!(!strs.iter().any(|s| s == "0:a:0?"));
    }

    #[test]
    fn a_gap_copies_the_clips_format_and_the_pieces_track_count() {
        let info = StreamInfo {
            codec: Codec::H264,
            size: (2560, 1440),
            fps: (144, 1),
            audio_tracks: 3,
            audio_rate: 44_100,
            audio_channels: 1,
        };
        let plan = PiecePlan {
            source: 0,
            trim: None,
            range: (Hns::ZERO, Hns::from_secs(2)),
            reencode: false,
            rebuild_master: false,
            tracks: vec![zc_editor::ffmpeg::AudioTrack {
                edit: zc_editor::ffmpeg::AudioEdit::Keep,
                bitrate_bps: 96_000,
            }],
            gap: true,
        };
        let spec = gap_spec(&plan, &info);
        assert_eq!((spec.codec, spec.size, spec.fps), (Codec::H264, (2560, 1440), (144, 1)));
        assert_eq!((spec.audio_rate, spec.audio_channels), (44_100, 1));
        assert_eq!((spec.audio_tracks, spec.audio_bitrate_bps), (1, 96_000));
        assert_eq!(spec.duration, Hns::from_secs(2));
    }
}
