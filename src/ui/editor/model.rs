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

/// Active pointer tool on the editor timeline (`V` = Select, `S` = Split).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimelineTool {
    /// Default pointer mode: click selects, drags scrub/trim/envelope only.
    #[default]
    Select,
    /// Razor mode: clicking on the timeline splits at the clicked time.
    Split,
}

/// How long "Add gap" makes a black, silent piece.
pub const DEFAULT_GAP: Hns = Hns::from_secs(2);

/// Maximum source-time gap between two adjacent pieces of the same clip that can be joined (`0.25 s`).
pub const JOIN_GAP_THRESHOLD: Hns = Hns::from_millis(250);

/// Maximum pixel gap at the current timeline zoom under which adjacent same-clip pieces show a
/// join button on their seam.
pub const JOIN_SEAM_MAX_PX: f32 = 6.0;

/// Maximum source-time gap permitted for joining at the current timeline zoom: at least
/// [`JOIN_GAP_THRESHOLD`] (`0.25 s`), or whatever duration spans [`JOIN_SEAM_MAX_PX`] px
/// (capped at `2.0 s` so extreme zoom-out on long recordings never bridges large deleted cuts).
#[must_use]
pub fn join_gap_limit(disp_total: Hns, timeline_width: f32) -> Hns {
    let px_gap = if disp_total > Hns::ZERO && timeline_width > 0.0 {
        let secs = disp_total.as_secs_f64() * f64::from(JOIN_SEAM_MAX_PX / timeline_width);
        Hns::from_secs_f64(secs)
    } else {
        Hns::ZERO
    };
    JOIN_GAP_THRESHOLD.max(px_gap).min(Hns::from_secs(2))
}

/// Whether two adjacent timeline pieces (`first` followed by `second`) touch or sit within the
/// join gap threshold (under `0.25 s` or `6 px` at the current zoom) so a join button should
/// appear on their seam.
#[must_use]
pub fn seam_joinable(first: &Piece, second: &Piece, disp_total: Hns, timeline_width: f32) -> bool {
    // A black gap is not part of any clip: never offer to join across one.
    if first.gap || second.gap || first.source != second.source {
        return false;
    }
    // Reject overlapping or reordered pieces (tolerance of 1 ms for keyframe rounding).
    if second.a + Hns::from_millis(1) < first.b {
        return false;
    }
    let gap = (second.a - first.b).max(Hns::ZERO);
    gap <= join_gap_limit(disp_total, timeline_width)
}

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
                let run = (w[1].at - w[0].at).as_secs_f64();
                if run <= 0.0 {
                    return w[1].db;
                }
                let f = (t - w[0].at).as_secs_f64() / run;
                return w[0].db + (w[1].db - w[0].db) * f;
            }
        }
        p[p.len() - 1].db
    }

    /// Puts a point at `at` (a piece `span` long) at level `db`; the first one also pins both ends
    /// at the old flat level. Returns the new point's index.
    pub fn add_point(&mut self, at: Hns, db: f64, span: Hns) -> usize {
        // A piece too short to fit a point clear of both ends has no room for one (and the clamp
        // below would panic with its bounds crossed).
        if span < MIN_GAP + MIN_GAP {
            return 0;
        }
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
            let (lo, hi) = (self.points[i - 1].at + MIN_GAP, self.points[i + 1].at - MIN_GAP);
            // Neighbours closer than two gaps leave no room: stay where it is.
            if lo <= hi { at.clamp(lo, hi) } else { self.points[i].at }
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

    /// Re-cuts the piece this track belongs to. `shift` is how much earlier the piece now starts
    /// to count from (positive when its start was trimmed, negative when it was pulled out) and
    /// `new_span` its new length. The sound inside stays what it was; the line is cut or extended
    /// flat at the new ends.
    pub fn resized(&self, shift: Hns, new_span: Hns) -> Self {
        if !self.has_line() {
            return self.clone();
        }
        let mut out = self.clone();
        let inside = self
            .points
            .iter()
            .filter(|p| p.at > shift + MIN_GAP && p.at < shift + new_span - MIN_GAP)
            .map(|p| EnvPoint { at: p.at - shift, db: p.db });
        out.points = std::iter::once(EnvPoint { at: Hns::ZERO, db: self.db_at(shift) })
            .chain(inside)
            .chain(std::iter::once(EnvPoint { at: new_span, db: self.db_at(shift + new_span) }))
            .collect();
        out.simplify();
        out
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
        // Keep the times strictly rising: a point of `next` within a gap of its end would land on
        // or past the end point, and two points at one time divide by zero in `db_at`.
        let end = points.pop();
        let mut rising: Vec<EnvPoint> = Vec::with_capacity(points.len() + 1);
        for p in points {
            if end.is_none_or(|e| p.at < e.at) && rising.last().is_none_or(|l| p.at > l.at) {
                rising.push(p);
            }
        }
        rising.extend(end);
        let points = rising;
        let mut out = Self { gain_db: 0.0, mute: false, points };
        out.simplify();
        out
    }
}

