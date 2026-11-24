//! Turning pieces of the timeline into files with ffmpeg. Runs on a background thread. Video is
//! stream-copied; only a piece with an audio edit has that track re-encoded (D38).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use zc_editor::concat::{StreamInfo, check_compatible};
use zc_editor::ffmpeg::Ffmpeg;

use super::model::{PiecePlan, scratch_name};

/// One piece made into a file. A `scratch` file is ours to delete; the source clip never is.
struct Made {
    path: PathBuf,
    scratch: bool,
}

/// Makes one piece into a file next to `output`. A piece that is the whole clip with no edits is
/// the source file itself.
fn make(
    ffmpeg: &Ffmpeg,
    plan: &PiecePlan,
    source: &Path,
    output: &Path,
    tag: &str,
) -> Result<Made> {
    let trimmed = match plan.trim {
        Some((start, end)) => {
            let path = scratch_name(output, &format!("{tag}t"));
            ffmpeg.trim(source, start, end, &path)?;
            Made { path, scratch: true }
        }
        None => Made { path: source.to_owned(), scratch: false },
    };
    if !plan.has_audio_edits() {
        return Ok(trimmed);
    }
    let path = scratch_name(output, &format!("{tag}a"));
    let edited = ffmpeg.edit_audio(&trimmed.path, &path, &plan.tracks);
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
    let started = Instant::now();
    let mut made: Vec<Made> = Vec::new();
    let result = (|| {
        for (n, plan) in plans.iter().enumerate() {
            made.push(make(ffmpeg, plan, &sources[plan.source], output, &n.to_string())?);
        }
        match made.as_slice() {
            [] => anyhow::bail!("nothing to save"),
            [only] if only.scratch => std::fs::rename(&only.path, output)?,
            [only] => ffmpeg.remux_for_export(&only.path, output)?,
            many => {
                let used: Vec<StreamInfo> = plans.iter().map(|p| infos[p.source]).collect();
                check_compatible(&used)?;
                let paths: Vec<PathBuf> = many.iter().map(|m| m.path.clone()).collect();
                ffmpeg.concat(&paths, output)?;
            }
        }
        // Opening the output proves it is a real file before we report success (DESIGN §9).
        ffmpeg.probe(output).map(drop)?;
        Ok(())
    })();
    for m in made.iter().filter(|m| m.scratch) {
        // The scratch files are ours; failing to delete one must not fail the export.
        let _ = std::fs::remove_file(&m.path);
    }
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result.map(|()| started.elapsed())
}
