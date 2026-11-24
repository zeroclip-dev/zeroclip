//! The Editor's pure logic: the timeline of pieces, time and pixel mapping, the export plan built
//! from it, and output file names. No GPUI and no ffmpeg here, so all of it is unit-tested.
//!
//! The timeline is a row of pieces, each a range of one source clip (A, B, C...). Splitting cuts a
//! piece in two, deleting removes one and closes the gap, and every piece keeps its own volume
//! and mute per audio track. Video is stream-copied, so every cut lands on a keyframe.

use std::path::{Path, PathBuf};

use zc_core::Hns;
use zc_editor::ffmpeg::{AudioEdit, AudioTrack, EnvelopePoint, SILENT_DB};
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

/// The loudest peak behind each of `columns` equal slices of `[a, b)` of the clip, for drawing a
/// waveform. `peaks` cover one audio track, which starts at `span.0` and runs for `span.1`: that
/// is not the clip's own length (a track starts a few milliseconds late and may end early), so
/// time is placed by the track's span. A slice outside the track is 0.
pub fn column_peaks(peaks: &[f32], span: (Hns, Hns), a: Hns, b: Hns, columns: usize) -> Vec<f32> {
    if peaks.is_empty() || span.1 <= Hns::ZERO || b <= a || columns == 0 {
        return vec![0.0; columns];
    }
    let (start, len) = (span.0.as_secs_f64(), span.1.as_secs_f64());
    let (a, b) = (a.as_secs_f64(), b.as_secs_f64());
    let index = |t: f64| (t - start) / len * peaks.len() as f64;
    (0..columns)
        .map(|c| {
            let t0 = a + (b - a) * c as f64 / columns as f64;
            let t1 = a + (b - a) * (c + 1) as f64 / columns as f64;
            let (i0, i1) = (index(t0), index(t1));
            if i1 <= 0.0 || i0 >= peaks.len() as f64 {
                return 0.0;
            }
            let from = i0.max(0.0) as usize;
            let to = (i1.ceil() as usize).clamp(from + 1, peaks.len());
            peaks[from..to].iter().copied().fold(0.0_f32, f32::max)
        })
        .collect()
}

/// "12.7 s".
pub fn secs_label(t: Hns) -> String {
    format!("{:.1} s", t.as_secs_f64())
}

/// The loudest and quietest level a volume line may take, in decibels. The bottom is silence.
pub const MIN_DB: f64 = SILENT_DB;
pub const MAX_DB: f64 = 12.0;
/// The closest two points of a volume line may be.
const MIN_GAP: Hns = Hns::from_millis(50);

/// A point of a volume line: where in the piece, and the level there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvPoint {
    /// Time from the start of the piece.
    pub at: Hns,
    pub db: f64,
}

/// A level in dB as a fraction of the lane's height from the top: 0 is the loudest, 1 is silence.
pub fn db_to_frac(db: f64) -> f32 {
    ((MAX_DB - db.clamp(MIN_DB, MAX_DB)) / (MAX_DB - MIN_DB)) as f32
}

/// The inverse of [`db_to_frac`].
pub fn frac_to_db(frac: f32) -> f64 {
    MAX_DB - f64::from(frac.clamp(0.0, 1.0)) * (MAX_DB - MIN_DB)
}

/// One audio track of one piece, as the UI holds it: a flat level, or a line through `points`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackEdit {
    pub gain_db: f64,
    /// A muted piece of a track is "deleted": it plays as silence and keeps its length.
    pub mute: bool,
    /// A volume line, sorted by time, the first at 0 and the last at the end of the piece. Fewer
    /// than two means no line: the track is flat at `gain_db`.
    pub points: Vec<EnvPoint>,
}

impl Default for TrackEdit {
    fn default() -> Self {
        Self { gain_db: 0.0, mute: false, points: Vec::new() }
    }
}

impl TrackEdit {
    /// The flat level: the bottom when muted.
    pub fn flat_db(&self) -> f64 {
        if self.mute { MIN_DB } else { self.gain_db }
    }

    /// Whether the volume follows a line, not a flat level.
    pub fn has_line(&self) -> bool {
        !self.mute && self.points.len() >= 2
    }

    /// Sets the flat level. At the bottom it mutes; anything above unmutes.
    pub fn set_level(&mut self, db: f64) {
        let db = db.clamp(MIN_DB, MAX_DB);
        if db <= MIN_DB + 2.0 {
            self.mute = true;
            self.gain_db = 0.0;
        } else {
            self.mute = false;
            self.gain_db = (db * 10.0).round() / 10.0;
        }
    }

