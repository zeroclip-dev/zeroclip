//! The Editor's pure logic: the timeline of pieces, time and pixel mapping, the export plan built
//! from it, and output file names. No GPUI and no ffmpeg here, so all of it is unit-tested.
//!
//! The timeline is a row of pieces, each a range of one source clip (A, B, C...). Splitting cuts a
//! piece in two, deleting removes one and closes the gap, and every piece keeps its own volume
//! and mute per audio track. Video is stream-copied, so every cut lands on a keyframe.

use std::path::{Path, PathBuf};

use zc_core::Hns;
use zc_editor::ffmpeg::{AudioEdit, AudioTrack};
use zc_editor::snap::snap_in_point;

/// The shortest piece a split may leave.
pub const MIN_PIECE: Hns = Hns::from_millis(100);

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

/// The part of `peaks` (which spans `duration`) that covers `[a, b)`.
pub fn peaks_between(peaks: &[f32], duration: Hns, a: Hns, b: Hns) -> &[f32] {
    if peaks.is_empty() || duration <= Hns::ZERO {
        return &[];
    }
    let at = |t: Hns| {
        let f = (t.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0);
        ((f * peaks.len() as f64) as usize).min(peaks.len())
    };
    let (from, to) = (at(a), at(b));
    &peaks[from..to.max(from)]
}

/// "12.7 s".
pub fn secs_label(t: Hns) -> String {
    format!("{:.1} s", t.as_secs_f64())
}

/// One audio track of one piece, as the UI holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackEdit {
    pub gain_db: f64,
    /// A muted piece of a track is "deleted": it plays as silence and keeps its length.
    pub mute: bool,
}

impl Default for TrackEdit {
    fn default() -> Self {
        Self { gain_db: 0.0, mute: false }
    }
}

impl TrackEdit {
    /// What ffmpeg does to the track.
    pub fn audio_edit(self) -> AudioEdit {
        if self.mute {
            AudioEdit::Mute
        } else if self.gain_db != 0.0 {
            AudioEdit::Gain { db: self.gain_db }
        } else {
            AudioEdit::Keep
        }
    }
}

/// A range `[a, b)` of source clip `source`, with its own audio edits.
#[derive(Clone, Debug, PartialEq)]
pub struct Piece {
    /// Index of the clip in the editor's list of sources.
    pub source: usize,
    pub a: Hns,
    pub b: Hns,
    /// One per audio track of the source.
    pub tracks: Vec<TrackEdit>,
}

impl Piece {
    pub fn span(&self) -> Hns {
        self.b - self.a
    }
}

/// The pieces in play order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    pub pieces: Vec<Piece>,
}

impl Timeline {
    /// Total length of every piece.
    pub fn total(&self) -> Hns {
        self.pieces.iter().fold(Hns::ZERO, |sum, p| sum + p.span())
    }

    /// Where piece `i` starts on the timeline.
    pub fn start_of(&self, i: usize) -> Hns {
        self.pieces.iter().take(i).fold(Hns::ZERO, |sum, p| sum + p.span())
    }

    /// The piece at timeline time `t` and the matching source time. The very end belongs to the
    /// last piece.
    pub fn locate(&self, t: Hns) -> Option<(usize, Hns)> {
        let mut start = Hns::ZERO;
        for (i, p) in self.pieces.iter().enumerate() {
            let end = start + p.span();
            if t < end || (i + 1 == self.pieces.len() && t <= end) {
                return Some((i, p.a + (t - start).max(Hns::ZERO)));
            }
            start = end;
        }
        None
    }

    /// Splits the piece at timeline time `t`, on the keyframe at or before it (`keyframes` are the
    /// source's). Returns the index of the new second piece.
    ///
    /// # Errors
    /// A message when there is no piece there, or no keyframe leaves both halves long enough.
    pub fn split(&mut self, t: Hns, keyframes: &[Hns]) -> Result<usize, String> {
        let (i, at) = self.locate(t).ok_or("There is nothing to split.")?;
        let piece = &self.pieces[i];
        let cut = snap_in_point(keyframes, at).unwrap_or(Hns::ZERO);
        if cut < piece.a + MIN_PIECE || cut > piece.b - MIN_PIECE || at > piece.b - MIN_PIECE {
            return Err("Cuts land on keyframes, and there is none here. Move the playhead \
                        a little further in."
                .to_owned());
        }
        let mut second = piece.clone();
        second.a = cut;
        self.pieces[i].b = cut;
        self.pieces.insert(i + 1, second);
        Ok(i + 1)
    }

    /// Removes piece `i`; the ones after it move up.
    pub fn delete(&mut self, i: usize) {
        if i < self.pieces.len() {
            self.pieces.remove(i);
        }
    }

