//! The Editor tab: put one or more clips on a timeline, split them into pieces (A, B, C...), delete
//! or reorder pieces, set the volume or mute of each audio track per piece, and save the result
//! as a new file or as one file per piece. Video is always stream-copied; only an edited audio
//! track is re-encoded (D38), so every cut lands on a keyframe.
//!
//! The picture and sound come from the in-app player (`player.rs`).
//!
//! Every ffmpeg call runs on a background thread. The view polls nothing: the task that started
//! the job updates the view when the job returns.

mod export;
mod model;
mod player;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    AnyElement, Bounds, Context, ExternalPaths, FocusHandle, FontWeight, ImageSource,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, ParentElement, Pixels, Render, RenderImage, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window, canvas, div,
    img, px, rgb,
};
use zc_core::Hns;
use zc_core::settings::{EditorSettings, FinishChoice};
use zc_editor::concat::StreamInfo;
use zc_editor::ffmpeg::{Ffmpeg, FfmpegError, ProbeInfo};
use zc_editor::waveform::{file_key, rms_peaks};
use zc_store::Store;

use self::model::{Piece, PiecePlan, Timeline, TrackEdit};
use self::player::{Item, Player, Sound};
use super::components::{
    button::{button, button_ghost, button_primary},
    card::card,
    labeled_toggle::labeled_toggle,
    number_stepper::{Stepper, number_stepper},
};
use super::icons::icon;
use super::{human_duration, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

/// Peaks stored per waveform, and the sample rate they are computed from (P5.10).
const BUCKETS: usize = 1_000;
const SAMPLE_RATE: u32 = 8_000;
/// Bar width before the first paint has measured the real one.
const FALLBACK_WIDTH: f32 = 720.0;
const HEAD_HEIGHT: f32 = 24.0;
const VIDEO_LANE_HEIGHT: f32 = 44.0;
const LANE_HEIGHT: f32 = 40.0;
/// Size of one thumbnail in the video lane (16:9 at the lane's inner height).
const THUMB_H: f32 = 40.0;
const THUMB_W: f32 = 72.0;
/// One waveform bar is at least this far from the previous one.
const BAR_STEP: f32 = 3.0;
/// The picture's height is this share of the window's, within these limits.
const VIDEO_SHARE: f32 = 0.38;
const VIDEO_MIN: f32 = 200.0;
const VIDEO_MAX: f32 = 760.0;
/// The editing panel beside the picture, and the window width from which it sits beside it.
const SIDE_WIDTH: f32 = 380.0;
const WIDE: f32 = 1_100.0;
/// How often the picture is refreshed while playing.
const TICK: Duration = Duration::from_millis(16);
/// Pieces are tinted in turn so neighbours read as separate.
const TINTS: [u32; 2] = [theme::BRAND_SOFT, 0x1a2338];

/// A clip on the timeline's list of sources.
struct Source {
    path: PathBuf,
    name: String,
    info: ProbeInfo,
    /// One per audio track; `None` until computed, or if the track has no usable audio.
    waves: Vec<Option<Vec<f32>>>,
    /// Small pictures along the clip (source time, picture), in time order, for the video lane.
    thumbs: Vec<(Hns, Arc<RenderImage>)>,
}

enum Export {
    Idle,
    Running(String),
    Done { files: Vec<PathBuf>, took: Duration },
    Failed(String),
}

pub struct EditorView {
    app: WeakEntity<ZeroClipApp>,
    ffmpeg_path: Option<PathBuf>,
    ffmpeg: Option<Ffmpeg>,
    store: Option<Arc<Mutex<Store>>>,
    /// The recording's AAC bitrate, reused when an edited track is re-encoded.
    bitrate_bps: u32,
    sources: Vec<Source>,
    timeline: Timeline,
    /// The piece under the playhead.
    selected: Option<usize>,
    /// Names of clips still being read, and the last problem adding one.
    loading: Vec<String>,
    problem: Option<String>,
    /// Bumped when the project is replaced, so a slow job for the old one is ignored.
    generation: u64,
    /// The timeline's bounds from the last paint, to turn mouse positions into times.
    strip: Rc<Cell<Bounds<Pixels>>>,
    /// The width the timeline was last drawn at.
    drawn_width: f32,
    scrubbing: bool,
    player: Player,
    /// Whether a task is already ticking the player.
    ticking: bool,
    picking: bool,
    export: Export,
    /// The "what now?" choice is showing.
    finishing: bool,
    /// A piece edge being dragged.
    trim: Option<TrimDrag>,
    /// "Close the editor?" is showing.
    confirm_close: bool,
    /// Whether the choice is kept for next time.
    remember: bool,
    /// The remembered choice for Done, and the audio track the preview plays.
    finish: FinishChoice,
    listen: usize,
    /// Every audio track is shown and editable; otherwise only Master.
    tracks_open: bool,
    focus: Option<FocusHandle>,
    /// Whether the timeline and editing tools show. Off, the clip plays like in a normal player.
    editing: bool,
    /// The picture is showing over the whole window, with a bar to seek by.
    fullscreen: bool,
    full_bar: Rc<Cell<Bounds<Pixels>>>,
    /// Each audio lane's bounds from the last paint, to turn a mouse position into a level.
    lanes: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    /// The volume point being dragged: piece, track, point.
    drag: Option<(usize, usize, usize)>,
    /// A stretch of one track picked with the mouse (track, piece, from, to, in piece time) and
    /// whether it is still being dragged out. Delete silences it.
    range: Option<(usize, usize, Hns, Hns)>,
    ranging: bool,
    /// Earlier and undone states of the pieces.
    undo: Vec<Vec<Piece>>,
    redo: Vec<Vec<Piece>>,
}

/// A piece edge being dragged: which piece and side, where the mouse went down, how much time a
/// pixel is, and the piece as it was.
struct TrimDrag {
    index: usize,
    left: bool,
    x0: f32,
    per_px: f64,
    orig: Piece,
    /// Where every piece started, and the timeline's length, when the edge was grabbed.
    starts: Vec<Hns>,
    total: Hns,
}

impl EditorView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            ffmpeg_path: None,
            ffmpeg: None,
            store: None,
            bitrate_bps: 160_000,
            sources: Vec::new(),
            timeline: Timeline::default(),
            selected: None,
            loading: Vec::new(),
            problem: None,
            generation: 0,
            strip: Rc::new(Cell::new(Bounds::default())),
            drawn_width: 0.0,
            scrubbing: false,
            lanes: Rc::default(),
            drag: None,
            range: None,
            ranging: false,
            undo: Vec::new(),
            redo: Vec::new(),
            player: Player::default(),
            ticking: false,
            picking: false,
            export: Export::Idle,
            finishing: false,
            confirm_close: false,
            trim: None,
            remember: false,
            finish: FinishChoice::Ask,
            listen: 0,
            tracks_open: false,
            focus: None,
            editing: true,
            fullscreen: false,
            full_bar: Rc::new(Cell::new(Bounds::default())),
        }
    }

    /// Shows the picture over the whole window (and the window itself fullscreen) or puts it back.
    fn set_fullscreen(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.fullscreen = on;
        if window.is_fullscreen() != on {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    /// Starts a new project with `path` on the timeline.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        path: PathBuf,
        watch: bool,
        prefs: Option<(FinishChoice, usize)>,
        ffmpeg_path: Option<PathBuf>,
        store: Option<Arc<Mutex<Store>>>,
        bitrate_bps: u32,
        cx: &mut Context<Self>,
    ) {
        self.ffmpeg_path = ffmpeg_path;
        self.store = store;
        self.bitrate_bps = bitrate_bps;
        self.reset();
        self.editing = !watch;
        // The app hands its saved choices in: reading it from here would panic when it is the one
        // calling (it is being updated).
        if let Some((finish, listen)) = prefs {
            (self.finish, self.listen) = (finish, listen);
        }
        self.add_clip(path, cx);
    }

    /// Empties the project: no clips, no history, no picture.
    fn reset(&mut self) {
        self.generation += 1;
        self.player.pause();
        self.sources.clear();
        self.timeline = Timeline::default();
        self.undo.clear();
        self.redo.clear();
        self.drag = None;
        self.selected = None;
        self.loading.clear();
        self.problem = None;
        self.scrubbing = false;
        self.player.reset();
        self.picking = false;
        self.finishing = false;
        self.confirm_close = false;
        self.range = None;
        self.export = Export::Idle;
    }

    /// Reads `path` in the background, then puts it at the end of the timeline.
    fn add_clip(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let generation = self.generation;
        let ffmpeg_path = self.ffmpeg_path.clone();
        let known = self.ffmpeg.clone();
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.loading.push(name);
        self.problem = None;
        self.picking = false;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        let ffmpeg = match known {
                            Some(ffmpeg) => ffmpeg,
                            None => Ffmpeg::locate(ffmpeg_path.as_deref())?,
                        };
                        let info = ffmpeg.probe(&path)?;
                        Ok::<_, FfmpegError>((ffmpeg, info))
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| this.probed(generation, path, result, cx));
        })
        .detach();
    }

    fn probed(
        &mut self,
        generation: u64,
        path: PathBuf,
        result: Result<(Ffmpeg, ProbeInfo), FfmpegError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.loading.pop();
        match result {
            Ok((ffmpeg, info)) => {
                // Clips differ in how many audio tracks they have (an app that was closed has no
                // track), so those are not compared; every piece keeps the tracks all share.
                let same_audio = |s: StreamInfo| StreamInfo { audio_tracks: 0, ..s };
                if let Some(first) = self.sources.first()
                    && let Err(error) = zc_editor::concat::check_compatible(&[
                        same_audio(first.info.stream),
                        same_audio(info.stream),
                    ])
                {
                    self.problem = Some(format!(
                        "This clip can't join the others without re-encoding: {error}."
                    ));
                    cx.notify();
                    return;
                }
                let own = usize::from(info.stream.audio_tracks);
                let tracks = self.timeline.pieces.first().map_or(own, |p| p.tracks.len().min(own));
                if tracks < own
                    || self.timeline.pieces.first().is_some_and(|p| p.tracks.len() > tracks)
                {
                    self.problem = Some(format!(
                        "The clips have different numbers of audio tracks, so the joined video keeps                          the first {tracks} of each."
                    ));
                }
                let index = self.sources.len();
                let name =
                    path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                self.checkpoint();
                for piece in &mut self.timeline.pieces {
                    piece.tracks.truncate(tracks);
                }
                self.timeline.pieces.push(Piece {
                    source: index,
                    a: Hns::ZERO,
                    b: info.duration,
                    tracks: vec![TrackEdit::default(); tracks],
                });
                self.sources.push(Source {
                    path,
                    name,
                    info,
                    waves: vec![None; own],
                    thumbs: Vec::new(),
                });
                for track in 0..own {
                    self.spawn_waveform(generation, &ffmpeg, index, track, cx);
                }
                self.spawn_thumbs(generation, &ffmpeg, index, cx);
                let first = self.ffmpeg.is_none();
                self.ffmpeg = Some(ffmpeg);
                if first {
                    self.seek(Hns::ZERO, cx);
                }
            }
            Err(error) => {
                let hint = if matches!(error, FfmpegError::NotFound) {
                    " Set its path in Settings, Advanced, Editor."
                } else {
                    ""
                };
                self.problem = Some(format!("{error}.{hint}"));
            }
        }
        cx.notify();
    }

    /// Track 0 comes from the store when it is there (the cache holds one waveform per file);
    /// the others are computed each time. The store lock is never held while ffmpeg runs.
    fn spawn_waveform(
        &mut self,
        generation: u64,
        ffmpeg: &Ffmpeg,
        source: usize,
        track: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.sources.get(source).map(|s| s.path.clone()) else { return };
        let (ffmpeg, store) = (ffmpeg.clone(), self.store.clone());
        cx.spawn(async move |this, cx| {
            let peaks = cx
                .background_executor()
                .spawn(async move { waveform(&ffmpeg, store.as_deref(), &path, track) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation == this.generation
                    && let Some(slot) =
                        this.sources.get_mut(source).and_then(|s| s.waves.get_mut(track))
                {
                    *slot = peaks;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Makes the video lane's thumbnails, one ffmpeg still at a time on a background thread, so
    /// they appear one by one and never hold up the editor.
    fn spawn_thumbs(
        &mut self,
        generation: u64,
        ffmpeg: &Ffmpeg,
        source: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.sources.get(source) else { return };
        let (path, duration) = (s.path.clone(), s.info.duration);
        let count = ((duration.as_secs_f64() / 2.0) as usize).clamp(8, 40);
        let ffmpeg = ffmpeg.clone();
        cx.spawn(async move |this, cx| {
            for k in 0..count {
                // Mid-slot, so the first and last pictures sit inside the clip.
                let at =
                    Hns::from_secs_f64(duration.as_secs_f64() * (k as f64 + 0.5) / count as f64);
                let (ffmpeg, path) = (ffmpeg.clone(), path.clone());
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        player::still(&ffmpeg, &path, at, THUMB_W as u32, THUMB_H as u32)
                    })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    if generation != this.generation {
                        return false;
                    }
                    if let (Some(image), Some(s)) = (image, this.sources.get_mut(source)) {
                        s.thumbs.push((at, image));
                        cx.notify();
                    }
                    true
                });
                if !alive.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Adds every clip in `paths` to the end of the timeline. Anything that isn't a file is
    /// reported, not silently skipped.
    fn add_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let mut skipped = 0;
        for path in paths {
            if path.is_file() {
                self.add_clip(path.clone(), cx);
            } else {
                skipped += 1;
            }
        }
        if skipped > 0 {
            self.problem = Some(format!("{skipped} of those aren't files."));
            cx.notify();
        }
    }

    /// Ctrl+V: file paths on the clipboard, copied in Explorer or as text, join the timeline.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in item.entries() {
            if let gpui::ClipboardEntry::ExternalPaths(p) = entry {
                paths.extend(p.paths().iter().cloned());
            }
        }
        if paths.is_empty()
            && let Some(text) = item.text()
        {
            paths.extend(
                text.lines()
                    .map(|l| PathBuf::from(l.trim().trim_matches('"')))
                    .filter(|p| !p.as_os_str().is_empty()),
            );
        }
        if paths.is_empty() {
            self.problem = Some("Nothing to paste. Copy a clip's file or its path first.".into());
            cx.notify();
        } else {
            self.add_paths(&paths, cx);
        }
    }

    /// Timeline x, window-relative, to a time.
    fn time_at(&self, x: Pixels) -> Hns {
        let bounds = self.strip.get();
        let width = f32::from(bounds.size.width);
        model::x_to_time(f32::from(x - bounds.origin.x), self.timeline.total(), width)
    }

    /// Pauses and shows the frame at timeline time `t`.
    fn seek(&mut self, t: Hns, cx: &mut Context<Self>) {
        let t = t.min(self.timeline.total());
        let Some((i, src)) = self.timeline.locate(t) else { return };
        let Some(ffmpeg) = self.ffmpeg.as_ref() else { return };
        let path = &self.sources[self.timeline.pieces[i].source].path;
        self.player.show_at(ffmpeg, path, src, t);
        self.selected = Some(i);
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Stops playback, picture and sound. Called when the user leaves the Editor tab.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.player.pause();
            cx.notify();
        }
    }

    /// Play from the playhead, or pause. At the very end it starts over.
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.player.pause();
            cx.notify();
            return;
        }
        let total = self.timeline.total();
        let mut at = self.player.playhead;
        if total - at < Hns::from_millis(200) {
            at = Hns::ZERO;
        }
        let Some((first, src)) = self.timeline.locate(at) else { return };
        let Some(ffmpeg) = self.ffmpeg.as_ref() else { return };
        let items: Vec<Item> = self
            .timeline
            .pieces
            .iter()
            .enumerate()
            .skip(first)
            .map(|(n, piece)| Item {
                path: self.sources[piece.source].path.clone(),
                from: if n == first { src } else { piece.a },
                to: piece.b,
                sound: self.sound_of(piece, if n == first { src } else { piece.a }),
            })
            .collect();
        self.player.play(ffmpeg, items, at);
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// What the preview plays for `piece`: the listened-to track, unless it is muted.
    fn sound_of(&self, piece: &Piece, from: Hns) -> Option<Sound> {
        let track = self.listen.min(piece.tracks.len().checked_sub(1)?);
        let edit = &piece.tracks[track];
        (!edit.mute).then(|| Sound {
            track,
            gain_db: edit.gain_db,
            line: edit.line_from(from - piece.a),
        })
    }

    /// Starts the task that refreshes the picture while the player has work, unless one runs.
    fn ensure_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep = this
                    .update(cx, |this, cx| {
                        let busy = this.player.tick();
                        if this.player.playing {
                            this.selected = this.timeline.locate(this.player.playhead).map(|l| l.0);
                        }
                        cx.notify();
                        if !busy {
                            this.ticking = false;
                        }
                        busy
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    fn release(&mut self) {
        self.trim = None;
        self.scrubbing = false;
        self.drag = None;
        self.ranging = false;
        // A click, not a drag: no range.
        if self.range.is_some_and(|(_, _, a, b)| b.max(a) - b.min(a) < Hns::from_millis(100)) {
            self.range = None;
        }
    }

    /// Silences the picked stretch of its track.
    fn silence_range(&mut self, cx: &mut Context<Self>) {
        let Some((t, i, a, b)) = self.range.take() else { return };
        let Some(piece) = self.timeline.pieces.get(i) else { return };
        let span = piece.span();
        self.checkpoint();
        self.timeline.pieces[i].tracks[t].silence(a.min(b), a.max(b), span);
        // Playback restarts at the playhead so the new edit is heard.
        let at = self.player.playhead;
        self.seek(at, cx);
    }

    /// Where, in piece `i`, timeline x `x` falls, as time from the piece's start.
    fn piece_time(&self, i: usize, x: Pixels) -> Hns {
        let t = self.time_at(x) - self.timeline.start_of(i);
        t.clamp(Hns::ZERO, self.timeline.pieces[i].span())
    }

    /// A press on audio lane `t`: grabs a volume point (Shift removes it), starts a new one on the
    /// line, or else scrubs.
    fn press_lane(&mut self, t: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        const GRAB: f32 = 9.0;
        let lane = self.lanes.borrow().get(t).copied();
        let here = self.time_at(ev.position.x);
        let (Some(lane), Some((i, _))) = (lane, self.timeline.locate(here)) else {
            self.scrubbing = true;
            self.seek(here, cx);
            return;
        };
        let span = self.timeline.pieces[i].span();
        let bounds = self.strip.get();
        let total = self.timeline.total();
        let to_x = |at: Hns| {
            model::time_to_x(self.timeline.start_of(i) + at, total, f32::from(bounds.size.width))
        };
        let (mx, my) =
            (f32::from(ev.position.x - bounds.origin.x), f32::from(ev.position.y - lane.origin.y));
        let y_of = |db: f64| model::db_to_frac(db) * LANE_HEIGHT;
        let edit = &self.timeline.pieces[i].tracks[t];
        let line = edit.line(span);
        let near = |pt: &model::EnvPoint| {
            (to_x(pt.at) - mx).abs() <= GRAB && (y_of(pt.db) - my).abs() <= GRAB
        };
        let grabbed = if edit.has_line() { line.iter().position(near) } else { None };
        let at = self.piece_time(i, ev.position.x);
        let on_line = (y_of(edit.db_at(at)) - my).abs() <= GRAB && at > Hns::ZERO && at < span;
        if let Some(k) = grabbed {
            self.checkpoint();
            self.selected = Some(i);
            if ev.modifiers.shift {
                if k != 0 && k + 1 != line.len() {
                    self.timeline.pieces[i].tracks[t].remove_point(k);
                }
            } else {
                self.drag = Some((i, t, k));
            }
            cx.notify();
        } else if on_line {
            self.checkpoint();
            self.selected = Some(i);
            let db = model::frac_to_db(my / LANE_HEIGHT);
            let k = self.timeline.pieces[i].tracks[t].add_point(at, db, span);
            self.drag = Some((i, t, k));
            cx.notify();
        } else {
            // Drag across the lane to pick a stretch; a plain click just moves the playhead.
            self.range = Some((t, i, at, at));
            self.ranging = true;
            self.seek(here, cx);
        }
    }

    /// Drags the grabbed piece edge to the mouse, from the piece as it was when grabbed.
    fn drag_edge(&mut self, ev: &MouseMoveEvent) {
        let Some(t) = &self.trim else { return };
        let Some(source) = self.sources.get(t.orig.source) else { return };
        let by = Hns::from_ticks((f64::from(f32::from(ev.position.x) - t.x0) * t.per_px) as i64);
        let (a, b) = if t.left { (t.orig.a + by, t.orig.b) } else { (t.orig.a, t.orig.b + by) };
        let trimmed = t.orig.trimmed(a, b, source.info.duration);
        if let Some(piece) = self.timeline.pieces.get_mut(t.index) {
            *piece = trimmed;
        }
    }

    /// Moves the grabbed
    fn drag_point(&mut self, ev: &MouseMoveEvent) {
        let Some((i, t, k)) = self.drag else { return };
        let Some(lane) = self.lanes.borrow().get(t).copied() else { return };
        if i >= self.timeline.pieces.len() {
            return;
        }
        let span = self.timeline.pieces[i].span();
        let at = self.piece_time(i, ev.position.x);
        let frac = f32::from(ev.position.y - lane.origin.y) / LANE_HEIGHT;
        self.timeline.pieces[i].tracks[t].move_point(k, at, model::frac_to_db(frac), span);
    }

    /// Remembers the pieces as they are, before a change.
    fn checkpoint(&mut self) {
        self.undo.push(self.timeline.pieces.clone());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Steps back (`forward` false) or forward through the history.
    fn step_history(&mut self, forward: bool, cx: &mut Context<Self>) {
        let (from, to) = if forward {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some(state) = from.pop() else { return };
        to.push(std::mem::replace(&mut self.timeline.pieces, state));
        self.problem = None;
        self.drag = None;
        self.range = None;
        let at = self.player.playhead.min(self.timeline.total());
        self.selected = self.timeline.locate(at).map(|l| l.0);
        self.seek(at, cx);
        cx.notify();
    }

    /// Joins the selected piece to the one after it.
    fn join_selected(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        self.checkpoint();
        match self.timeline.join(i) {
            Ok(j) => {
                self.problem = None;
                self.selected = Some(j);
            }
            Err(message) => {
                self.undo.pop();
                self.problem = Some(message);
            }
        }
        cx.notify();
    }

    /// Cuts the piece under the playhead in two.
    fn split(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        let Some(piece) = self.timeline.pieces.get(i) else { return };
        let keyframes = &self.sources[piece.source].info.keyframes;
        let at = self.player.playhead;
        let before = self.timeline.pieces.clone();
        match self.timeline.split(at, keyframes) {
            Ok(new) => {
                self.undo.push(before);
                self.redo.clear();
                self.problem = None;
                self.selected = Some(new);
            }
            Err(message) => self.problem = Some(message),
        }
        cx.notify();
    }

    /// Removes the selected piece; the ones after it close up.
    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        let at = self.timeline.start_of(i);
        self.checkpoint();
        self.timeline.delete(i);
        self.problem = None;
        self.seek(at, cx);
        if self.timeline.pieces.is_empty() {
            self.selected = None;
        }
        cx.notify();
    }

    fn shift_selected(&mut self, by: isize, cx: &mut Context<Self>) {
        if let Some(i) = self.selected
            && i.checked_add_signed(by).is_some_and(|j| j < self.timeline.pieces.len())
            && let Some(j) = {
                self.checkpoint();
                self.timeline.shift(i, by)
            }
        {
            let at = self.timeline.start_of(j);
            self.seek(at, cx);
        }
    }

    fn edit_track(
        &mut self,
        piece: usize,
        track: usize,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut TrackEdit),
    ) {
        self.checkpoint();
        if let Some(edit) =
            self.timeline.pieces.get_mut(piece).and_then(|p| p.tracks.get_mut(track))
        {
            f(edit);
            cx.notify();
        }
    }

    /// Picks the audio track the preview plays, and remembers it.
    fn set_listen(&mut self, track: usize, cx: &mut Context<Self>) {
        self.listen = track;
        let track = u8::try_from(track).unwrap_or(0);
        self.remember_prefs(cx, move |e| e.listen_track = track);
        cx.notify();
    }

    /// Saves an editor preference now, without touching the Settings screen's unsaved edits.
    fn remember_prefs(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut EditorSettings) + 'static,
    ) {
        let _ = self.app.update(cx, |app, _| app.remember_editor(f));
    }

    /// The plans for pieces `which`, or a message for the first that can't be made.
    fn plans(&self, which: &[usize]) -> Result<Vec<PiecePlan>, String> {
        which
            .iter()
            .map(|&i| {
                let piece = self.timeline.pieces.get(i).ok_or("That piece is gone.")?;
                let info = &self.sources[piece.source].info;
                model::plan_piece(piece, info.duration, &info.keyframes, self.bitrate_bps)
            })
            .collect()
    }

    /// Done: asks what to do unless a choice was remembered.
    fn done(&mut self, cx: &mut Context<Self>) {
        if self.timeline.pieces.is_empty() || matches!(self.export, Export::Running(_)) {
            return;
        }
        match self.finish {
            FinishChoice::Ask => self.finishing = true,
            choice => self.run_choice(choice, cx),
        }
        cx.notify();
    }

    fn choose(&mut self, choice: FinishChoice, cx: &mut Context<Self>) {
        self.finishing = false;
        if self.remember {
            self.finish = choice;
            self.remember_prefs(cx, move |e| e.finish = choice);
        }
        self.run_choice(choice, cx);
    }

    fn run_choice(&mut self, choice: FinishChoice, cx: &mut Context<Self>) {
        match choice {
            FinishChoice::SaveAs => self.save_all(cx),
            FinishChoice::Extract => {
                let all: Vec<usize> = (0..self.timeline.pieces.len()).collect();
                self.extract(&all, cx);
            }
            FinishChoice::Ask => {}
        }
    }

    /// Joins every piece into one new file next to the first clip.
    fn save_all(&mut self, cx: &mut Context<Self>) {
        let all: Vec<usize> = (0..self.timeline.pieces.len()).collect();
        let Some(first) = self.timeline.pieces.first().map(|p| &self.sources[p.source].path) else {
            return;
        };
        let output = model::numbered_name(
            first.parent().unwrap_or_else(|| Path::new("")),
            &model::stem_of(first),
            "edited",
            Path::exists,
        );
        match self.plans(&all) {
            Ok(plans) => self.run_jobs("Saving.", vec![(plans, output)], cx),
            Err(message) => self.fail(message, cx),
        }
    }

    /// Saves each of pieces `which` as its own file next to its clip. The timeline is untouched,
    /// so editing carries on.
    fn extract(&mut self, which: &[usize], cx: &mut Context<Self>) {
        if matches!(self.export, Export::Running(_)) {
            return;
        }
        let plans = match self.plans(which) {
            Ok(plans) => plans,
            Err(message) => return self.fail(message, cx),
        };
        let mut taken: Vec<PathBuf> = Vec::new();
        let mut jobs = Vec::new();
        for (&i, plan) in which.iter().zip(plans) {
            let source = &self.sources[self.timeline.pieces[i].source].path;
            let tag = format!("clip {}", model::piece_label(i));
            let output = model::numbered_name(
                source.parent().unwrap_or_else(|| Path::new("")),
                &model::stem_of(source),
                &tag,
                |p| p.exists() || taken.iter().any(|t| t == p),
            );
            taken.push(output.clone());
            jobs.push((vec![plan], output));
        }
        self.run_jobs("Extracting.", jobs, cx);
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.export = Export::Failed(message);
        cx.notify();
    }

    /// Runs each job (its plans joined into its output file) one after another in the background.
    fn run_jobs(
        &mut self,
        label: &str,
        jobs: Vec<(Vec<PiecePlan>, PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        let Some(ffmpeg) = self.ffmpeg.clone() else { return };
        let sources: Vec<PathBuf> = self.sources.iter().map(|s| s.path.clone()).collect();
        let infos: Vec<StreamInfo> = self.sources.iter().map(|s| s.info.stream).collect();
        self.export = Export::Running(label.to_owned());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut files = Vec::new();
                    let mut took = Duration::ZERO;
                    for (plans, output) in jobs {
                        took += export::run(&ffmpeg, &plans, &sources, &infos, &output)?;
                        files.push(output);
                    }
                    Ok::<_, anyhow::Error>((files, took))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.export = match result {
                    Ok((files, took)) => Export::Done { files, took },
                    Err(error) => Export::Failed(format!("{error:#}")),
                };
                this.report_export(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Mirrors the export result into the activity feed and refreshes the Clips list.
    fn report_export(&mut self, cx: &mut Context<Self>) {
        let (level, text) = match &self.export {
            Export::Done { files, took } => (
                NoticeLevel::Info,
                format!(
                    "Saved {} in {:.1} s.",
                    if files.len() == 1 {
                        files[0]
                            .file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                    } else {
                        format!("{} clips", files.len())
                    },
                    took.as_secs_f64()
                ),
            ),
            Export::Failed(error) => (NoticeLevel::Error, format!("Export failed: {error}")),
            _ => return,
        };
        let _ = self.app.update(cx, |app, cx| {
            app.clips.invalidate();
            app.notify(level, text);
            cx.notify();
        });
    }

    fn key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let plain = !(k.modifiers.control || k.modifiers.alt || k.modifiers.platform);
        match k.key.as_str() {
            "f" if plain => self.set_fullscreen(!self.fullscreen, window, cx),
            "escape" if self.fullscreen => self.set_fullscreen(false, window, cx),
            "space" if plain => self.toggle_play(cx),
            // Watching is only watching: the editing keys wait for the Edit button.
            _ if !self.editing => {}
            "s" if plain => self.split(cx),
            "delete" | "backspace" if plain && self.range.is_some() => self.silence_range(cx),
            "delete" | "backspace" if plain => self.delete_selected(cx),
            "j" if plain => self.join_selected(cx),
            "z" if k.modifiers.control && !k.modifiers.alt => {
                self.step_history(k.modifiers.shift, cx);
            }
            "y" if k.modifiers.control => self.step_history(true, cx),
            "v" if k.modifiers.control => self.paste(cx),
            _ => {}
        }
    }
}

/// Track `track`'s waveform: from the store (track 0 only), else decoded and, for track 0, stored.
fn waveform(
    ffmpeg: &Ffmpeg,
    store: Option<&Mutex<Store>>,
    path: &Path,
    track: usize,
) -> Option<Vec<f32>> {
    let key = match file_key(path) {
        Ok(key) => key,
        Err(error) => {
            tracing::debug!(%error, "waveform: couldn't key the file");
            return None;
        }
    };
    if track == 0
        && let Some(peaks) =
            store.and_then(|s| s.lock().ok()?.waveform(&key, BUCKETS).ok().flatten())
    {
        tracing::debug!("waveform cache hit");
        return Some(peaks);
    }
    let samples = match ffmpeg.track_samples(path, track, SAMPLE_RATE) {
        Ok(samples) => samples,
        Err(error) => {
            tracing::debug!(%error, track, "waveform: no usable audio");
            return None;
        }
    };
    let peaks = rms_peaks(&samples, BUCKETS);
    if track == 0
        && let Some(store) = store
        && let Ok(store) = store.lock()
        && let Err(error) = store.put_waveform(&key, &peaks)
    {
        tracing::debug!(%error, "waveform: couldn't cache");
    }
    Some(peaks)
}

/// What track `i` of `source` is called: the name the recorder wrote into the file (Master, Game,
/// Mic, then each app's label), else a generic one.
/// The thumbnails that tile a piece `width` px wide: one `THUMB_W` slot at a time, each taking the
/// picture nearest the middle of its slot. Returns each slot's x within the piece.
fn thumb_tiles(
    thumbs: &[(Hns, Arc<RenderImage>)],
    a: Hns,
    span: Hns,
    width: f32,
) -> Vec<(f32, Arc<RenderImage>)> {
    if thumbs.is_empty() || width < 1.0 {
        return Vec::new();
    }
    (0..(width / THUMB_W).ceil() as usize)
        .filter_map(|k| {
            let x = k as f32 * THUMB_W;
            let mid = ((x + THUMB_W / 2.0).min(width) / width).clamp(0.0, 1.0);
            let t = a.ticks() + (span.ticks() as f64 * f64::from(mid)) as i64;
            let (_, image) = thumbs.iter().min_by_key(|(at, _)| (at.ticks() - t).abs())?;
            Some((x, image.clone()))
        })
        .collect()
}

fn track_label(source: &Source, i: usize) -> String {
    match source.info.audio_names.get(i) {
        Some(name) if !name.is_empty() => name.clone(),
        _ => track_name(i),
    }
}

/// The name for track `i` when the file doesn't carry one: Master first, then Game and Mic.
fn track_name(i: usize) -> String {
    match i {
        0 => "Master".to_owned(),
        1 => "Game".to_owned(),
        2 => "Mic".to_owned(),
        n => format!("Track {}", n + 1),
    }
}

/// `m:ss.t`, for the player's clock.
fn clock(t: Hns) -> String {
    let secs = t.as_secs_f64().max(0.0);
    let whole = secs as u64;
    format!("{}:{:02}.{}", whole / 60, whole % 60, ((secs - whole as f64) * 10.0) as u64)
}

fn muted(text: impl Into<SharedString>) -> gpui::Div {
    div().text_sm().text_color(rgb(theme::MUTED_FG)).child(text.into())
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Frames that left the screen are removed from the GPU cache, or playback fills it.
        for old in self.player.spent.drain(..) {
            let _ = window.drop_image(old);
        }
        // The timeline's real width is only known after a paint; draw again once it is.
        let measured = f32::from(self.strip.get().size.width);
        if measured > 0.0 && (measured - self.drawn_width).abs() > 0.5 {
            self.drawn_width = measured;
            cx.notify();
        }
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        // Keys only reach the page while it has focus, which coming back from another tab loses.
        if !focus.contains_focused(window, cx) {
            window.focus(&focus, cx);
        }
        let viewport = window.viewport_size();
        let share = if self.editing { VIDEO_SHARE } else { 0.7 };
        let video_height = (f32::from(viewport.height) * share).clamp(VIDEO_MIN, VIDEO_MAX * 1.5);
        let wide = f32::from(viewport.width) >= WIDE;

        let mut page = div()
            .id("editor-page")
            .track_focus(&focus)
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .border_2()
            .border_color(rgb(theme::BG))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.border_color(rgb(theme::BRAND)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_paths(paths.paths(), cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if let Some(focus) = &this.focus {
                        window.focus(focus, cx);
                    }
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, window, cx| this.key(ev, window, cx)),
            )
            .child(self.header(cx));
        if let Some(problem) = &self.problem {
            page =
                page.child(div().text_sm().text_color(rgb(theme::DANGER)).child(problem.clone()));
        }
        if !self.loading.is_empty() {
            page = page.child(muted(format!("Reading {}.", self.loading.join(", "))));
        }
        if self.timeline.pieces.is_empty() && self.loading.is_empty() {
            page = page.child(self.drop_zone(cx));
        }
        if self.picking {
            page = page.child(self.picker(cx));
        }
        if self.confirm_close {
            page = page.child(self.close_prompt(cx));
        }
        if !self.timeline.pieces.is_empty() && !self.editing {
            page = page.child(self.player_view(video_height, cx));
        }
        if !self.timeline.pieces.is_empty() && self.editing {
            // Wide windows put the editing panel beside the picture; narrow ones stack it below.
            let side = div()
                .flex()
                .flex_col()
                .gap_3()
                .flex_none()
                .children(self.inspector(cx))
                .child(self.finish_card(cx));
            let top = div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().flex_1().min_w_0().child(self.player_view(video_height, cx)));
            let top =
                if wide { top.flex_row().child(side.w(px(SIDE_WIDTH))) } else { top.child(side) };
            page = page.child(top).child(self.timeline_view(cx));
        }
        div().relative().size_full().child(page).children(
            (self.fullscreen && !self.timeline.pieces.is_empty()).then(|| self.full_view(cx)),
        )
    }
}

impl EditorView {
    fn header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let length = (!self.timeline.pieces.is_empty()).then(|| {
            format!(
                "{} {}, {} long",
                self.timeline.pieces.len(),
                if self.timeline.pieces.len() == 1 { "piece" } else { "pieces" },
                human_duration(self.timeline.total())
            )
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Editor"))
            .children(length.map(muted))
            .child(div().flex_1())
            .child(
                button("editor-pick", "Add clip")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picking = !this.picking;
                        cx.notify();
                    }))
                    .child(icon("plus", theme::FG)),
            )
            .children((!self.timeline.pieces.is_empty()).then(|| {
                button_ghost("editor-close", "Close").child(icon("x", theme::FG)).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.confirm_close = true;
                        cx.notify();
                    }),
                )
            }))
    }

    /// The empty editor: a big target for dropping a video, and a + that opens the clip list.
    fn drop_zone(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("editor-drop")
            .w_full()
            .h(px(260.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .rounded(px(theme::RADIUS))
            .border_2()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::CARD))
            .cursor_pointer()
            .hover(|s| s.border_color(rgb(theme::BRAND)))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.border_color(rgb(theme::BRAND)))
            .child(
                div()
                    .size(px(56.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(theme::BG))
                    .child(icon("plus", theme::BRAND).size(px(28.))),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Drop a video here"))
            .child(muted(if self.picking {
                "or pick one of your clips below"
            } else {
                "or click to open one of your clips"
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.picking = !this.picking;
                cx.notify();
            }))
    }

    /// Asks before closing the project, since closing drops every edit.
    fn close_prompt(&self, cx: &mut Context<Self>) -> gpui::Div {
        card("Close the editor?")
            .child(muted(
                "Are you sure you want to continue? The pieces and every edit you made are dropped. Clips on disk are not touched.",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(button_primary("close-yes", "Yes, close it").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.reset();
                            cx.notify();
                        },
                    )))
                    .child(button("close-no", "Keep editing").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.confirm_close = false;
                            cx.notify();
                        },
                    ))),
            )
    }

    /// The video clips of the output folder; picking one adds it. Dropping a file on the editor or
    /// pasting its path (Ctrl+V) works too.
    fn picker(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(app) = self.app.upgrade() else { return div() };
        let dir = app.read(cx).output_dir();
        let videos = app.update(cx, |app, _| {
            app.clips.ensure_scanned(&dir);
            app.clips.videos()
        });
        let body = if videos.is_empty() {
            muted("No videos yet. Saved clips will show up here.").into_any_element()
        } else {
            div()
                .id("editor-picker")
                .max_h(px(180.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(videos.into_iter().enumerate().map(|(i, (name, path))| {
                    div()
                        .id(("editor-clip", i))
                        .px_2()
                        .py_1()
                        .rounded(px(theme::RADIUS_SM))
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::ACCENT)))
                        .child(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            if this.sources.is_empty() {
                                let (ffmpeg, store, bitrate) = this
                                    .app
                                    .upgrade()
                                    .map(|a| {
                                        let a = a.read(cx);
                                        (
                                            a.settings.editor.ffmpeg_path.clone(),
                                            a.store.clone(),
                                            a.settings.audio.bitrate_kbps * 1_000,
                                        )
                                    })
                                    .unwrap_or((None, None, this.bitrate_bps));
                                this.open(path, false, None, ffmpeg, store, bitrate, cx);
                            } else {
                                this.add_clip(path, cx);
                            }
                        }))
                }))
                .into_any_element()
        };
        card("Your clips")
            .child(muted("Pick one, drop video files anywhere on this page, or paste a file or its path with Ctrl+V."))
            .child(body)
    }

    /// The picture, with play/pause, the clock, and the buttons that edit at the playhead.
    fn player_view(&self, height: f32, cx: &mut Context<Self>) -> gpui::Div {
        let video = div()
            .w_full()
            .h(px(height))
            .flex_none()
            // The page scrolls, so without this the flex column squeezes the picture to nothing.
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(0x000000))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        let video = match &self.player.shown {
            Some(frame) => video.child(
                img(ImageSource::Render(frame.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => video.child(muted("Loading the picture.")),
        };
        let total = self.timeline.total();
        let play = div()
            .id("editor-play")
            .w(px(40.))
            .h(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::PRIMARY))
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            .child(icon(if self.player.playing { "pause" } else { "play" }, theme::PRIMARY_FG))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx)));
        let controls =
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(play)
                .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                    "{} / {}",
                    clock(self.player.playhead),
                    clock(total)
                )))
                .child(div().flex_1())
                .child(button_ghost("editor-fullscreen", "Fullscreen (F)").on_click(
                    cx.listener(|this, _, window, cx| this.set_fullscreen(true, window, cx)),
                ))
                .children(self.editing.then(|| {
                    button("editor-split", "Split")
                        .child(icon("scissors", theme::FG))
                        .on_click(cx.listener(|this, _, _, cx| this.split(cx)))
                }))
                .children(self.editing.then(|| {
                    button_ghost("editor-delete", "Delete piece")
                        .child(icon("trash-2", theme::MUTED_FG))
                        .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx)))
                }))
                .children((!self.editing).then(|| {
                    button_primary("editor-edit", "Edit")
                        .on_click(cx.listener(|this, _, _, cx| this.start_editing(cx)))
                }));
        let help = if self.editing {
            "Space plays, S splits, J joins, Delete removes, Ctrl+Z undoes. On a lane, drag to pick a stretch and press Delete to silence it; click the line to add a volume point."
        } else {
            "Space plays, F is fullscreen. Press Edit to cut, trim and change the sound."
        };
        div().flex().flex_col().gap_2().child(video).child(controls).child(muted(help))
    }

    /// Leaves the plain player for the full editor, on the same clip and place.
    fn start_editing(&mut self, cx: &mut Context<Self>) {
        self.editing = true;
        cx.notify();
    }

    /// The picture over the whole editor: play, the time, and a bar of the pieces to seek by, so
    /// an edit can be watched at full size. Esc or F goes back.
    fn full_view(&self, cx: &mut Context<Self>) -> gpui::Div {
        let total = self.timeline.total();
        let bar = self.full_bar.clone();
        let width = f32::from(bar.get().size.width).max(1.0);
        let mut strip = div()
            .id("editor-full-bar")
            .relative()
            .w_full()
            .h(px(18.))
            .rounded(px(theme::RADIUS_SM))
            .overflow_hidden()
            .cursor_pointer()
            .child(canvas(move |b, _, _| bar.set(b), |_, (), _, _| {}).absolute().size_full())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let bounds = this.full_bar.get();
                    let x = f32::from(ev.position.x - bounds.origin.x);
                    let t =
                        model::x_to_time(x, this.timeline.total(), f32::from(bounds.size.width));
                    this.seek(t, cx);
                }),
            );
        for (i, piece) in self.timeline.pieces.iter().enumerate() {
            let x0 = model::time_to_x(self.timeline.start_of(i), total, width);
            let x1 = model::time_to_x(self.timeline.start_of(i) + piece.span(), total, width);
            strip =
                strip.child(
                    div().absolute().left(px(x0)).w(px((x1 - x0 - 1.0).max(1.0))).h_full().bg(rgb(
                        if self.selected == Some(i) { theme::BRAND } else { TINTS[i % 2] },
                    )),
                );
        }
        strip = strip.child(
            div()
                .absolute()
                .left(px(model::time_to_x(self.player.playhead, total, width) - 1.0))
                .w(px(2.))
                .h_full()
                .bg(rgb(theme::FG)),
        );
        let picture = match &self.player.shown {
            Some(frame) => div().flex_1().min_h_0().child(
                img(ImageSource::Render(frame.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => div().flex_1().child(muted("Loading the picture.")),
        };
        let play = div()
            .id("editor-full-play")
            .w(px(40.))
            .h(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::PRIMARY))
            .cursor_pointer()
            .child(icon(if self.player.playing { "pause" } else { "play" }, theme::PRIMARY_FG))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx)));
        let bar_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(play)
            .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                "{} / {}   Piece {}",
                clock(self.player.playhead),
                clock(total),
                self.selected.map_or(String::new(), model::piece_label)
            )))
            .child(div().flex_1())
            .child(button("editor-full-exit", "Exit fullscreen").on_click(
                cx.listener(|this, _, window, cx| this.set_fullscreen(false, window, cx)),
            ));
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(rgb(0x000000))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(picture)
            .child(strip)
            .child(bar_row)
    }

    /// The pieces on a ruler with an extract button above each, then one lane per audio track.
    fn timeline_view(&self, cx: &mut Context<Self>) -> gpui::Div {
        let width = if self.drawn_width > 0.0 { self.drawn_width } else { FALLBACK_WIDTH };
        // While an edge is dragged the scale and the other pieces stay put, so the edge follows the
        // mouse and what was cut off shows as a gray area; the pieces close up on release.
        let total = self.trim.as_ref().map_or_else(|| self.timeline.total(), |t| t.total);
        let x_of = |t: Hns| model::time_to_x(t, total, width);
        let place = |i: usize| -> (f32, f32) {
            let span = self.timeline.pieces[i].span();
            let start = match &self.trim {
                Some(t) if i < t.starts.len() => {
                    let moved = if t.left && i == t.index {
                        self.timeline.pieces[i].a - t.orig.a
                    } else {
                        Hns::ZERO
                    };
                    t.starts[i] + moved
                }
                _ => self.timeline.start_of(i),
            };
            (x_of(start), x_of(start + span))
        };
        let tracks = self.timeline.pieces.first().map_or(0, |p| p.tracks.len());

        // Above the ruler: each piece's letter, its length and the extract button.
        let mut head = div().relative().w_full().h(px(HEAD_HEIGHT));
        for (i, piece) in self.timeline.pieces.iter().enumerate() {
            let (x0, x1) = place(i);
            let w = (x1 - x0).max(1.0);
            let label = if w >= 150.0 {
                format!("{}  {}", model::piece_label(i), model::secs_label(piece.span()))
            } else if w >= 60.0 {
                model::piece_label(i)
            } else {
                String::new()
            };
            let extract = (w >= 24.0).then(|| {
                div()
                    .id(("extract", i))
                    .w(px(20.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_SM))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ACCENT)))
                    .child(icon("download", theme::MUTED_FG))
                    .on_click(cx.listener(move |this, _, _, cx| this.extract(&[i], cx)))
            });
            head = head.child(
                div()
                    .absolute()
                    .left(px(x0))
                    .w(px(w))
                    .h_full()
                    .px_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(rgb(if self.selected == Some(i) {
                        theme::BRAND
                    } else {
                        theme::MUTED_FG
                    }))
                    .child(div().absolute().left(px(4.)).child(label))
                    .children(extract),
            );
        }

        let bounds = self.strip.clone();
        let mut video_lane = div()
            .relative()
            .w_full()
            .h(px(VIDEO_LANE_HEIGHT))
            .rounded(px(theme::RADIUS_SM))
            .bg(rgb(theme::BG))
            .overflow_hidden()
            .child(canvas(move |b, _, _| bounds.set(b), |_, (), _, _| {}).absolute().size_full());
        for (i, piece) in self.timeline.pieces.iter().enumerate() {
            let (x0, x1) = place(i);
            let selected = self.selected == Some(i);
            // Handles on the selected piece's two edges: drag to trim it.
            let handles: Vec<gpui::Stateful<gpui::Div>> = if selected {
                [true, false]
                    .into_iter()
                    .map(|left| {
                        let piece = piece.clone();
                        let starts: Vec<Hns> = (0..self.timeline.pieces.len())
                            .map(|k| self.timeline.start_of(k))
                            .collect();
                        let total_now = self.timeline.total();
                        let per_px = f64::from(self.timeline.total().ticks() as f32)
                            / f64::from(f32::from(self.strip.get().size.width).max(1.0));
                        let side = div().id(("trim", i * 2 + usize::from(left))).absolute().top_0();
                        let side = if left { side.left_0() } else { side.right_0() };
                        side.w(px(10.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(theme::BRAND))
                            .cursor_col_resize()
                            .child(div().w(px(2.)).h(px(16.)).rounded_full().bg(rgb(theme::BG)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                    this.checkpoint();
                                    this.trim = Some(TrimDrag {
                                        index: i,
                                        left,
                                        x0: f32::from(ev.position.x),
                                        per_px,
                                        orig: piece.clone(),
                                        starts: starts.clone(),
                                        total: total_now,
                                    });
                                    cx.stop_propagation();
                                }),
                            )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            // The part of the piece being cut off stays as a gray area until the mouse is let go.
            if let Some(t) = self.trim.as_ref().filter(|t| t.index == i && i < t.starts.len()) {
                let was = t.starts[i];
                let (g0, g1) =
                    if t.left { (x_of(was), x0) } else { (x1, x_of(was + t.orig.span())) };
                video_lane = video_lane.child(
                    div()
                        .absolute()
                        .left(px(g0.min(g1)))
                        .w(px((g1 - g0).abs()))
                        .h_full()
                        .bg(rgb(theme::MUTED))
                        .opacity(0.8),
                );
            }
            let tiles =
                thumb_tiles(&self.sources[piece.source].thumbs, piece.a, piece.span(), x1 - x0);
            video_lane = video_lane.child(
                div()
                    .absolute()
                    .left(px(x0))
                    .w(px((x1 - x0 - 1.0).max(1.0)))
                    .h_full()
                    .overflow_hidden()
                    .bg(rgb(TINTS[i % 2]))
                    .border_2()
                    .border_color(rgb(if selected { theme::BRAND } else { TINTS[i % 2] }))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_sm()
                    .text_color(rgb(theme::FG))
                    .children(tiles.into_iter().map(|(x, image)| {
                        img(ImageSource::Render(image))
                            .absolute()
                            .left(px(x))
                            .top_0()
                            .w(px(THUMB_W))
                            .h_full()
                            .object_fit(ObjectFit::Cover)
                            .opacity(0.6)
                    }))
                    .child(model::piece_label(i))
                    .children(handles),
            );
        }

        let mut lanes = Vec::new();
        let shown = if self.tracks_open { tracks } else { tracks.min(1) };
        for t in 0..shown {
            let mut lane = div()
                .relative()
                .w_full()
                .h(px(LANE_HEIGHT))
                .rounded(px(theme::RADIUS_SM))
                .bg(rgb(theme::CARD))
                .overflow_hidden()
                .child({
                    let lanes = self.lanes.clone();
                    canvas(
                        move |b, _, _| {
                            let mut all = lanes.borrow_mut();
                            if all.len() <= t {
                                all.resize(t + 1, Bounds::default());
                            }
                            all[t] = b;
                        },
                        |_, (), _, _| {},
                    )
                    .absolute()
                    .size_full()
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.press_lane(t, ev, cx);
                        cx.stop_propagation();
                    }),
                );
            for (i, piece) in self.timeline.pieces.iter().enumerate() {
                let (x0, x1) = place(i);
                let w = (x1 - x0 - 1.0).max(1.0);
                let edit = &piece.tracks[t];
                let source = &self.sources[piece.source];
                let mut block = div()
                    .absolute()
                    .left(px(x0))
                    .w(px(w))
                    .h_full()
                    .overflow_hidden()
                    .bg(rgb(if self.selected == Some(i) { theme::ACCENT } else { theme::CARD }))
                    .opacity(if edit.mute { 0.35 } else { 1.0 });
                if let Some(Some(peaks)) = source.waves.get(t) {
                    let span = source
                        .info
                        .audio_spans
                        .get(t)
                        .copied()
                        .unwrap_or((Hns::ZERO, source.info.duration));
                    let columns =
                        model::column_peaks(peaks, span, piece.a, piece.b, (w / BAR_STEP) as usize);
                    let n = columns.len().max(1);
                    block = block.children(columns.into_iter().enumerate().map(|(c, peak)| {
                        let h = (peak.min(1.0).sqrt() * LANE_HEIGHT).max(1.0);
                        div()
                            .absolute()
                            .left(px((c as f32 + 0.5) / n as f32 * w))
                            .top(px((LANE_HEIGHT - h) / 2.0))
                            .w(px(2.))
                            .h(px(h))
                            .bg(rgb(if t == self.listen { theme::BRAND } else { theme::MUTED_FG }))
                    }));
                }
                // The volume line, drawn as thin steps (no path primitive needed), with a dot on
                // each point.
                let line = edit.line(piece.span());
                let px_of = |at: Hns| model::time_to_x(at, piece.span(), w);
                let y_of = |db: f64| model::db_to_frac(db) * LANE_HEIGHT;
                block = block.children(line.windows(2).flat_map(|seg| {
                    let (x0, x1) = (px_of(seg[0].at), px_of(seg[1].at));
                    let steps = ((x1 - x0) / 2.0).ceil().max(1.0) as usize;
                    let (d0, d1) = (seg[0].db, seg[1].db);
                    (0..steps).map(move |k| {
                        let f = (k as f64 + 0.5) / steps as f64;
                        let y = y_of(d0 + (d1 - d0) * f);
                        div()
                            .absolute()
                            .left(px(x0 + (x1 - x0) * k as f32 / steps as f32))
                            .top(px(y - 1.0))
                            .w(px(((x1 - x0) / steps as f32).max(1.0) + 0.5))
                            .h(px(2.))
                            .bg(rgb(theme::FG))
                    })
                }));
                if edit.has_line() {
                    block = block.children(line.iter().map(|pt| {
                        div()
                            .absolute()
                            .left(px(px_of(pt.at) - 4.0))
                            .top(px(y_of(pt.db) - 4.0))
                            .w(px(8.))
                            .h(px(8.))
                            .rounded_full()
                            .bg(rgb(theme::BRAND))
                    }));
                }
                if w >= 40.0 {
                    block = block.child(
                        div()
                            .id(("lane-mute", i * 16 + t))
                            .absolute()
                            .top(px(2.))
                            .left(px(2.))
                            .w(px(20.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(theme::RADIUS_SM))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::MUTED)))
                            // The strip scrubs on press; this button must not.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(icon(
                                if edit.mute { "volume-x" } else { "volume-2" },
                                theme::MUTED_FG,
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_track(i, t, cx, |e| e.mute = !e.mute);
                            })),
                    );
                }
                lane = lane.child(block);
            }
            if let Some((_, i, a, b)) = self.range.filter(|r| r.0 == t && r.2 != r.3)
                && i < self.timeline.pieces.len()
            {
                let start = self.timeline.start_of(i);
                let (x0, x1) = (x_of(start + a.min(b)), x_of(start + a.max(b)));
                lane = lane.child(
                    div()
                        .absolute()
                        .left(px(x0))
                        .w(px((x1 - x0).max(1.0)))
                        .h_full()
                        .bg(rgb(theme::BRAND))
                        .opacity(0.3),
                );
            }
            lanes.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(rgb(theme::MUTED_FG))
                            .child(icon("volume-2", theme::MUTED_FG))
                            .child(
                                self.sources
                                    .first()
                                    .map_or_else(|| track_name(t), |s| track_label(s, t)),
                            )
                            .children((t == 0).then(|| {
                                muted(if self.tracks_open {
                                    "the whole mix"
                                } else {
                                    "the whole mix, plays by default"
                                })
                            })),
                    )
                    .child(lane),
            );
        }

        let playhead = div()
            .absolute()
            .top_0()
            .h_full()
            .left(px(x_of(self.player.playhead) - 1.0))
            .w(px(2.))
            .bg(rgb(theme::FG));

        let strip = div()
            .id("editor-strip")
            .relative()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .cursor_pointer()
            .child(video_lane)
            .children(lanes)
            .child(playhead)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.scrubbing = true;
                    let t = this.time_at(ev.position.x);
                    this.seek(t, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.drag.is_some() && ev.dragging() {
                    this.drag_point(ev);
                    cx.notify();
                } else if this.trim.is_some() && ev.dragging() {
                    this.drag_edge(ev);
                    cx.notify();
                } else if this.ranging && ev.dragging() {
                    if let Some(i) = this.range.map(|r| r.1) {
                        let at = this.piece_time(i, ev.position.x);
                        if let Some(r) = this.range.as_mut() {
                            r.3 = at;
                        }
                    }
                    cx.notify();
                } else if this.scrubbing && ev.dragging() {
                    let t = this.time_at(ev.position.x);
                    this.seek(t, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.release()))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.release()));

        let toggle = (tracks > 1).then(|| {
            button_ghost(
                "tracks-toggle",
                if self.tracks_open {
                    "Hide the other tracks".to_owned()
                } else {
                    format!("Edit {} tracks separately", tracks - 1)
                },
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.tracks_open = !this.tracks_open;
                cx.notify();
            }))
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Timeline"))
                    .child(div().flex_1())
                    .children(toggle),
            )
            .child(head)
            .child(strip)
    }

    /// The selected piece: where it comes from, order, and each audio track's volume and mute.
    fn inspector(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.selected?;
        let piece = self.timeline.pieces.get(i)?;
        let source = &self.sources[piece.source];
        let mut list = card(format!("Piece {}", model::piece_label(i)))
            .child(muted(format!(
                "{}, {} to {} of the clip, {} long.",
                source.name,
                model::secs_label(piece.a),
                model::secs_label(piece.b),
                model::secs_label(piece.span())
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button_ghost("piece-left", "Move earlier")
                            .child(icon("chevron-left", theme::MUTED_FG))
                            .on_click(cx.listener(|this, _, _, cx| this.shift_selected(-1, cx))),
                    )
                    .child(
                        button_ghost("piece-right", "Move later")
                            .child(icon("chevron-right", theme::MUTED_FG))
                            .on_click(cx.listener(|this, _, _, cx| this.shift_selected(1, cx))),
                    )
                    .child(
                        button_ghost("piece-join", "Join with next")
                            .on_click(cx.listener(|this, _, _, cx| this.join_selected(cx))),
                    )
                    .child(
                        button_ghost("undo", "Undo")
                            .on_click(cx.listener(|this, _, _, cx| this.step_history(false, cx))),
                    )
                    .child(
                        button_ghost("redo", "Redo")
                            .on_click(cx.listener(|this, _, _, cx| this.step_history(true, cx))),
                    ),
            );
        let shown = if self.tracks_open { piece.tracks.len() } else { piece.tracks.len().min(1) };
        for (t, edit) in piece.tracks.iter().enumerate().take(shown) {
            let listening = self.listen == t;
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(84.)).truncate().text_sm().child(track_label(source, t)))
                    .child(number_stepper(
                        format!("gain-{t}"),
                        edit.gain_db,
                        Stepper {
                            min: -30.0,
                            max: 12.0,
                            step: 1.0,
                            suffix: " dB",
                            enabled: !edit.mute,
                        },
                        cx.listener(move |this, v: &f64, _, cx| {
                            let v = *v;
                            this.edit_track(i, t, cx, |e| e.gain_db = v);
                        }),
                    ))
                    .child(labeled_toggle(
                        ("mute", t),
                        "Mute this piece",
                        edit.mute,
                        true,
                        cx.listener(move |this, on: &bool, _, cx| {
                            let on = *on;
                            this.edit_track(i, t, cx, |e| e.mute = on);
                        }),
                    ))
                    .child(div().flex_1())
                    .child(if listening {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .text_color(rgb(theme::BRAND))
                            .child(icon("headphones", theme::BRAND))
                            .child("Listening")
                            .into_any_element()
                    } else {
                        button_ghost(("listen", t), "Listen")
                            .child(icon("headphones", theme::MUTED_FG))
                            .on_click(cx.listener(move |this, _, _, cx| this.set_listen(t, cx)))
                            .into_any_element()
                    }),
            );
        }
        Some(
            list.child(muted(
                "Master is the whole mix and plays by default. Open the other tracks to edit each one alone. Muting here only silences this piece.",
            ))
            .into_any_element(),
        )
    }

    /// Done, the "what now?" choice, and how the last save went.
    fn finish_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let running = matches!(self.export, Export::Running(_));
        let mut done = button_primary("editor-done", if running { "Working" } else { "Done" });
        done = if running {
            done.opacity(0.5)
        } else {
            done.on_click(cx.listener(|this, _, _, cx| this.done(cx)))
        };
        let mut out =
            card("Finish").child(div().flex().flex_row().items_center().gap_2().child(done));
        if self.finish != FinishChoice::Ask {
            out = out.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(muted(if self.finish == FinishChoice::SaveAs {
                        "Done saves one new file."
                    } else {
                        "Done saves each piece as its own file."
                    }))
                    .child(button_ghost("editor-ask", "Ask me each time").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.finish = FinishChoice::Ask;
                            this.remember_prefs(cx, |e| e.finish = FinishChoice::Ask);
                            cx.notify();
                        },
                    ))),
            );
        }
        if self.finishing {
            out = out
                .child(muted("What do you want to do with this?"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_2()
                        .child(button_primary("choose-save", "Save as a new file").on_click(
                            cx.listener(|this, _, _, cx| this.choose(FinishChoice::SaveAs, cx)),
                        ))
                        .child(button("choose-extract", "Extract the pieces, keep editing").on_click(
                            cx.listener(|this, _, _, cx| this.choose(FinishChoice::Extract, cx)),
                        ))
                        .child(button_ghost("choose-cancel", "Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.finishing = false;
                                cx.notify();
                            },
                        ))),
                )
                .child(muted(
                    "Save as joins the pieces in order into one new file. Extract saves every piece as its own file and leaves the timeline as it is.",
                ))
                .child(labeled_toggle(
                    "remember-choice",
                    "Remember my choice",
                    self.remember,
                    true,
                    cx.listener(|this, on: &bool, _, cx| {
                        this.remember = *on;
                        cx.notify();
                    }),
                ));
        }
        let status: Option<AnyElement> = match &self.export {
            Export::Idle => None,
            Export::Running(label) => Some(
                muted(format!("{label} Long clips take a moment when audio is edited."))
                    .into_any_element(),
            ),
            Export::Done { files, took } => Some(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(rgb(theme::BRAND))
                    .child(format!(
                        "Saved {} in {:.1} s.",
                        if files.len() == 1 {
                            files[0]
                                .file_name()
                                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                        } else {
                            format!("{} clips", files.len())
                        },
                        took.as_secs_f64()
                    ))
                    .child(button_ghost("editor-show", "Show in folder").on_click({
                        let file = files.first().cloned();
                        cx.listener(move |_, _, _, _| {
                            if let Some(file) = &file {
                                let _ = std::process::Command::new("explorer")
                                    .arg("/select,")
                                    .arg(file)
                                    .spawn();
                            }
                        })
                    }))
                    .into_any_element(),
            ),
            Export::Failed(error) => Some(
                div()
                    .text_sm()
                    .text_color(rgb(theme::DANGER))
                    .child(error.clone())
                    .into_any_element(),
            ),
        };
        out.children(status)
    }
}