    /// The level `t` into a piece.
    pub fn db_at(&self, t: Hns) -> f64 {
        if !self.has_line() {
            return self.flat_db();
        }
        let p = &self.points;
        if t <= p[0].at {
            return p[0].db;
        }
        for w in p.windows(2) {
            if t <= w[1].at {
                let f = (t - w[0].at).as_secs_f64() / (w[1].at - w[0].at).as_secs_f64();
                return w[0].db + (w[1].db - w[0].db) * f;
            }
        }
        p[p.len() - 1].db
    }

    /// Puts a point at `at` (a piece `span` long) at level `db`; the first one also pins both ends
    /// at the old flat level. Returns the new point's index.
    pub fn add_point(&mut self, at: Hns, db: f64, span: Hns) -> usize {
        if self.points.len() < 2 {
            let base = if self.mute { 0.0 } else { self.gain_db };
            self.mute = false;
            self.points =
                vec![EnvPoint { at: Hns::ZERO, db: base }, EnvPoint { at: span, db: base }];
        }
        let at = at.clamp(MIN_GAP, span - MIN_GAP);
        let i = self.points.partition_point(|p| p.at < at);
        self.points.insert(i, EnvPoint { at, db: db.clamp(MIN_DB, MAX_DB) });
        self.move_point(i, at, db, span);
        i
    }

    /// Moves point `i`. The ends keep their place in time; the others stay between their
    /// neighbours.
    pub fn move_point(&mut self, i: usize, at: Hns, db: f64, span: Hns) {
        let last = self.points.len().saturating_sub(1);
        if i > last {
            return;
        }
        let at = if i == 0 {
            Hns::ZERO
        } else if i == last {
            span
        } else {
            at.clamp(self.points[i - 1].at + MIN_GAP, self.points[i + 1].at - MIN_GAP)
        };
        self.points[i] = EnvPoint { at, db: db.clamp(MIN_DB, MAX_DB) };
    }

    /// Removes point `i`. With nothing left to draw a line through, the track goes flat.
    pub fn remove_point(&mut self, i: usize) {
        if i < self.points.len() {
            self.points.remove(i);
        }
        self.simplify();
    }

    /// Silences `a..b` of a piece `span` long with a dip in the volume line, short ramps either side
    /// so there is no click. The video and the other tracks are untouched.
    pub fn silence(&mut self, a: Hns, b: Hns, span: Hns) {
        if self.mute || b <= a {
            return;
        }
        let (lo, hi) = ((a - MIN_GAP).max(Hns::ZERO), (b + MIN_GAP).min(span));
        let (before, after) = (self.db_at(a), self.db_at(b));
        let old = self.line(span);
        let mut points: Vec<EnvPoint> = old.iter().copied().filter(|p| p.at < lo).collect();
        if lo < a {
            points.push(EnvPoint { at: lo, db: before });
        }
        points.push(EnvPoint { at: a, db: MIN_DB });
        points.push(EnvPoint { at: b, db: MIN_DB });
        if hi > b {
            points.push(EnvPoint { at: hi, db: after });
        }
        points.extend(old.iter().copied().filter(|p| p.at > hi));
        self.mute = false;
        self.points = points;
        self.simplify();
    }

    /// Drops a line that does nothing: fewer than two points, or all at one level.
    pub fn simplify(&mut self) {
        let flat = match self.points.as_slice() {
            [] => return,
            [only] => Some(only.db),
            [first, rest @ ..] if rest.iter().all(|p| (p.db - first.db).abs() < 0.05) => {
                Some(first.db)
            }
            _ => None,
        };
        if let Some(db) = flat {
            self.points.clear();
            self.set_level(db);
        }
    }

    /// The line to draw and hit-test: the points, or the flat level across a piece `span` long.
    pub fn line(&self, span: Hns) -> Vec<EnvPoint> {
        if self.has_line() {
            self.points.clone()
        } else {
            let db = self.flat_db();
            vec![EnvPoint { at: Hns::ZERO, db }, EnvPoint { at: span, db }]
        }
    }

    /// The volume line from `offset` into the piece on, with time counted from `offset`. `None`
    /// when the track has no line.
    pub fn line_from(&self, offset: Hns) -> Option<Vec<EnvelopePoint>> {
        if !self.has_line() {
            return None;
        }
        let start = EnvelopePoint { secs: 0.0, db: self.db_at(offset) };
        let rest = self
            .points
            .iter()
            .filter(|p| p.at > offset + MIN_GAP)
            .map(|p| EnvelopePoint { secs: (p.at - offset).as_secs_f64(), db: p.db });
        let points: Vec<_> = std::iter::once(start).chain(rest).collect();
        (points.len() >= 2).then_some(points)
    }

