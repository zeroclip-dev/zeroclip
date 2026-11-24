//! The Editor's pure logic: time and pixel mapping, trim handles, the export plan built from the
//! UI's state, and output file names. No GPUI and no ffmpeg here, so all of it is unit-tested.

use std::path::{Path, PathBuf};

use zc_core::Hns;
use zc_editor::ffmpeg::{AudioEdit, AudioTrack};
use zc_editor::snap::snap_in_point;

/// The shortest selection the handles allow.
pub const MIN_SELECTION: Hns = Hns::from_millis(100);

/// x offset of `t` on a bar `width` px wide that spans `duration`.
pub fn time_to_x(t: Hns, duration: Hns, width: f32) -> f32 {
    if duration <= Hns::ZERO {
        return 0.0;
    }
    (t.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0) as f32 * width
}

/// The time at x offset `x` (clamped to the bar).
pub fn x_to_time(x: f32, duration: Hns, width: f32) -> Hns {
    if width <= 0.0 {
        return Hns::ZERO;
    }
    let fraction = f64::from((x / width).clamp(0.0, 1.0));
    Hns::from_secs_f64(duration.as_secs_f64() * fraction)
}

/// Which end of the selection a drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    In,
    Out,
}

/// The handle closest to `x`. When both sit at the same spot, the side `x` is on decides.
pub fn nearest_handle(x: f32, in_x: f32, out_x: f32) -> Handle {
    let (d_in, d_out) = ((x - in_x).abs(), (x - out_x).abs());
    if d_in < d_out || (d_in == d_out && x < in_x) { Handle::In } else { Handle::Out }
}

/// Moves `handle` to `t`, keeping the selection inside the clip and at least `MIN_SELECTION` long.
pub fn move_handle(handle: Handle, t: Hns, in_pt: Hns, out_pt: Hns, duration: Hns) -> (Hns, Hns) {
    match handle {
        Handle::In => (t.max(Hns::ZERO).min((out_pt - MIN_SELECTION).max(Hns::ZERO)), out_pt),
        Handle::Out => (in_pt, t.min(duration).max((in_pt + MIN_SELECTION).min(duration))),
    }
}

/// Keyframe tick positions for a bar `width` px wide, dropping ticks closer than `min_gap` px to
/// the previous one so a long clip doesn't produce thousands of elements.
pub fn tick_xs(keyframes: &[Hns], duration: Hns, width: f32, min_gap: f32) -> Vec<f32> {
    let mut xs: Vec<f32> = Vec::new();
    for &k in keyframes {
        let x = time_to_x(k, duration, width);
        if xs.last().is_none_or(|last| x - last >= min_gap) {
            xs.push(x);
        }
    }
    xs
}

/// Reduces `peaks` to `columns` values (the loudest peak of each group) for drawing.
pub fn downsample(peaks: &[f32], columns: usize) -> Vec<f32> {
    if peaks.is_empty() || columns == 0 {
        return Vec::new();
    }
    let columns = columns.min(peaks.len());
    (0..columns)
        .map(|c| {
            let span = &peaks[c * peaks.len() / columns..(c + 1) * peaks.len() / columns];
            span.iter().copied().fold(0.0_f32, f32::max)
        })
        .collect()
}

/// Seconds typed by the user ("12.5"), or `None` if it isn't a finite, non-negative number.
pub fn parse_seconds(text: &str) -> Option<Hns> {
    let secs: f64 = text.trim().replace(',', ".").parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Hns::from_secs_f64(secs))
}

/// "12.7 s".
pub fn secs_label(t: Hns) -> String {
    format!("{:.1} s", t.as_secs_f64())
}

/// What a typed cut range does to its track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutMode {
    /// The range goes quiet; the track keeps its length.
    Silence,
    /// The range is cut out of this track only; the track gets shorter.
    Remove,
}