impl TrackEdit {
    /// The level the piece's dB control shows: the flat level, or the line's loudest point.
    pub fn level(&self) -> f64 {
        if self.has_line() {
            self.points.iter().map(|p| p.db).fold(f64::MIN, f64::max)
        } else {
            self.gain_db
        }
    }

    /// Sets [`Self::level`] to `db`. A line moves up or down as a whole, keeping its shape (a flat
    /// `gain_db` is ignored while a line exists, so changing only that would do nothing).
    pub fn set_piece_level(&mut self, db: f64) {
        if self.has_line() {
            let by = db - self.level();
            for p in &mut self.points {
                p.db += by;
            }
        } else {
            self.gain_db = db;
        }
    }
}

/// A whole audio track's settings in the editor's mixer, on top of every piece's own edits.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrackMix {
    pub mute: bool,
    pub gain_db: f64,
}

/// A piece's track edits as they sound with the mixer applied: a muted track mutes every piece,
/// and the track's gain raises or lowers each piece's level or line. Preview and export both use
/// this, so they always agree.
pub fn mixed_tracks(tracks: &[TrackEdit], mix: &[TrackMix]) -> Vec<TrackEdit> {
    tracks
        .iter()
        .enumerate()
        .map(|(t, edit)| {
            let m = mix.get(t).copied().unwrap_or_default();
            let mut out = edit.clone();
            out.mute |= m.mute;
            if out.points.len() >= 2 {
                for p in &mut out.points {
                    p.db += m.gain_db;
                }
            } else {
                out.gain_db += m.gain_db;
            }
            out
        })
        .collect()
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
    /// A black, silent stretch, not a part of a clip. `source` then only names a clip whose
    /// picture size, frame rate and audio layout the exported gap copies; `a..b` is just its
    /// length (it has no source to run past, so it can be any length).
    pub gap: bool,
}

impl Piece {
    pub fn span(&self) -> Hns {
        self.b - self.a
    }

    /// A black, silent gap `len` long that matches clip `source` and its `tracks` audio tracks.
    pub fn new_gap(source: usize, len: Hns, tracks: usize) -> Self {
        Self {
            source,
            a: Hns::ZERO,
            b: len.max(MIN_PIECE),
            tracks: vec![TrackEdit::default(); tracks],
            gap: true,
        }
    }

    /// The stretch `from..to` into this piece (offsets from its start) as a piece of its own. The
    /// audio edits keep their place in the sound, as in [`Self::trimmed`].
    pub fn part(&self, from: Hns, to: Hns) -> Self {
        if self.gap {
            return Self { a: Hns::ZERO, b: to - from, ..self.clone() };
        }
        let tracks = self.tracks.iter().map(|t| t.resized(from, to - from)).collect();
        Self { source: self.source, a: self.a + from, b: self.a + to, tracks, gap: false }
    }