    /// What ffmpeg does to the track.
    pub fn audio_edit(&self) -> AudioEdit {
        if self.mute {
            AudioEdit::Mute
        } else if self.points.len() >= 2 {
            AudioEdit::Envelope {
                points: self
                    .points
                    .iter()
                    .map(|p| EnvelopePoint { secs: p.at.as_secs_f64(), db: p.db })
                    .collect(),
            }
        } else if self.gain_db != 0.0 {
            AudioEdit::Gain { db: self.gain_db }
        } else {
            AudioEdit::Keep
        }
    }

    /// The two halves when the piece (`span` long) is cut `at` into it. A line is cut with a point
    /// on the cut in both halves, so the sound is the same either side of it.
    pub fn split(&self, at: Hns, span: Hns) -> (Self, Self) {
        if !self.has_line() {
            return (self.clone(), self.clone());
        }
        let db = self.db_at(at);
        let mut left = self.clone();
        left.points = self.points.iter().copied().filter(|p| p.at < at).collect();
        left.points.push(EnvPoint { at, db });
        let mut right = self.clone();
        right.points = std::iter::once(EnvPoint { at: Hns::ZERO, db })
            .chain(
                self.points
                    .iter()
                    .filter(|p| p.at > at)
                    .map(|p| EnvPoint { at: p.at - at, db: p.db }),
            )
            .collect();
        debug_assert!(right.points.last().is_some_and(|p| p.at <= span - at));
        left.simplify();
        right.simplify();
        (left, right)
    }

    /// This track of a piece `len` long followed by `next` of the piece after it: one track for the
    /// joined piece. Equal flat levels stay flat; anything else becomes a line.
    pub fn joined(&self, len: Hns, next: &Self, next_len: Hns) -> Self {
        if !self.has_line() && !next.has_line() && self.flat_db() == next.flat_db() {
            return self.clone();
        }
        let mut points = self.line(len);
        // The step at the join gets a short ramp, so two points never share a time.
        points.extend(
            next.line(next_len)
                .into_iter()
                .map(|p| EnvPoint { at: len + p.at + MIN_GAP, db: p.db }),
        );
        if let Some(p) = points.last_mut() {
            p.at = len + next_len;
        }
        let mut out = Self { gain_db: 0.0, mute: false, points };
        out.simplify();
        out
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
        // A stream copy can only start on a keyframe, so the cut goes on the keyframe nearest the
        // playhead (before or after it) that leaves both pieces long enough.
        let distance = |k: Hns| (k.as_secs_f64() - at.as_secs_f64()).abs();
        let cut = keyframes
            .iter()
            .copied()
            .filter(|&k| k >= piece.a + MIN_PIECE && k <= piece.b - MIN_PIECE)
            .min_by(|&x, &y| distance(x).total_cmp(&distance(y)))
            .ok_or("There is no keyframe inside this piece to cut on, so it can't be split.")?;
        let (span, offset) = (piece.span(), cut - piece.a);
        let halves: Vec<(TrackEdit, TrackEdit)> =
            piece.tracks.iter().map(|t| t.split(offset, span)).collect();
        let mut second = piece.clone();
        second.a = cut;
        second.tracks = halves.iter().map(|h| h.1.clone()).collect();
        self.pieces[i].b = cut;
        self.pieces[i].tracks = halves.into_iter().map(|h| h.0).collect();
        self.pieces.insert(i + 1, second);
        Ok(i + 1)
    }