/// One audio track's edits as the UI holds them. Empty cut fields mean no cut.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackEdit {
    pub gain_db: f64,
    pub mute: bool,
    pub cut_start: String,
    pub cut_end: String,
    pub cut_mode: CutMode,
}

impl Default for TrackEdit {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            mute: false,
            cut_start: String::new(),
            cut_end: String::new(),
            cut_mode: CutMode::Silence,
        }
    }
}

/// What the export will run: an optional stream-copy trim, then audio edits per track.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// `(snapped start, end)` when the selection is not the whole clip.
    pub trim: Option<(Hns, Hns)>,
    /// One entry per audio track of the source.
    pub tracks: Vec<AudioTrack>,
}

impl Plan {
    /// Whether any audio track is re-encoded.
    pub fn has_audio_edits(&self) -> bool {
        self.tracks.iter().any(|t| t.edit != AudioEdit::Keep)
    }
}

/// Where a cut starting at `in_pt` really starts (the keyframe at or before it).
pub fn snapped_start(keyframes: &[Hns], in_pt: Hns) -> Hns {
    snap_in_point(keyframes, in_pt).unwrap_or(Hns::ZERO)
}

/// Builds the export plan. Cut ranges are in the original clip's time; they are moved to the
/// trimmed clip's time here, because the audio edits run on the trimmed file.
///
/// # Errors
/// A message for the first problem: a bad cut range, a cut outside the selection, or a track
/// with both a volume change and a cut.
pub fn build_plan(
    duration: Hns,
    keyframes: &[Hns],
    in_pt: Hns,
    out_pt: Hns,
    edits: &[TrackEdit],
    bitrate_bps: u32,
) -> Result<Plan, String> {
    let start = snapped_start(keyframes, in_pt);
    if out_pt <= start {
        return Err("The selection is empty.".to_owned());
    }
    let trim = (start > Hns::ZERO || out_pt < duration).then_some((start, out_pt));
    let mut tracks = Vec::with_capacity(edits.len());
    for (i, edit) in edits.iter().enumerate() {
        let n = i + 1;
        let cut = parse_cut(edit, start, out_pt).map_err(|m| format!("Track {n}: {m}"))?;
        let gain = edit.gain_db != 0.0;
        let edited = if edit.mute {
            AudioEdit::Mute
        } else if let Some((a, b)) = cut {
            if gain {
                return Err(format!("Track {n}: use a volume change or a cut, not both."));
            }
            match edit.cut_mode {
                CutMode::Silence => AudioEdit::Silence { start: a, end: b },
                CutMode::Remove => AudioEdit::Remove { start: a, end: b },
            }
        } else if gain {
            AudioEdit::Gain { db: edit.gain_db }
        } else {
            AudioEdit::Keep
        };
        tracks.push(AudioTrack { edit: edited, bitrate_bps });
    }
    Ok(Plan { trim, tracks })
}

/// The cut range in trimmed-clip time, or `None` when both fields are empty.
fn parse_cut(edit: &TrackEdit, start: Hns, out_pt: Hns) -> Result<Option<(Hns, Hns)>, String> {
    let (a, b) = (edit.cut_start.trim(), edit.cut_end.trim());
    if a.is_empty() && b.is_empty() {
        return Ok(None);
    }
    let a = parse_seconds(a).ok_or("the cut start must be a number of seconds.")?;
    let b = parse_seconds(b).ok_or("the cut end must be a number of seconds.")?;
    if b <= a {
        return Err("the cut must end after it starts.".to_owned());
    }
    let (from, to) = (a.max(start), b.min(out_pt));
    if to <= from {
        return Err("the cut is outside the selected part.".to_owned());
    }
    Ok(Some((from - start, to - start)))
}