    /// Moves piece `i` one place left (`-1`) or right (`1`). Returns its new index.
    pub fn shift(&mut self, i: usize, by: isize) -> Option<usize> {
        let j =
            i.checked_add_signed(by).filter(|&j| j < self.pieces.len() && i < self.pieces.len())?;
        self.pieces.swap(i, j);
        Some(j)
    }
}

/// "A", "B", ... "Z", "AA", "AB"... for piece `i`.
pub fn piece_label(i: usize) -> String {
    let mut n = i;
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a cut starting at `a` really starts (the keyframe at or before it).
pub fn snapped_start(keyframes: &[Hns], a: Hns) -> Hns {
    snap_in_point(keyframes, a).unwrap_or(Hns::ZERO)
}

/// What the export runs for one piece: an optional stream-copy trim, then its audio edits.
#[derive(Clone, Debug, PartialEq)]
pub struct PiecePlan {
    pub source: usize,
    /// `(start, end)` when the piece is not the whole source clip.
    pub trim: Option<(Hns, Hns)>,
    /// One entry per audio track of the source.
    pub tracks: Vec<AudioTrack>,
}

impl PiecePlan {
    /// Whether any audio track is re-encoded.
    pub fn has_audio_edits(&self) -> bool {
        self.tracks.iter().any(|t| t.edit != AudioEdit::Keep)
    }
}

/// The plan for `piece`, whose source is `duration` long and has `keyframes`.
///
/// # Errors
/// When the piece is empty.
pub fn plan_piece(
    piece: &Piece,
    duration: Hns,
    keyframes: &[Hns],
    bitrate_bps: u32,
) -> Result<PiecePlan, String> {
    let start = snapped_start(keyframes, piece.a);
    if piece.b <= start {
        return Err("A piece is empty.".to_owned());
    }
    let whole = start == Hns::ZERO && piece.b >= duration;
    let tracks =
        piece.tracks.iter().map(|t| AudioTrack { edit: t.audio_edit(), bitrate_bps }).collect();
    Ok(PiecePlan { source: piece.source, trim: (!whole).then_some((start, piece.b)), tracks })
}

/// `<stem> (<tag>).mp4` in `dir`, or `(<tag> 2)`, `(<tag> 3)`... when `exists` says the name is
/// taken. Nothing already on disk is ever overwritten.
pub fn numbered_name(dir: &Path, stem: &str, tag: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let mut n = 1;
    loop {
        let tag = if n == 1 { tag.to_owned() } else { format!("{tag} {n}") };
        let candidate = dir.join(format!("{stem} ({tag}).mp4"));
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// A source's file name without its extension.
pub fn stem_of(path: &Path) -> String {
    path.file_stem().map_or_else(|| "clip".into(), |s| s.to_string_lossy().into_owned())
}

/// The scratch name for an intermediate file beside `output`, never colliding with it.
pub fn scratch_name(output: &Path, tag: &str) -> PathBuf {
    output.with_file_name(format!("{}.zc-{tag}-{}.mp4", stem_of(output), std::process::id()))
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

    fn one(duration: i64) -> Timeline {
        Timeline {
            pieces: vec![Piece {
                source: 0,
                a: Hns::ZERO,
                b: secs(duration),
                tracks: vec![TrackEdit::default(); 2],
            }],
        }
    }

    #[test]
    fn time_and_x_map_both_ways() {
        let d = secs(100);
        assert_eq!(time_to_x(secs(50), d, 400.0), 200.0);
        assert_eq!(x_to_time(100.0, d, 400.0), secs(25));
        assert_eq!(time_to_x(secs(150), d, 400.0), 400.0);
        assert_eq!(x_to_time(-10.0, d, 400.0), Hns::ZERO);
        assert_eq!(time_to_x(secs(1), Hns::ZERO, 400.0), 0.0);
        assert_eq!(x_to_time(10.0, d, 0.0), Hns::ZERO);
    }

    #[test]
    fn downsample_keeps_the_loudest_of_each_group() {
        assert_eq!(downsample(&[0.1, 0.9, 0.2, 0.3], 2), [0.9, 0.3]);
        assert_eq!(downsample(&[0.5], 10), [0.5]);
        assert!(downsample(&[], 4).is_empty());
    }

    #[test]
    fn a_piece_takes_its_share_of_the_peaks() {
        let peaks: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(peaks_between(&peaks, secs(10), secs(2), secs(5)), [2.0, 3.0, 4.0]);
        assert!(peaks_between(&[], secs(10), secs(0), secs(5)).is_empty());
        assert!(peaks_between(&peaks, secs(10), secs(7), secs(3)).is_empty());
    }

    #[test]
    fn splitting_cuts_on_the_keyframe_before_the_playhead() {
        let mut t = one(20);
        let second = t.split(Hns::from_millis(5_300), &keyframes()).unwrap();
        assert_eq!(second, 1);
        assert_eq!((t.pieces[0].a, t.pieces[0].b), (Hns::ZERO, secs(4)));
        assert_eq!((t.pieces[1].a, t.pieces[1].b), (secs(4), secs(20)));
        // Nothing is lost or repeated.
        assert_eq!(t.total(), secs(20));
    }

    #[test]
    fn a_split_with_no_keyframe_between_is_refused() {
        let mut t = one(20);
        assert!(t.split(Hns::from_millis(300), &keyframes()).is_err());
        assert!(t.split(secs(20), &keyframes()).is_err());
        assert_eq!(t.pieces.len(), 1);
    }

    #[test]
    fn locate_maps_timeline_time_into_the_right_piece() {
        let mut t = one(20);
        t.split(secs(10), &keyframes()).unwrap();
        t.delete(0); // the piece that was 10..20 s is now first
        assert_eq!(t.locate(secs(3)), Some((0, secs(13))));
        assert_eq!(t.locate(secs(10)), Some((0, secs(20))));
        assert_eq!(t.locate(secs(11)), None);
        assert_eq!(Timeline::default().locate(Hns::ZERO), None);
    }

    #[test]
    fn deleting_closes_the_gap() {
        let mut t = one(20);
        t.split(secs(6), &keyframes()).unwrap();
        t.split(secs(12), &keyframes()).unwrap();
        assert_eq!(t.pieces.len(), 3);
        t.delete(1);
        assert_eq!(t.total(), secs(14));
        assert_eq!(t.start_of(1), secs(6));
    }

    #[test]
    fn shifting_swaps_neighbours_and_stops_at_the_ends() {
        let mut t = one(20);
        t.split(secs(6), &keyframes()).unwrap();
        assert_eq!(t.shift(0, 1), Some(1));
        assert_eq!(t.pieces[1].a, Hns::ZERO);
        assert_eq!(t.shift(1, 1), None);
        assert_eq!(t.shift(0, -1), None);
    }

    #[test]
    fn labels_run_a_to_z_then_aa() {
        assert_eq!(piece_label(0), "A");
        assert_eq!(piece_label(2), "C");
        assert_eq!(piece_label(25), "Z");
        assert_eq!(piece_label(26), "AA");
        assert_eq!(piece_label(27), "AB");
    }

    #[test]
    fn an_untouched_whole_piece_is_neither_trimmed_nor_re_encoded() {
        let plan = plan_piece(&one(20).pieces[0], secs(20), &keyframes(), 160_000).unwrap();
        assert_eq!(plan.trim, None);
        assert!(!plan.has_audio_edits());
    }

    #[test]
    fn a_partial_piece_trims_and_a_muted_track_is_re_encoded() {
        let mut t = one(20);
        t.split(secs(6), &keyframes()).unwrap();
        t.pieces[1].tracks[1].mute = true;
        t.pieces[1].tracks[0].gain_db = 3.0;
        let plan = plan_piece(&t.pieces[1], secs(20), &keyframes(), 96_000).unwrap();
        assert_eq!(plan.trim, Some((secs(6), secs(20))));
        assert_eq!(plan.tracks[0].edit, AudioEdit::Gain { db: 3.0 });
        assert_eq!(plan.tracks[1].edit, AudioEdit::Mute);
        assert!(plan.has_audio_edits());
    }

    #[test]
    fn an_empty_piece_is_refused() {
        let piece = Piece { source: 0, a: secs(6), b: secs(6), tracks: vec![] };
        assert!(plan_piece(&piece, secs(20), &keyframes(), 1).is_err());
    }

    #[test]
    fn copies_never_take_an_existing_name() {
        let dir = Path::new("C:/clips");
        assert_eq!(
            numbered_name(dir, "Ace", "edited", |_| false),
            Path::new("C:/clips/Ace (edited).mp4")
        );
        let taken = [Path::new("C:/clips/Ace (edited).mp4")];
        assert_eq!(
            numbered_name(dir, "Ace", "edited", |p| taken.contains(&p)),
            Path::new("C:/clips/Ace (edited 2).mp4")
        );
    }

    #[test]
    fn scratch_file_sits_beside_the_output() {
        let scratch = scratch_name(Path::new("d/Ace (edited).mp4"), "trim");
        assert_eq!(scratch.parent(), Some(Path::new("d")));
        assert_ne!(scratch, Path::new("d/Ace (edited).mp4"));
    }
}