    /// Joins piece `i` and the one after it back into one, when they are neighbours in the same
    /// clip (what a split made). Returns the joined piece's index.
    pub fn join(&mut self, i: usize) -> Result<usize, String> {
        let (Some(first), Some(second)) = (self.pieces.get(i), self.pieces.get(i + 1)) else {
            return Err("There is no piece after this one to join.".to_owned());
        };
        let gap = (second.a.as_secs_f64() - first.b.as_secs_f64()).abs();
        if first.source != second.source || gap > 0.001 {
            return Err("These two pieces aren't side by side in the same clip, so they can't \
                        be joined. Undo the changes between them, or add the clip again."
                .to_owned());
        }
        let (len, next_len) = (first.span(), second.span());
        let tracks = first
            .tracks
            .iter()
            .zip(&second.tracks)
            .map(|(x, y)| x.joined(len, y, next_len))
            .collect();
        let joined = Piece { source: first.source, a: first.a, b: second.b, tracks };
        self.pieces[i] = joined;
        self.pieces.remove(i + 1);
        Ok(i)
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
    fn waveform_columns_follow_the_tracks_own_span() {
        // Ten peaks over a track that starts at 1 s and runs 10 s; the clip is longer.
        let peaks: Vec<f32> = (1..=10).map(|i| i as f32).collect();
        let span = (secs(1), secs(10));
        // Before the track starts there is silence; then peak 1, 2, ... in order.
        assert_eq!(column_peaks(&peaks, span, secs(0), secs(3), 3), [0.0, 1.0, 2.0]);
        // Past the end is silence again.
        assert_eq!(column_peaks(&peaks, span, secs(10), secs(13), 3), [10.0, 0.0, 0.0]);
        assert_eq!(column_peaks(&[], span, secs(0), secs(3), 2), [0.0, 0.0]);
    }

    #[test]
    fn a_volume_line_is_cut_and_rejoined_without_changing_the_sound() {
        let mut t = one(20);
        let span = secs(20);
        // Dip to -20 dB in the middle of the 20 s piece.
        t.pieces[0].tracks[0].add_point(secs(10), -20.0, span);
        let before: Vec<f64> = (0..=20).map(|s| t.pieces[0].tracks[0].db_at(secs(s))).collect();
        t.split(secs(10), &keyframes()).unwrap();
        // Each half carries a point on the cut at the same level.
        assert_eq!(t.pieces[0].tracks[0].db_at(secs(10)), -20.0);
        assert_eq!(t.pieces[1].tracks[0].db_at(Hns::ZERO), -20.0);
        t.join(0).unwrap();
        assert_eq!(t.pieces.len(), 1);
        assert_eq!((t.pieces[0].a, t.pieces[0].b), (Hns::ZERO, secs(20)));
        let after: Vec<f64> = (0..=20).map(|s| t.pieces[0].tracks[0].db_at(secs(s))).collect();
        for (x, y) in before.iter().zip(&after) {
            assert!((x - y).abs() < 1.5, "{before:?} vs {after:?}");
        }
    }

    #[test]
    fn pieces_from_different_places_do_not_join() {
        let mut t = one(20);
        t.split(secs(6), &keyframes()).unwrap();
        t.split(secs(12), &keyframes()).unwrap();
        t.delete(1);
        assert!(t.join(0).is_err());
        assert!(t.join(1).is_err(), "nothing after the last piece");
    }

    #[test]
    fn dragging_a_flat_level_to_the_bottom_mutes_and_back_up_unmutes() {
        let mut e = TrackEdit::default();
        e.set_level(-59.0);
        assert!(e.mute);
        assert_eq!(e.audio_edit(), AudioEdit::Mute);
        e.set_level(-6.04);
        assert!(!e.mute);
        assert_eq!(e.audio_edit(), AudioEdit::Gain { db: -6.0 });
    }

    #[test]
    fn a_line_with_every_point_at_one_level_goes_flat() {
        let mut e = TrackEdit::default();
        let i = e.add_point(secs(5), -10.0, secs(10));
        assert!(e.has_line());
        e.remove_point(i);
        assert!(!e.has_line());
        assert_eq!(e.audio_edit(), AudioEdit::Keep);
    }

    #[test]
    fn splitting_cuts_on_the_keyframe_nearest_the_playhead() {
        let mut t = one(20);
        let second = t.split(Hns::from_millis(5_300), &keyframes()).unwrap();
        assert_eq!(second, 1);
        assert_eq!((t.pieces[0].a, t.pieces[0].b), (Hns::ZERO, secs(6)));
        assert_eq!((t.pieces[1].a, t.pieces[1].b), (secs(6), secs(20)));
        // Nothing is lost or repeated.
        assert_eq!(t.total(), secs(20));
    }

    #[test]
    fn a_split_with_no_keyframe_between_is_refused() {
        let mut t = one(20);
        assert!(t.split(Hns::from_millis(300), &[Hns::ZERO]).is_err());
        assert_eq!(t.pieces.len(), 1);
        // Splitting again right after a split still works: the next keyframe is used.
        t.split(secs(10), &keyframes()).unwrap();
        t.split(Hns::from_millis(10_500), &keyframes()).unwrap();
        assert_eq!(t.pieces.len(), 3);
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
    fn silencing_a_range_dips_the_line_and_leaves_the_rest() {
        let mut e = TrackEdit::default();
        e.silence(secs(4), secs(6), secs(10));
        assert_eq!(e.db_at(secs(1)), 0.0);
        assert_eq!(e.db_at(secs(5)), MIN_DB);
        assert_eq!(e.db_at(secs(9)), 0.0);
        assert!(e.points.windows(2).all(|w| w[0].at < w[1].at));
        // The whole piece silenced is just a mute.
        let mut all = TrackEdit::default();
        all.silence(Hns::ZERO, secs(10), secs(10));
        assert!(all.mute);
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