/// `<stem> (edited).mp4` next to `source`, or `(edited 2)`, `(edited 3)`... when `exists` says
/// the name is taken. The original is never overwritten.
pub fn edited_name(source: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let stem = source.file_stem().map_or_else(|| "clip".into(), |s| s.to_string_lossy());
    let dir = source.parent().unwrap_or_else(|| Path::new(""));
    let mut n = 1;
    loop {
        let tag = if n == 1 { "edited".to_owned() } else { format!("edited {n}") };
        let candidate = dir.join(format!("{stem} ({tag}).mp4"));
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The scratch name for the trimmed intermediate: beside `output`, never colliding with it.
pub fn trim_scratch(output: &Path) -> PathBuf {
    let stem = output.file_stem().map_or_else(|| "clip".into(), |s| s.to_string_lossy());
    output.with_file_name(format!("{stem}.zc-trim-{}.mp4", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: i64) -> Hns {
        Hns::from_secs(s)
    }

    fn keyframes() -> Vec<Hns> {
        (0..10).map(|i| secs(i * 2)).collect()
    }

    #[test]
    fn time_and_x_map_both_ways() {
        let d = secs(100);
        assert_eq!(time_to_x(secs(50), d, 400.0), 200.0);
        assert_eq!(x_to_time(100.0, d, 400.0), secs(25));
        // Outside the bar clamps; an empty clip doesn't divide by zero.
        assert_eq!(time_to_x(secs(150), d, 400.0), 400.0);
        assert_eq!(x_to_time(-10.0, d, 400.0), Hns::ZERO);
        assert_eq!(time_to_x(secs(1), Hns::ZERO, 400.0), 0.0);
        assert_eq!(x_to_time(10.0, d, 0.0), Hns::ZERO);
    }

    #[test]
    fn the_closer_handle_moves() {
        assert_eq!(nearest_handle(10.0, 20.0, 300.0), Handle::In);
        assert_eq!(nearest_handle(250.0, 20.0, 300.0), Handle::Out);
        // Stacked handles: pull left of them for In, right for Out.
        assert_eq!(nearest_handle(5.0, 100.0, 100.0), Handle::In);
        assert_eq!(nearest_handle(150.0, 100.0, 100.0), Handle::Out);
    }

    #[test]
    fn handles_stay_inside_the_clip_and_apart() {
        let d = secs(10);
        assert_eq!(
            move_handle(Handle::In, secs(9), secs(1), secs(5), d).0,
            secs(5) - MIN_SELECTION
        );
        assert_eq!(
            move_handle(Handle::Out, secs(2), secs(3), secs(8), d).1,
            secs(3) + MIN_SELECTION
        );
        assert_eq!(move_handle(Handle::Out, secs(99), secs(3), secs(8), d).1, d);
        assert_eq!(move_handle(Handle::In, Hns(-5), secs(3), secs(8), d).0, Hns::ZERO);
    }

    #[test]
    fn ticks_thin_out_on_a_narrow_bar() {
        let kf: Vec<Hns> = (0..1000).map(secs).collect();
        let xs = tick_xs(&kf, secs(1000), 500.0, 3.0);
        assert!(xs.len() <= 167, "{}", xs.len());
        assert!(xs.windows(2).all(|w| w[1] - w[0] >= 3.0));
    }

    #[test]
    fn downsample_keeps_the_loudest_of_each_group() {
        assert_eq!(downsample(&[0.1, 0.9, 0.2, 0.3], 2), [0.9, 0.3]);
        assert_eq!(downsample(&[0.5], 10), [0.5]);
        assert!(downsample(&[], 4).is_empty());
    }

    #[test]
    fn seconds_parse_with_comma_and_reject_junk() {
        assert_eq!(parse_seconds(" 12,5 "), Some(Hns::from_millis(12_500)));
        assert_eq!(parse_seconds("-1"), None);
        assert_eq!(parse_seconds("nan"), None);
        assert_eq!(parse_seconds("abc"), None);
    }

    fn edit() -> TrackEdit {
        TrackEdit::default()
    }

    #[test]
    fn untouched_whole_clip_is_not_trimmed_or_re_encoded() {
        let plan =
            build_plan(secs(20), &keyframes(), Hns::ZERO, secs(20), &[edit()], 160_000).unwrap();
        assert_eq!(plan.trim, None);
        assert!(!plan.has_audio_edits());
    }

    #[test]
    fn the_trim_starts_on_the_keyframe_before_the_in_point() {
        let plan =
            build_plan(secs(20), &keyframes(), Hns::from_millis(5_300), secs(12), &[], 0).unwrap();
        assert_eq!(plan.trim, Some((secs(4), secs(12))));
    }

    #[test]
    fn cuts_move_into_trimmed_time_and_clamp_to_the_selection() {
        let mut e = edit();
        e.cut_start = "3".into();
        e.cut_end = "20".into();
        e.cut_mode = CutMode::Remove;
        // Selection 4..12 s: the cut 3..20 s becomes 0..8 s of the trimmed clip.
        let plan =
            build_plan(secs(20), &keyframes(), secs(5), secs(12), std::slice::from_ref(&e), 96_000)
                .unwrap();
        assert_eq!(
            plan.tracks,
            [AudioTrack {
                edit: AudioEdit::Remove { start: Hns::ZERO, end: secs(8) },
                bitrate_bps: 96_000
            }]
        );
        // Entirely before the selection.
        e.cut_start = "0".into();
        e.cut_end = "2".into();
        let err = build_plan(secs(20), &keyframes(), secs(5), secs(12), &[e], 0).unwrap_err();
        assert!(err.starts_with("Track 1:"), "{err}");
    }

    #[test]
    fn mute_gain_and_cut_choose_one_edit_per_track() {
        let mut muted = edit();
        muted.mute = true;
        muted.gain_db = 6.0;
        let mut loud = edit();
        loud.gain_db = 6.0;
        let mut both = edit();
        both.gain_db = 3.0;
        both.cut_start = "1".into();
        both.cut_end = "2".into();
        let plan =
            build_plan(secs(20), &keyframes(), Hns::ZERO, secs(20), &[muted, loud], 1).unwrap();
        assert_eq!(plan.tracks[0].edit, AudioEdit::Mute);
        assert_eq!(plan.tracks[1].edit, AudioEdit::Gain { db: 6.0 });
        assert!(plan.has_audio_edits());
        let err = build_plan(secs(20), &keyframes(), Hns::ZERO, secs(20), &[both], 1).unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn bad_cut_fields_and_empty_selections_are_refused() {
        let mut e = edit();
        e.cut_start = "5".into();
        e.cut_end = "5".into();
        assert!(build_plan(secs(20), &keyframes(), Hns::ZERO, secs(20), &[e.clone()], 1).is_err());
        e.cut_end = "x".into();
        assert!(build_plan(secs(20), &keyframes(), Hns::ZERO, secs(20), &[e], 1).is_err());
        assert!(build_plan(secs(20), &keyframes(), secs(6), secs(6), &[], 1).is_err());
    }

    #[test]
    fn copies_never_take_the_original_name() {
        let src = Path::new("C:/clips/Ace.mp4");
        assert_eq!(edited_name(src, |_| false), Path::new("C:/clips/Ace (edited).mp4"));
        let taken = [Path::new("C:/clips/Ace (edited).mp4")];
        assert_eq!(
            edited_name(src, |p| taken.contains(&p)),
            Path::new("C:/clips/Ace (edited 2).mp4")
        );
        // A .mkv source still exports to .mp4.
        assert_eq!(edited_name(Path::new("a/b.mkv"), |_| false), Path::new("a/b (edited).mp4"));
    }

    #[test]
    fn scratch_file_sits_beside_the_output() {
        let scratch = trim_scratch(Path::new("d/Ace (edited).mp4"));
        assert_eq!(scratch.parent(), Some(Path::new("d")));
        assert_ne!(scratch, Path::new("d/Ace (edited).mp4"));
    }
}