    /// This piece with its range changed to `a..b` of a source `duration` long (kept inside it and
    /// at least `MIN_PIECE` long). The audio edits keep their place in the sound.
    pub fn trimmed(&self, a: Hns, b: Hns, duration: Hns) -> Self {
        if self.gap {
            // A gap has no clip to stay inside: only its length is limited.
            let a = a.min(b - MIN_PIECE);
            return Self { a, b, ..self.clone() };
        }
        if duration < MIN_PIECE {
            // A source shorter than a piece can only be taken whole, never run past its end.
            return Self { a: Hns::ZERO, b: duration, ..self.clone() };
        }
        let a = a.clamp(Hns::ZERO, (b - MIN_PIECE).max(Hns::ZERO));
        let b = b.clamp(a + MIN_PIECE, duration.max(a + MIN_PIECE));
        let (shift, span) = (a - self.a, b - a);
        let tracks = self.tracks.iter().map(|t| t.resized(shift, span)).collect();
        Self { source: self.source, a, b, tracks, gap: false }
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
        if piece.gap {
            return Err("A gap can't be split. Resize it by its edges instead.".to_owned());
        }
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
    /// clip within [`JOIN_GAP_THRESHOLD`]. Returns the joined piece's index.
    #[cfg(test)]
    pub fn join(&mut self, i: usize) -> Result<usize, String> {
        self.join_within(i, JOIN_GAP_THRESHOLD)
    }

    /// Joins piece `i` and the one after it back into one, allowing a source-time gap up to
    /// `max_gap` between `pieces[i].b` and `pieces[i + 1].a` (matching [`seam_joinable`] at the
    /// current zoom). Returns the joined piece's index.
    pub fn join_within(&mut self, i: usize, max_gap: Hns) -> Result<usize, String> {
        let (Some(first), Some(second)) = (self.pieces.get(i), self.pieces.get(i + 1)) else {
            return Err("There is no piece after this one to join.".to_owned());
        };
        if first.gap || second.gap {
            return Err("A gap can't be joined to a clip.".to_owned());
        }
        let overlap_tol = Hns::from_millis(1);
        let gap = (second.a - first.b).max(Hns::ZERO);
        if first.source != second.source || second.a + overlap_tol < first.b || gap > max_gap {
            return Err("These two pieces aren't side by side in the same clip, so they can't \
                        be joined. Undo the changes between them, or add the clip again."
                .to_owned());
        }
        let (len, next_len) = (first.span() + gap, second.span());
        let tracks = first
            .tracks
            .iter()
            .zip(&second.tracks)
            .map(|(x, y)| x.joined(len, y, next_len))
            .collect();
        let joined = Piece { source: first.source, a: first.a, b: second.b, tracks, gap: false };
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

    /// Moves piece `from` so it sits at index `to` (the other pieces keep their order and close
    /// up around it). Returns the piece's new index, or `None` when `from` is out of range. `to`
    /// past the end means last.
    pub fn move_piece(&mut self, from: usize, to: usize) -> Option<usize> {
        if from >= self.pieces.len() {
            return None;
        }
        let to = to.min(self.pieces.len() - 1);
        let piece = self.pieces.remove(from);
        self.pieces.insert(to, piece);
        Some(to)
    }

    /// Puts a black, silent gap `len` long at piece boundary `at` (`0` is the very start, `len()`
    /// the end), matching clip `source` and `tracks` audio tracks. Returns the gap's index.
    pub fn insert_gap(&mut self, at: usize, len: Hns, source: usize, tracks: usize) -> usize {
        let at = at.min(self.pieces.len());
        self.pieces.insert(at, Piece::new_gap(source, len, tracks));
        at
    }

    /// Whether a split at timeline time `t` would work: on a clip (not a gap), at least
    /// [`MIN_PIECE`] from both of its edges, with a keyframe to cut on. Drives the razor preview
    /// and the click, so the line is dimmed exactly when a click would do nothing.
    pub fn can_split_at(&self, t: Hns, keyframes: &[Hns]) -> bool {
        let Some((i, at)) = self.locate(t) else { return false };
        let p = &self.pieces[i];
        !p.gap
            && at >= p.a + MIN_PIECE
            && at <= p.b - MIN_PIECE
            && keyframes.iter().any(|&k| k >= p.a + MIN_PIECE && k <= p.b - MIN_PIECE)
    }

    /// Timeline time `t`, pulled onto the nearest piece boundary (the start, every seam and the
    /// end) when one is within `within`.
    pub fn snap_time(&self, t: Hns, within: Hns) -> Hns {
        (0..=self.pieces.len())
            .map(|i| self.start_of(i))
            .filter(|&b| (b - t).ticks().abs() <= within.ticks())
            .min_by_key(|&b| (b - t).ticks().abs())
            .unwrap_or(t)
    }

    /// Cuts `a..b` out and closes the gap (ripple delete): pieces are cut at both ends and
    /// everything inside goes. A leftover shorter than [`MIN_PIECE`] is removed with the range.
    /// Returns the range actually removed, or `None` when it is shorter than [`MIN_PIECE`].
    pub fn cut_range(&mut self, a: Hns, b: Hns) -> Option<(Hns, Hns)> {
        self.replace_range(a, b, false)
    }

    /// Like [`Self::cut_range`], but leaves a black, silent gap of the same length where the
    /// range was, so everything after it keeps its place.
    pub fn gap_range(&mut self, a: Hns, b: Hns) -> Option<(Hns, Hns)> {
        self.replace_range(a, b, true)
    }

    fn replace_range(&mut self, a: Hns, b: Hns, make_gap: bool) -> Option<(Hns, Hns)> {
        let (mut a, mut b) = clamp_range(a, b, self.total())?;
        // Absorb slivers: an end that would leave a shorter-than-minimum scrap of a piece moves
        // out to that piece's edge instead.
        let mut s = Hns::ZERO;
        for p in &self.pieces {
            let e = s + p.span();
            if s < a && a < e && a - s < MIN_PIECE {
                a = s;
            }
            if s < b && b < e && e - b < MIN_PIECE {
                b = e;
            }
            s = e;
        }
        let (source, tracks) = {
            let (i, _) = self.locate(a)?;
            (self.pieces[i].source, self.pieces[0].tracks.len())
        };
        let mut out = Vec::with_capacity(self.pieces.len() + 2);
        let mut s = Hns::ZERO;
        let mut gap_done = !make_gap;
        for p in std::mem::take(&mut self.pieces) {
            let e = s + p.span();
            if e <= a || s >= b {
                out.push(p);
            } else {
                if a > s {
                    out.push(p.part(Hns::ZERO, a - s));
                }
                if !gap_done {
                    out.push(Piece::new_gap(source, b - a, tracks));
                    gap_done = true;
                }
                if b < e {
                    out.push(p.part(b - s, e - s));
                }
            }
            s = e;
        }
        self.pieces = out;
        Some((a, b))
    }

    /// The name of piece `i`: its letter (gaps don't use one up), or "Gap".
    pub fn label(&self, i: usize) -> String {
        match self.pieces.get(i) {
            Some(p) if p.gap => "Gap".to_owned(),
            _ => piece_label(self.pieces.iter().take(i).filter(|p| !p.gap).count()),
        }
    }

    /// "Piece A", or "Gap".
    pub fn title(&self, i: usize) -> String {
        match self.pieces.get(i) {
            Some(p) if p.gap => "Gap".to_owned(),
            _ => format!("Piece {}", self.label(i)),
        }
    }

    /// The piece boundary (`0..=len`) closest to timeline time `t`.
    pub fn boundary_near(&self, t: Hns) -> usize {
        let mut best = (0, (t - Hns::ZERO).ticks().abs());
        for i in 1..=self.pieces.len() {
            let d = (t - self.start_of(i)).ticks().abs();
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    }

    /// Moves piece `i` one place left (`-1`) or right (`1`). Returns its new index.
    pub fn shift(&mut self, i: usize, by: isize) -> Option<usize> {
        let j =
            i.checked_add_signed(by).filter(|&j| j < self.pieces.len() && i < self.pieces.len())?;
        self.pieces.swap(i, j);
        Some(j)
    }
}

/// A picked range `a..b` put in order and kept inside a timeline `total` long. `None` when it is
/// shorter than [`MIN_PIECE`], so a click never counts as a range.
pub fn clamp_range(a: Hns, b: Hns, total: Hns) -> Option<(Hns, Hns)> {
    let (a, b) = (a.min(b).max(Hns::ZERO), a.max(b).min(total));
    (b - a >= MIN_PIECE).then_some((a, b))
}

/// Where a dragged piece would land: among pieces whose displayed middles are `mids` (in
/// order, `from` being the one dragged), the index it takes when dropped with its pointer at
/// `x`. That is the number of other pieces whose middle is left of the pointer.
pub fn move_target(mids: &[f32], from: usize, x: f32) -> usize {
    mids.iter().enumerate().filter(|&(j, &m)| j != from && m < x).count()
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
    /// The piece's range in its clip, always (what a frame-exact export cuts).
    pub range: (Hns, Hns),
    /// The start is not on a keyframe, so the video must be re-encoded to begin exactly there.
    pub reencode: bool,
    /// When `true` (and `tracks.len() > 1`), export rebuilds track 0 (Master) as the mix of the
    /// individually edited tracks `1..` via `edit_audio_rebuilding_master`.
    pub rebuild_master: bool,
    /// Audio tracks to export: track 0 only in simple mode, or every track when rebuilding Master.
    pub tracks: Vec<AudioTrack>,
    /// A black, silent gap made by ffmpeg (as long as `range`), not cut from `source`; the source
    /// only says which clip's picture size, frame rate and codec it matches.
    pub gap: bool,
}

impl PiecePlan {
    /// Whether any audio track is re-encoded or Master must be rebuilt from separate tracks.
    pub fn has_audio_edits(&self) -> bool {
        (self.rebuild_master && self.tracks.len() > 1)
            || self.tracks.iter().any(|t| t.edit != AudioEdit::Keep)
    }
}

/// The plan for `piece`, whose source is `duration` long and has `keyframes`.
///
/// When `rebuild_master` is `true` and the piece has separate source tracks (`tracks.len() > 1`),
/// every track is included so export rebuilds track 0 (Master) from the edited `1..` tracks.
/// Otherwise only track 0 (Master) is exported.
///
/// # Errors
/// When the piece is empty.
pub fn plan_piece(
    piece: &Piece,
    duration: Hns,
    keyframes: &[Hns],
    bitrate_bps: u32,
    rebuild_master: bool,
) -> Result<PiecePlan, String> {
    // The start stays where the user put it. Off a keyframe it costs a re-encode, not accuracy.
    let start = piece.a;
    if piece.b <= start {
        return Err("A piece is empty.".to_owned());
    }
    if piece.gap {
        // Same track count as the clip pieces it is joined to; nothing in it is edited.
        let count = if rebuild_master { piece.tracks.len() } else { piece.tracks.len().min(1) };
        return Ok(PiecePlan {
            source: piece.source,
            trim: None,
            range: (Hns::ZERO, piece.span()),
            reencode: false,
            rebuild_master: false,
            tracks: vec![AudioTrack { edit: AudioEdit::Keep, bitrate_bps }; count],
            gap: true,
        });
    }
    let off_keyframe = (snapped_start(keyframes, start).ticks() - start.ticks()).abs() > 10_000;
    let whole = start == Hns::ZERO && piece.b >= duration;
    let rebuild = rebuild_master && piece.tracks.len() > 1;
    let tracks = if rebuild {
        piece.tracks.iter().map(|t| AudioTrack { edit: t.audio_edit(), bitrate_bps }).collect()
    } else {
        // Simple mode: only the Master track (the first) goes into an export.
        piece
            .tracks
            .iter()
            .take(1)
            .map(|t| AudioTrack { edit: t.audio_edit(), bitrate_bps })
            .collect()
    };
    Ok(PiecePlan {
        source: piece.source,
        trim: (!whole).then_some((start, piece.b)),
        range: (start, piece.b),
        reencode: off_keyframe,
        rebuild_master: rebuild,
        tracks,
        gap: false,
    })
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
                gap: false,
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

    fn three() -> Timeline {
        let mut t = one(20);
        t.split(secs(6), &keyframes()).unwrap();
        t.split(secs(12), &keyframes()).unwrap();
        t
    }

    #[test]
    fn moving_a_piece_reorders_and_keeps_the_rest_in_order() {
        let mut t = three(); // A 0-6, B 6-12, C 12-20
        let starts = |t: &Timeline| t.pieces.iter().map(|p| p.a).collect::<Vec<_>>();
        assert_eq!(t.move_piece(0, 2), Some(2));
        assert_eq!(starts(&t), [secs(6), secs(12), Hns::ZERO]);
        assert_eq!(t.move_piece(2, 0), Some(0));
        assert_eq!(starts(&t), [Hns::ZERO, secs(6), secs(12)]);
        // Past the end means last; same place changes nothing; a missing piece is refused.
        assert_eq!(t.move_piece(0, 99), Some(2));
        assert_eq!(t.move_piece(1, 1), Some(1));
        assert_eq!(t.move_piece(7, 0), None);
        assert_eq!(t.total(), secs(20));
    }

    #[test]
    fn a_drag_lands_after_the_pieces_whose_middle_it_passed() {
        let mids = [10.0, 30.0, 50.0];
        assert_eq!(move_target(&mids, 0, 5.0), 0);
        assert_eq!(move_target(&mids, 0, 40.0), 1);
        assert_eq!(move_target(&mids, 0, 90.0), 2);
        assert_eq!(move_target(&mids, 2, 5.0), 0);
        assert_eq!(move_target(&mids, 1, 20.0), 1);
    }

    #[test]
    fn a_gap_goes_at_a_boundary_and_counts_in_the_totals() {
        let mut t = three();
        let at = t.insert_gap(1, DEFAULT_GAP, 0, 2);
        assert_eq!(at, 1);
        assert!(t.pieces[1].gap);
        assert_eq!(t.pieces[1].span(), secs(2));
        assert_eq!(t.total(), secs(22));
        assert_eq!(t.start_of(2), secs(8));
        // Time inside the gap lands in the gap; just after it, in the next piece.
        assert_eq!(t.locate(secs(7)).map(|l| l.0), Some(1));
        assert_eq!(t.locate(secs(9)).map(|l| l.0), Some(2));
        // Past the end clamps to the end.
        assert_eq!(t.insert_gap(99, secs(1), 0, 2), 4);
    }

    #[test]
    fn gaps_do_not_use_up_a_letter() {
        let mut t = three();
        t.insert_gap(1, DEFAULT_GAP, 0, 2);
        let names: Vec<String> = (0..t.pieces.len()).map(|i| t.label(i)).collect();
        assert_eq!(names, ["A", "Gap", "B", "C"]);
    }

    #[test]
    fn the_nearest_boundary_is_found_from_the_playhead() {
        let t = three(); // boundaries at 0, 6, 12, 20 s
        assert_eq!(t.boundary_near(secs(1)), 0);
        assert_eq!(t.boundary_near(secs(5)), 1);
        assert_eq!(t.boundary_near(secs(13)), 2);
        assert_eq!(t.boundary_near(secs(19)), 3);
        assert_eq!(Timeline::default().boundary_near(secs(5)), 0);
    }

    #[test]
    fn a_gap_resizes_freely_from_either_edge_down_to_the_minimum() {
        let gap = Piece::new_gap(0, secs(2), 2);
        // Right edge out past anything a clip could offer.
        assert_eq!(gap.trimmed(gap.a, secs(500), secs(20)).span(), secs(500));
        // Left edge in shortens it; a drag far past the right edge stops at MIN_PIECE.
        assert_eq!(
            gap.trimmed(Hns::from_millis(500), gap.b, secs(20)).span(),
            Hns::from_millis(1_500)
        );
        assert_eq!(gap.trimmed(secs(9), gap.b, secs(20)).span(), MIN_PIECE);
        // Left edge out lengthens it, even before zero.
        assert_eq!(gap.trimmed(secs(-3), gap.b, secs(20)).span(), secs(5));
        assert!(gap.trimmed(gap.a, gap.b, secs(20)).gap);
    }

    #[test]
    fn a_gap_is_never_joined_split_or_exported_as_a_cut() {
        let mut t = three();
        t.insert_gap(1, DEFAULT_GAP, 0, 2);
        let (before, gap, after) = (t.pieces[0].clone(), t.pieces[1].clone(), t.pieces[2].clone());
        // Even when the pieces beside it would join, nothing joins across or into the gap.
        assert!(!seam_joinable(&before, &gap, secs(22), 1000.0));
        assert!(!seam_joinable(&gap, &after, secs(22), 1000.0));
        assert!(t.join(0).is_err());
        assert!(t.join(1).is_err());
        assert!(t.split(secs(7), &keyframes()).is_err());
        assert_eq!(t.pieces.len(), 4);
    }

    #[test]
    fn a_gap_plans_as_black_silence_with_the_neighbours_track_count() {
        let t = {
            let mut t = one(20);
            t.insert_gap(1, secs(3), 0, 2);
            t
        };
        let simple = plan_piece(&t.pieces[1], secs(20), &keyframes(), 96_000, false).unwrap();
        assert!(simple.gap && !simple.reencode && !simple.rebuild_master);
        assert_eq!(simple.trim, None);
        assert_eq!(simple.range, (Hns::ZERO, secs(3)));
        assert_eq!(simple.tracks.len(), 1);
        assert!(!simple.has_audio_edits());
        let rebuilt = plan_piece(&t.pieces[1], secs(20), &keyframes(), 96_000, true).unwrap();
        assert_eq!(rebuilt.tracks.len(), 2);
        assert_eq!(rebuilt.tracks[0].bitrate_bps, 96_000);
        // The clip beside it is planned as before.
        assert!(!plan_piece(&t.pieces[0], secs(20), &keyframes(), 96_000, false).unwrap().gap);
    }

    #[test]
    fn an_edge_dragged_gap_with_a_shifted_start_still_plans_its_length() {
        let gap = Piece::new_gap(0, secs(2), 1).trimmed(secs(-1), secs(2), secs(20));
        let plan = plan_piece(&gap, secs(20), &keyframes(), 96_000, false).unwrap();
        assert_eq!(plan.range.1, secs(3));
    }

    fn spans(t: &Timeline) -> Vec<(Hns, Hns, bool)> {
        t.pieces.iter().map(|p| (p.a, p.b, p.gap)).collect()
    }

    #[test]
    fn cutting_a_range_closes_up_across_pieces() {
        let mut t = three(); // A 0-6, B 6-12, C 12-20
        assert_eq!(t.cut_range(secs(4), secs(14)), Some((secs(4), secs(14))));
        assert_eq!(spans(&t), [(Hns::ZERO, secs(4), false), (secs(14), secs(20), false)]);
        assert_eq!(t.total(), secs(10));
        // Backwards and over-long ranges are put in order and kept inside.
        let mut t = three();
        assert_eq!(t.cut_range(secs(30), secs(18)), Some((secs(18), secs(20))));
        assert_eq!(t.total(), secs(18));
    }

    #[test]
    fn a_cut_leaves_no_sliver_shorter_than_the_minimum() {
        let mut t = three();
        // 50 ms into A and 50 ms short of the end of C: both scraps go with the range.
        let a = Hns::from_millis(50);
        let b = secs(20) - Hns::from_millis(50);
        assert_eq!(t.cut_range(a, b), Some((Hns::ZERO, secs(20))));
        assert!(t.pieces.is_empty());
        // Too short to be a range at all.
        assert_eq!(three().cut_range(secs(3), secs(3) + Hns::from_millis(50)), None);
        let mut t = three();
        t.cut_range(secs(3), secs(9));
        assert!(t.pieces.iter().all(|p| p.span() >= MIN_PIECE));
    }

    #[test]
    fn a_range_cut_keeps_each_tracks_volume_line() {
        let mut t = one(20);
        t.pieces[0].tracks[0].add_point(secs(15), -20.0, secs(20));
        let before = t.pieces[0].tracks[0].db_at(secs(17));
        t.cut_range(secs(2), secs(8)); // the dip now sits at 9 s of the 14 s that remain
        assert_eq!(t.pieces.len(), 2);
        assert_eq!(t.pieces[1].a, secs(8));
        assert!((t.pieces[1].tracks[0].db_at(secs(9)) - before).abs() < 1.5);
    }

    #[test]
    fn a_range_becomes_a_gap_of_the_same_length() {
        let mut t = three();
        assert_eq!(t.gap_range(secs(4), secs(14)), Some((secs(4), secs(14))));
        assert_eq!(
            spans(&t),
            [(Hns::ZERO, secs(4), false), (Hns::ZERO, secs(10), true), (secs(14), secs(20), false)]
        );
        assert_eq!(t.total(), secs(20));
        assert_eq!(t.pieces[1].tracks.len(), 2);
        // Inside one piece it splits that piece around the gap.
        let mut t = one(20);
        t.gap_range(secs(5), secs(7));
        assert_eq!(t.pieces.len(), 3);
        assert!(t.pieces[1].gap);
        assert_eq!(t.total(), secs(20));
    }

    #[test]
    fn times_snap_to_piece_boundaries_within_reach() {
        let t = three(); // boundaries 0, 6, 12, 20
        let tol = Hns::from_millis(200);
        assert_eq!(t.snap_time(Hns::from_millis(6_150), tol), secs(6));
        assert_eq!(t.snap_time(Hns::from_millis(11_700), tol), Hns::from_millis(11_700));
        assert_eq!(t.snap_time(Hns::from_millis(100), tol), Hns::ZERO);
        assert_eq!(clamp_range(secs(9), secs(3), secs(20)), Some((secs(3), secs(9))));
        assert_eq!(clamp_range(secs(3), secs(3), secs(20)), None);
    }

    #[test]
    fn splitting_needs_a_clip_room_each_side_and_a_keyframe() {
        let mut t = three(); // A 0-6, B 6-12, C 12-20
        let k = keyframes(); // every 2 s
        assert!(t.can_split_at(secs(4), &k));
        // Too close to an edge.
        assert!(!t.can_split_at(secs(6) + Hns::from_millis(50), &k));
        assert!(!t.can_split_at(secs(12) - Hns::from_millis(50), &k));
        // No keyframe to cut on inside the piece.
        assert!(!t.can_split_at(secs(4), &[Hns::ZERO]));
        // A gap and the space past the end.
        t.insert_gap(1, secs(2), 0, 2);
        assert!(!t.can_split_at(secs(7), &k));
        assert!(!t.can_split_at(secs(99), &k));
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
        let plan = plan_piece(&one(20).pieces[0], secs(20), &keyframes(), 160_000, false).unwrap();
        assert_eq!(plan.trim, None);
        assert!(!plan.rebuild_master);
        assert!(!plan.has_audio_edits());
    }

    #[test]
    fn a_partial_piece_trims_and_a_muted_track_is_re_encoded() {
        let mut t = one(20);
        t.split(secs(6), &keyframes()).unwrap();
        t.pieces[1].tracks[1].mute = true;
        t.pieces[1].tracks[0].gain_db = 3.0;
        let plan = plan_piece(&t.pieces[1], secs(20), &keyframes(), 96_000, false).unwrap();
        assert_eq!(plan.trim, Some((secs(6), secs(20))));
        assert_eq!(plan.tracks[0].edit, AudioEdit::Gain { db: 3.0 });
        // Simple mode: only Master is exported, so the muted second track is left out.
        assert_eq!(plan.tracks.len(), 1);
        assert!(!plan.rebuild_master);
        assert!(plan.has_audio_edits());
    }

    #[test]
    fn rebuild_master_plan_keeps_all_tracks_and_triggers_audio_pass() {
        let mut t = one(20);
        t.pieces[0].tracks[1].mute = true;
        let plan = plan_piece(&t.pieces[0], secs(20), &keyframes(), 160_000, true).unwrap();
        assert!(plan.rebuild_master);
        assert_eq!(plan.tracks.len(), 2);
        assert_eq!(plan.tracks[1].edit, AudioEdit::Mute);
        assert!(plan.has_audio_edits());

        // Single-track clip has nothing to rebuild from.
        let single = Piece {
            source: 0,
            a: Hns::ZERO,
            b: secs(20),
            tracks: vec![TrackEdit::default()],
            gap: false,
        };
        let plan_single = plan_piece(&single, secs(20), &keyframes(), 160_000, true).unwrap();
        assert!(!plan_single.rebuild_master);
        assert_eq!(plan_single.tracks.len(), 1);
    }

    #[test]
    fn a_start_between_keyframes_is_kept_and_asks_for_a_re_encode() {
        let t = one(20);
        let piece = t.pieces[0].trimmed(Hns::from_millis(7_300), secs(20), secs(20));
        let plan = plan_piece(&piece, secs(20), &keyframes(), 160_000, false).unwrap();
        assert_eq!(plan.trim, Some((Hns::from_millis(7_300), secs(20))));
        assert!(plan.reencode);
        let on_key = t.pieces[0].trimmed(secs(6), secs(20), secs(20));
        assert!(!plan_piece(&on_key, secs(20), &keyframes(), 160_000, false).unwrap().reencode);
    }

    #[test]
    fn seam_joinable_respects_time_and_pixel_thresholds() {
        let mk = |source, a_ms, b_ms| Piece {
            source,
            a: Hns::from_millis(a_ms),
            b: Hns::from_millis(b_ms),
            tracks: vec![TrackEdit::default()],
            gap: false,
        };
        // Touching pieces from the same clip -> joinable.
        assert!(seam_joinable(&mk(0, 0, 5_000), &mk(0, 5_000, 10_000), secs(10), 1000.0));
        // Small gap under 0.25 s -> joinable even when zoomed in (150 ms = 15 px at 10 s / 1000 px).
        assert!(seam_joinable(&mk(0, 0, 5_000), &mk(0, 5_150, 10_000), secs(10), 1000.0));
        // Gap of 0.4 s on 10 s / 1000 px (= 40 px > 6 px and > 0.25 s) -> NOT joinable.
        assert!(!seam_joinable(&mk(0, 0, 5_000), &mk(0, 5_400, 10_000), secs(10), 1000.0));
        // Gap of 0.4 s on 100 s / 1000 px (= 4 px <= 6 px) -> joinable at this zoom.
        assert!(seam_joinable(&mk(0, 0, 50_000), &mk(0, 50_400, 100_000), secs(100), 1000.0));
        // Gap of 0.8 s on 100 s / 1000 px (= 8 px > 6 px and > 0.25 s) -> NOT joinable.
        assert!(!seam_joinable(&mk(0, 0, 50_000), &mk(0, 50_800, 100_000), secs(100), 1000.0));
        // Different source clips -> never joinable.
        assert!(!seam_joinable(&mk(0, 0, 5_000), &mk(1, 5_000, 10_000), secs(10), 1000.0));
        // Reordered / overlapping pieces -> never joinable.
        assert!(!seam_joinable(&mk(0, 5_000, 10_000), &mk(0, 0, 5_000), secs(10), 1000.0));
    }

    #[test]
    fn mixer_applies_on_top_of_piece_edits() {
        let line = TrackEdit {
            points: vec![
                EnvPoint { at: Hns::ZERO, db: -6.0 },
                EnvPoint { at: Hns::from_secs(2), db: 0.0 },
            ],
            ..TrackEdit::default()
        };
        let flat = TrackEdit { gain_db: 3.0, ..TrackEdit::default() };
        let mix = [TrackMix::default(), TrackMix { mute: false, gain_db: -4.0 }];
        let out = mixed_tracks(&[flat.clone(), line.clone(), flat.clone()], &mix);
        assert_eq!(out[0], flat, "a track with no mixer entry is untouched");
        assert_eq!(out[1].points.iter().map(|p| p.db).collect::<Vec<_>>(), [-10.0, -4.0]);
        assert_eq!(out[2].gain_db, 3.0);
        let muted = mixed_tracks(&[flat], &[TrackMix { mute: true, gain_db: 0.0 }]);
        assert_eq!(muted[0].audio_edit(), AudioEdit::Mute);
    }

    #[test]
    fn piece_level_moves_a_line_as_a_whole() {
        let mut e = TrackEdit {
            points: vec![
                EnvPoint { at: Hns::ZERO, db: -6.0 },
                EnvPoint { at: Hns::from_secs(2), db: 0.0 },
            ],
            ..TrackEdit::default()
        };
        assert_eq!(e.level(), 0.0);
        e.set_piece_level(3.0);
        assert_eq!(e.points.iter().map(|p| p.db).collect::<Vec<_>>(), [-3.0, 3.0]);
        let mut flat = TrackEdit::default();
        flat.set_piece_level(-5.0);
        assert_eq!((flat.level(), flat.gain_db), (-5.0, -5.0));
    }

    #[test]
    fn join_within_bridges_small_gap_and_rejects_large_gap() {
        let mut t = Timeline {
            pieces: vec![
                Piece {
                    source: 0,
                    a: Hns::ZERO,
                    b: Hns::from_millis(5_000),
                    tracks: vec![TrackEdit::default()],
                    gap: false,
                },
                Piece {
                    source: 0,
                    a: Hns::from_millis(5_150),
                    b: Hns::from_millis(10_000),
                    tracks: vec![TrackEdit::default()],
                    gap: false,
                },
            ],
        };
        assert_eq!(t.join(0), Ok(0));
        assert_eq!(t.pieces.len(), 1);
        assert_eq!((t.pieces[0].a, t.pieces[0].b), (Hns::ZERO, Hns::from_millis(10_000)));
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
    fn trimming_keeps_the_sound_where_it_was() {
        let mut e = TrackEdit::default();
        e.silence(secs(4), secs(6), secs(10));
        let piece = Piece { source: 0, a: secs(10), b: secs(20), tracks: vec![e], gap: false };
        // Cut 3 s off the start: the silent stretch is now 1..3 s in.
        let t = piece.trimmed(secs(13), secs(20), secs(60));
        assert_eq!(t.span(), secs(7));
        assert_eq!(t.tracks[0].db_at(secs(2)), MIN_DB);
        assert_eq!(t.tracks[0].db_at(secs(5)), 0.0);
        // Pulled back out, the piece can't start before the clip does.
        assert_eq!(t.trimmed(secs(0), secs(20), secs(60)).a, secs(0));
    }

    #[test]
    fn an_empty_piece_is_refused() {
        let piece = Piece { source: 0, a: secs(6), b: secs(6), tracks: vec![], gap: false };
        assert!(plan_piece(&piece, secs(20), &keyframes(), 1, false).is_err());
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

    #[test]
    fn short_pieces_never_panic_the_volume_line() {
        let mut t = TrackEdit::default();
        assert_eq!(t.add_point(Hns::from_millis(30), -6.0, Hns::from_millis(90)), 0);
        assert!(t.points.is_empty());
        let mut t = TrackEdit::default();
        let span = Hns::from_millis(300);
        t.add_point(Hns::from_millis(100), -6.0, span);
        t.add_point(Hns::from_millis(160), -9.0, span);
        t.move_point(1, Hns::from_millis(150), 0.0, span);
        t.move_point(2, Hns::from_millis(100), 0.0, span);
    }

    #[test]
    fn joined_line_has_no_duplicate_times() {
        let line = |db| TrackEdit {
            gain_db: 0.0,
            mute: false,
            points: vec![
                EnvPoint { at: Hns::ZERO, db },
                EnvPoint { at: Hns::from_millis(970), db: -20.0 },
                EnvPoint { at: Hns::from_secs(1), db: 0.0 },
            ],
        };
        let j = line(0.0).joined(secs(1), &line(-3.0), secs(1));
        assert!(j.points.windows(2).all(|w| w[0].at < w[1].at), "{:?}", j.points);
        assert!(j.db_at(Hns::from_millis(1990)).is_finite());
    }

    #[test]
    fn trim_of_a_tiny_source_stays_inside_it() {
        let p =
            Piece { source: 0, a: Hns::ZERO, b: Hns::from_millis(80), tracks: vec![], gap: false };
        let t = p.trimmed(Hns::ZERO, Hns::from_millis(80), Hns::from_millis(80));
        assert_eq!(t.b, Hns::from_millis(80));
    }
}
