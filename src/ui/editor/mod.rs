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
pub(crate) mod model;
pub mod player;
mod thumb_cache;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    AnyElement, Bounds, Context, ExternalPaths, FocusHandle, FontWeight, ImageSource,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, ParentElement, Pixels, Render, RenderImage, Role, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window, canvas, div,
    img, px, relative, rgb, rgba, transparent_black,
};
use zc_core::Hns;
use zc_core::settings::{EditorSettings, FinishChoice, PlayerSettings};
use zc_editor::concat::StreamInfo;
use zc_editor::ffmpeg::{AudioEdit, Ffmpeg, FfmpegError, ProbeInfo};
use zc_editor::waveform::{file_key, rms_peaks};
use zc_store::Store;

use self::model::{Piece, PiecePlan, Timeline, TimelineTool, TrackEdit};
use self::player::{
    Item, Player, Sound, clamp_volume, pause_on_focus_loss, step_volume, volume_gain, volume_icon,
};
use self::thumb_cache::{ThumbCache, ThumbKey};
use super::components::{
    button::{button, button_danger, button_ghost, button_primary, button_primary_if},
    card::{card, kbd},
    labeled_toggle::labeled_toggle,
    number_stepper::{Stepper, number_stepper},
    slider::{SliderState, slider},
    status_dot::status_dot,
};
use super::icons::icon;
use super::{human_duration, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

/// Peaks stored per waveform, and the sample rate they are computed from (P5.10).
const BUCKETS: usize = 1_000;
const SAMPLE_RATE: u32 = 8_000;
/// Bar width before the first paint has measured the real one.
const FALLBACK_WIDTH: f32 = 720.0;
const HEAD_HEIGHT: f32 = 32.0;
const VIDEO_LANE_HEIGHT: f32 = 48.0;
const LANE_HEIGHT: f32 = 40.0;
/// Width of each trim-edge grab handle on the selected piece.
const HANDLE_W: f32 = 12.0;
/// Size of one thumbnail in the video lane (16:9 at the lane's inner height).
const THUMB_H: f32 = 40.0;
const THUMB_W: f32 = 72.0;
/// One waveform bar is at least this far from the previous one.
const BAR_STEP: f32 = 3.0;
/// The picture's height is this share of the window's, within these limits.
const VIDEO_SHARE: f32 = 0.46;
const VIDEO_MIN: f32 = 180.0;
const VIDEO_MAX: f32 = 760.0;
/// The slim inspector beside the picture, and the window width from which it sits beside it.
const SIDE_WIDTH: f32 = 320.0;
const WIDE: f32 = 1_020.0;
/// Below `WIDE`, inspector and export sit side-by-side under the timeline from this width up.
const SPLIT_BELOW: f32 = 680.0;
/// How often the picture is refreshed while playing.
const TICK: Duration = Duration::from_millis(16);
/// Pieces are tinted in turn so neighbours read as separate.
const TINTS: [u32; 2] = [theme::BRAND_SOFT, theme::PIECE_ALT];

/// How far (px) a press on a piece must move before it becomes a reorder drag, so a click still
/// just selects.
const MOVE_THRESHOLD: f32 = 4.0;
/// A picked range's ends snap to a piece boundary within this many px.
const SNAP_PX: f32 = 6.0;
/// What Up and Down change the preview volume by, in percentage points.
const VOLUME_STEP: i32 = 5;
/// How long after the last volume change the setting is written to disk.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(200);

/// A clip on the timeline's list of sources.
struct Source {
    path: PathBuf,
    name: String,
    info: ProbeInfo,
    /// One per audio track; `None` until computed, or if the track has no usable audio.
    waves: Vec<Option<Vec<f32>>>,
    /// Moments marked while the clip was recorded.
    bookmarks: Vec<Hns>,
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
    /// The remembered choice for Done.
    finish: FinishChoice,
    /// Soloed audio track index while editing tracks separately (`None` = play mixed tracks `1..`).
    solo: Option<usize>,
    /// Every audio track is shown and editable; otherwise only Master.
    tracks_open: bool,
    /// Active timeline pointer tool (`V` = Select, `S` = Split).
    tool: TimelineTool,
    /// Bounded LRU cache of timeline thumbnails keyed by `(source path, quantized timestamp)`.
    thumb_cache: ThumbCache<Arc<RenderImage>>,
    /// Keys currently being decoded on a background thread so we never spawn duplicates.
    thumb_in_flight: HashSet<ThumbKey>,
    focus: Option<FocusHandle>,
    /// Subscription pausing playback when the OS window loses focus or is minimised.
    window_activation_sub: Option<gpui::Subscription>,
    /// Last observed OS window activation state.
    window_active: bool,
    /// Remembers whether playback was active when the window lost focus (never auto-resumes).
    paused_on_blur: bool,
    /// The page should take keyboard focus on its next draw (it was just shown). Otherwise it only
    /// does when nothing at all has focus, so it never pulls it from something else on every draw.
    focus_pending: bool,
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
    /// A range of the whole timeline picked with Alt+drag (timeline time, ends in either order
    /// while dragging), and the fixed edge while one of its ends is being dragged.
    sel: Option<(Hns, Hns)>,
    sel_anchor: Option<Hns>,
    /// The mouse over the timeline (window x), for the Split tool's razor line.
    hover: Option<Pixels>,
    /// Earlier and undone states of the pieces.
    undo: Vec<Vec<Piece>>,
    redo: Vec<Vec<Piece>>,
    /// A piece being pressed or dragged to a new place in the order.
    moving: Option<MoveDrag>,
    /// The preview's volume (`0..=100`), mute, and the level Unmute returns to. Preview only:
    /// the export never sees them.
    volume: u8,
    muted: bool,
    last_volume: u8,
    volume_slider: SliderState,
    /// Whether the saved volume was read from the app yet, and whether a change still has to be
    /// written to disk (writes are debounced while the slider moves).
    prefs_loaded: bool,
    prefs_dirty: bool,
    persist_gen: u64,
}

/// A piece pressed in Select mode. It turns into a drag once the mouse has moved
/// [`MOVE_THRESHOLD`] px; until then it is only a click.
struct MoveDrag {
    from: usize,
    /// Where the mouse went down, window x.
    x0: f32,
    active: bool,
    /// How far the mouse has moved since, px.
    dx: f32,
    /// The index the piece takes if dropped now.
    target: usize,
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
            sel: None,
            sel_anchor: None,
            hover: None,
            undo: Vec::new(),
            redo: Vec::new(),
            moving: None,
            volume: PlayerSettings::default().volume,
            muted: false,
            last_volume: 100,
            volume_slider: SliderState::default(),
            prefs_loaded: false,
            prefs_dirty: false,
            persist_gen: 0,
            player: Player::default(),
            ticking: false,
            picking: false,
            export: Export::Idle,
            finishing: false,
            confirm_close: false,
            trim: None,
            remember: false,
            finish: FinishChoice::Ask,
            solo: None,
            tracks_open: false,
            tool: TimelineTool::Select,
            thumb_cache: ThumbCache::default(),
            thumb_in_flight: HashSet::new(),
            focus: None,
            window_activation_sub: None,
            window_active: true,
            paused_on_blur: false,
            focus_pending: true,
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
        if let Some((finish, _listen)) = prefs {
            self.finish = finish;
        }
        self.add_clip(path, cx);
    }

    /// Empties the project: no clips, no history, no picture. Keeps the LRU thumbnail cache warm
    /// so reopening a clip reuses previously decoded frames.
    fn reset(&mut self) {
        self.generation += 1;
        self.player.pause();
        self.sources.clear();
        self.timeline = Timeline::default();
        self.undo.clear();
        self.redo.clear();
        self.drag = None;
        self.moving = None;
        self.selected = None;
        self.loading.clear();
        self.problem = None;
        self.scrubbing = false;
        self.player.reset();
        self.picking = false;
        self.finishing = false;
        self.confirm_close = false;
        self.range = None;
        self.sel = None;
        self.sel_anchor = None;
        self.hover = None;
        self.export = Export::Idle;
        self.solo = None;
        self.tool = TimelineTool::Select;
        self.paused_on_blur = false;
        self.thumb_in_flight.clear();
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
        // The entry of this clip, not just the last one: reads finish in any order.
        let read_name =
            path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if let Some(at) = self.loading.iter().position(|n| *n == read_name) {
            self.loading.remove(at);
        }
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
                // Anything that still points at a track that was just dropped would index past it.
                if self.range.is_some_and(|r| r.0 >= tracks)
                    || self.drag.is_some_and(|d| d.1 >= tracks)
                {
                    self.range = None;
                    self.drag = None;
                    self.ranging = false;
                }
                if self.solo.is_some_and(|s| s >= tracks) {
                    self.solo = None;
                }
                self.timeline.pieces.push(Piece {
                    source: index,
                    a: Hns::ZERO,
                    b: info.duration,
                    tracks: vec![TrackEdit::default(); tracks],
                    gap: false,
                });
                self.sources.push(Source {
                    bookmarks: zc_core::bookmarks::load(&path),
                    path,
                    name,
                    info,
                    waves: vec![None; own],
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

    /// Seeds the LRU thumbnail cache along a newly added clip, skipping timestamps already cached
    /// or decoding.
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
        let keys: Vec<ThumbKey> = (0..count)
            .map(|k| {
                // Mid-slot, so the first and last pictures sit inside the clip.
                let at =
                    Hns::from_secs_f64(duration.as_secs_f64() * (k as f64 + 0.5) / count as f64);
                ThumbKey::new(path.clone(), at)
            })
            .collect();
        self.queue_thumb_keys(generation, ffmpeg.clone(), keys, cx);
    }

    /// Decodes any uncached `keys` one at a time in the background and inserts them into the
    /// bounded LRU [`Self::thumb_cache`]. Evicted textures are pushed to [`Player::spent`] for GPU
    /// cleanup on the next render.
    fn queue_thumb_keys(
        &mut self,
        generation: u64,
        ffmpeg: Ffmpeg,
        keys: Vec<ThumbKey>,
        cx: &mut Context<Self>,
    ) {
        let mut pending = Vec::new();
        for key in keys {
            if self.thumb_cache.get(&key).is_some() {
                continue;
            }
            if self.thumb_in_flight.insert(key.clone()) {
                pending.push(key);
            }
        }
        if pending.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for key in pending {
                let (ffmpeg_c, path, at) = (ffmpeg.clone(), key.path.clone(), key.timestamp());
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        player::still(&ffmpeg_c, &path, at, THUMB_W as u32, THUMB_H as u32)
                    })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    this.thumb_in_flight.remove(&key);
                    if let Some(image) = image {
                        if let Some(evicted) = this.thumb_cache.insert(key, image) {
                            this.player.spent.push(evicted);
                        }
                        cx.notify();
                    }
                    generation == this.generation
                });
                if !alive.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Touches visible cached thumbnails so they stay hot in the LRU, and queues background
    /// decodes for any missing quantized slots (e.g. after zooming in via trim or split) while
    /// keeping existing cached frames visible so the timeline never flashes blank.
    fn ensure_visible_thumbs(&mut self, cx: &mut Context<Self>) {
        if self.trim.is_some() || self.timeline.pieces.is_empty() {
            return;
        }
        let Some(ffmpeg) = self.ffmpeg.clone() else { return };
        let width = if self.drawn_width > 0.0 { self.drawn_width } else { FALLBACK_WIDTH };
        let total = self.disp_total();
        let xd = |d: Hns| model::time_to_x(d, total, width);
        let mut missing = Vec::new();
        for (i, piece) in self.timeline.pieces.iter().enumerate() {
            if piece.gap {
                continue;
            }
            let Some(source) = self.sources.get(piece.source) else { continue };
            let start = self.to_disp(self.timeline.start_of(i));
            let piece_w = (xd(start + piece.span()) - xd(start)).max(0.0);
            for (_, target) in desired_thumb_times(piece.a, piece.span(), piece_w) {
                let key = ThumbKey::new(source.path.clone(), target);
                if self.thumb_cache.get(&key).is_none()
                    && !self.thumb_in_flight.contains(&key)
                    && missing.len() < 24
                {
                    missing.push(key);
                }
            }
        }
        if !missing.is_empty() {
            self.queue_thumb_keys(self.generation, ffmpeg, missing, cx);
        }
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
    /// How much room the selected piece's trimmed-off parts get on the timeline (left, right):
    /// they show as gray around it, so trimming doesn't zoom the picture in. Capped, so a short
    /// piece of a long recording isn't a sliver. None while an edge is being dragged.
    fn ghost(&self) -> (Hns, Hns) {
        let Some(piece) = self.selected.filter(|_| self.trim.is_none()) else {
            return (Hns::ZERO, Hns::ZERO);
        };
        let Some(piece) = self.timeline.pieces.get(piece).filter(|p| !p.gap) else {
            return (Hns::ZERO, Hns::ZERO);
        };
        let Some(source) = self.sources.get(piece.source) else { return (Hns::ZERO, Hns::ZERO) };
        let cap = Hns::from_secs_f64((piece.span().as_secs_f64() * 4.0).max(10.0));
        let side = |cut: Hns| {
            if cut < Hns::from_millis(200) { Hns::ZERO } else { cut.min(cap) }
        };
        (side(piece.a), side((source.info.duration - piece.b).max(Hns::ZERO)))
    }

    /// Timeline time to the time on the drawn scale, which has the gray room added.
    fn to_disp(&self, t: Hns) -> Hns {
        let (gl, gr) = self.ghost();
        let Some(i) = self.selected.filter(|i| *i < self.timeline.pieces.len()) else { return t };
        let s = self.timeline.start_of(i);
        let e = s + self.timeline.pieces[i].span();
        if t < s {
            t
        } else if t < e {
            t + gl
        } else {
            t + gl + gr
        }
    }

    /// The drawn scale's length.
    fn disp_total(&self) -> Hns {
        let (gl, gr) = self.ghost();
        self.timeline.total() + gl + gr
    }

    /// The reverse of `to_disp`; a spot in the gray room is the piece's nearest edge.
    fn undisp(&self, d: Hns) -> Hns {
        let (gl, gr) = self.ghost();
        let Some(i) = self.selected.filter(|i| *i < self.timeline.pieces.len()) else { return d };
        let s = self.timeline.start_of(i);
        let e = s + self.timeline.pieces[i].span();
        if d < s {
            d
        } else if d < s + gl {
            s
        } else if d < e + gl {
            d - gl
        } else if d < e + gl + gr {
            e
        } else {
            d - gl - gr
        }
    }

    fn time_at(&self, x: Pixels) -> Hns {
        let bounds = self.strip.get();
        let width = f32::from(bounds.size.width);
        self.undisp(model::x_to_time(f32::from(x - bounds.origin.x), self.disp_total(), width))
    }

    /// Pauses and shows the frame at timeline time `t`.
    fn seek(&mut self, t: Hns, cx: &mut Context<Self>) {
        let t = t.min(self.timeline.total());
        let Some((i, src)) = self.timeline.locate(t) else { return };
        let Some(ffmpeg) = self.ffmpeg.as_ref() else { return };
        let piece = &self.timeline.pieces[i];
        if piece.gap {
            self.player.show_black(t);
        } else {
            self.player.show_at(ffmpeg, &self.sources[piece.source].path, src, t);
        }
        self.selected = Some(i);
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Stops playback, picture and sound. Called when the user leaves the Editor tab.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        if self.player.playing {
            self.player.pause();
            cx.notify();
        }
    }

    /// Pauses when the OS window loses focus or is minimised, remembering that playback was
    /// running; never auto-resumes when focus returns.
    fn handle_window_activation(&mut self, active: bool, cx: &mut Context<Self>) {
        self.window_active = active;
        if pause_on_focus_loss(self.player.playing, active, &mut self.paused_on_blur) {
            self.player.pause();
            cx.notify();
        }
    }

    /// Starts playback from timeline position `at`.
    fn play_from(&mut self, at: Hns, cx: &mut Context<Self>) {
        let Some((first, src)) = self.timeline.locate(at) else { return };
        let Some(ffmpeg) = self.ffmpeg.as_ref() else { return };
        self.paused_on_blur = false;
        let items: Vec<Item> = self
            .timeline
            .pieces
            .iter()
            .enumerate()
            .skip(first)
            .map(|(n, piece)| {
                let from = if n == first { src } else { piece.a };
                if piece.gap {
                    return Item::gap(from, piece.b);
                }
                let (sounds, solo) =
                    build_piece_sounds(&piece.tracks, from - piece.a, self.tracks_open, self.solo);
                Item {
                    path: self.sources[piece.source].path.clone(),
                    from,
                    to: piece.b,
                    sounds,
                    solo,
                }
            })
            .collect();
        self.player.play(ffmpeg, items, at);
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Restarts playback at the current playhead if currently playing, so audio edits take effect
    /// immediately.
    fn restart_if_playing(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.play_from(self.player.playhead, cx);
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
        self.play_from(at, cx);
    }

    /// Stops playback, shuts down preview audio, and seeks the playhead to the start of the
    /// timeline.
    fn stop(&mut self, cx: &mut Context<Self>) {
        let ready = self.ffmpeg.is_some() && !self.timeline.pieces.is_empty();
        if !can_stop(ready, self.player.playing, self.player.playhead) {
            return;
        }
        self.paused_on_blur = false;
        self.scrubbing = false;
        self.player.pause();
        if self.timeline.locate(Hns::ZERO).is_some() {
            self.seek(Hns::ZERO, cx);
        } else {
            self.player.playhead = Hns::ZERO;
            cx.notify();
        }
    }

    /// Starts the task that refreshes the picture while the player has work, unless one runs.
    fn ensure_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking || !self.player.busy() {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep = this
                    .update(cx, |this, cx| {
                        if !this.player.busy() {
                            this.ticking = false;
                            return false;
                        }
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

    /// Whether a split at timeline time `t` would work (see [`Timeline::can_split_at`]).
    fn can_split(&self, t: Hns) -> bool {
        self.timeline
            .locate(t)
            .and_then(|(i, _)| self.sources.get(self.timeline.pieces[i].source))
            .is_some_and(|s| self.timeline.can_split_at(t, &s.info.keyframes))
    }

    /// A click with the Split tool: moves the playhead there and cuts, unless the razor line was
    /// showing as blocked.
    fn click_split(&mut self, t: Hns, cx: &mut Context<Self>) {
        self.seek(t, cx);
        if self.can_split(t) {
            self.split_at(t, cx);
        }
    }

    fn release(&mut self, cx: &mut Context<Self>) {
        if self.sel_anchor.take().is_some() {
            let total = self.timeline.total();
            self.sel = self.sel.and_then(|(a, b)| model::clamp_range(a, b, total));
        }
        if let Some(m) = self.moving.take()
            && m.active
            && m.target != m.from
        {
            self.commit_move(m.from, m.target, cx);
        }
        let edited_audio = self.drag.is_some();
        self.trim = None;
        self.scrubbing = false;
        self.drag = None;
        self.ranging = false;
        // A click, not a drag: no range.
        if self.range.is_some_and(|(_, _, a, b)| b.max(a) - b.min(a) < Hns::from_millis(100)) {
            self.range = None;
        }
        if edited_audio {
            self.restart_if_playing(cx);
        }
    }

    /// Silences the picked stretch of its track.
    fn silence_range(&mut self, cx: &mut Context<Self>) {
        let Some((t, i, a, b)) = self.range.take() else { return };
        let Some(piece) = self.timeline.pieces.get(i) else { return };
        let span = piece.span();
        if t >= piece.tracks.len() {
            return;
        }
        self.checkpoint();
        self.timeline.pieces[i].tracks[t].silence(a.min(b), a.max(b), span);
        // Playback restarts at the playhead so the new edit is heard.
        let at = self.player.playhead;
        if self.player.playing {
            self.play_from(at, cx);
        } else {
            self.seek(at, cx);
        }
    }

    /// Where, in piece `i`, timeline x `x` falls, as time from the piece's start.
    fn piece_time(&self, i: usize, x: Pixels) -> Hns {
        let t = self.time_at(x) - self.timeline.start_of(i);
        t.clamp(Hns::ZERO, self.timeline.pieces[i].span())
    }

    /// A press on audio lane `t`: in Split mode splits at the clicked time; in Select mode grabs a
    /// volume point (Shift removes it), starts a new one on the line, or selects/scrubs.
    fn press_lane(&mut self, t: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let here = self.time_at(ev.position.x);
        if ev.modifiers.alt {
            let at = self.snapped_time_at(ev.position.x);
            self.begin_range(at);
            cx.notify();
            return;
        }
        self.sel = None;
        if self.tool == TimelineTool::Split {
            self.click_split(here, cx);
            return;
        }
        const GRAB: f32 = 9.0;
        let lane = self.lanes.borrow().get(t).copied();
        let (Some(lane), Some((i, _))) = (lane, self.timeline.locate(here)) else {
            self.scrubbing = true;
            self.seek(here, cx);
            return;
        };
        if self.timeline.pieces[i].gap {
            // A gap has no sound to edit: the press just moves the playhead there.
            self.scrubbing = true;
            self.seek(here, cx);
            return;
        }
        let span = self.timeline.pieces[i].span();
        let bounds = self.strip.get();
        let total = self.disp_total();
        let start = self.to_disp(self.timeline.start_of(i));
        let to_x = |at: Hns| model::time_to_x(start + at, total, f32::from(bounds.size.width));
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

    /// Where piece `i` is drawn on the timeline (x of its start and end), when no edge is being
    /// dragged.
    fn piece_span_x(&self, i: usize) -> (f32, f32) {
        let width = f32::from(self.strip.get().size.width).max(1.0);
        let total = self.disp_total();
        let start = self.to_disp(self.timeline.start_of(i));
        let end = start + self.timeline.pieces[i].span();
        (model::time_to_x(start, total, width), model::time_to_x(end, total, width))
    }

    /// Follows the mouse while a pressed piece is dragged: once past the threshold, works out
    /// where it would land.
    fn drag_move(&mut self, ev: &MouseMoveEvent) {
        let Some(m) = self.moving.as_ref() else { return };
        let dx = f32::from(ev.position.x) - m.x0;
        if !m.active && dx.abs() < MOVE_THRESHOLD {
            return;
        }
        let from = m.from;
        if from >= self.timeline.pieces.len() {
            self.moving = None;
            return;
        }
        let mids: Vec<f32> = (0..self.timeline.pieces.len())
            .map(|i| {
                let (x0, x1) = self.piece_span_x(i);
                (x0 + x1) / 2.0
            })
            .collect();
        let target = model::move_target(&mids, from, mids[from] + dx);
        if let Some(m) = self.moving.as_mut() {
            m.active = true;
            m.dx = dx;
            m.target = target;
        }
    }

    /// Drops piece `from` at index `to`. One undo step.
    fn commit_move(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.checkpoint();
        let Some(j) = self.timeline.move_piece(from, to) else {
            self.undo.pop();
            return;
        };
        self.problem = None;
        self.range = None;
        let at = self.timeline.start_of(j);
        self.seek(at, cx);
        cx.notify();
    }

    /// Puts a black, silent gap after the selected piece, or at the boundary nearest the
    /// playhead when nothing is selected.
    fn add_gap(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self
            .selected
            .and_then(|i| self.timeline.pieces.get(i))
            .or_else(|| self.timeline.pieces.first())
            .map(|p| p.source)
        else {
            return;
        };
        let tracks = self.timeline.pieces.first().map_or(0, |p| p.tracks.len());
        let at = match self.selected.filter(|&i| i < self.timeline.pieces.len()) {
            Some(i) => i + 1,
            None => self.timeline.boundary_near(self.player.playhead),
        };
        self.checkpoint();
        let j = self.timeline.insert_gap(at, model::DEFAULT_GAP, source, tracks);
        self.problem = None;
        let start = self.timeline.start_of(j);
        self.seek(start, cx);
        cx.notify();
    }

    /// How much time [`SNAP_PX`] px of the timeline stand for.
    fn snap_tol(&self) -> Hns {
        let width = f64::from(f32::from(self.strip.get().size.width).max(1.0));
        Hns::from_ticks((self.disp_total().ticks() as f64 / width * f64::from(SNAP_PX)) as i64)
    }

    /// Timeline time under window x `x`, pulled onto a piece boundary when close to one.
    fn snapped_time_at(&self, x: Pixels) -> Hns {
        self.timeline.snap_time(self.time_at(x), self.snap_tol())
    }

    /// Starts picking a range at window x `x` (Alt+drag, or an edge of the range: `anchor` is then
    /// the fixed other edge).
    fn begin_range(&mut self, anchor: Hns) {
        self.sel = Some((anchor, anchor));
        self.sel_anchor = Some(anchor);
    }

    /// Moves the free edge of the range being picked to the mouse.
    fn drag_range(&mut self, x: Pixels) {
        if let Some(anchor) = self.sel_anchor {
            self.sel = Some((anchor, self.snapped_time_at(x)));
        }
    }

    /// Cuts the picked range out and closes the timeline up. One undo step.
    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        self.edit_selection(cx, false);
    }

    /// Replaces the picked range with a black gap of the same length. One undo step.
    fn gap_selection(&mut self, cx: &mut Context<Self>) {
        self.edit_selection(cx, true);
    }

    fn edit_selection(&mut self, cx: &mut Context<Self>, make_gap: bool) {
        self.sel_anchor = None;
        let Some((a, b)) = self.sel.take() else { return };
        self.checkpoint();
        let done =
            if make_gap { self.timeline.gap_range(a, b) } else { self.timeline.cut_range(a, b) };
        let Some((start, _)) = done else {
            self.undo.pop();
            return;
        };
        self.problem = None;
        self.range = None;
        self.selected = self.timeline.locate(start).map(|l| l.0);
        self.seek(start, cx);
        cx.notify();
    }

    /// Hydrates the saved preview volume once. Not from `open`, which runs while the app is
    /// borrowed; the first render is safe.
    fn load_volume_prefs(&mut self, cx: &mut Context<Self>) {
        if self.prefs_loaded {
            return;
        }
        let Some(app) = self.app.upgrade() else { return };
        let prefs = app.read(cx).settings.editor_volume;
        self.prefs_loaded = true;
        self.volume = clamp_volume(prefs.volume);
        self.last_volume = self.volume.max(1);
        self.muted = prefs.muted;
        self.apply_gain();
    }

    /// Hands the preview its master gain; the mixer picks it up on its next chunk, so it is
    /// heard at once without restarting playback.
    fn apply_gain(&self) {
        self.player.set_gain(volume_gain(self.volume, self.muted));
    }

    /// Copies the volume into the app's settings and draft (so a later Settings save keeps it)
    /// and, when `write_disk`, saves `settings.toml`.
    fn sync_volume_prefs(&mut self, write_disk: bool, cx: &mut Context<Self>) {
        let prefs = PlayerSettings { volume: self.volume, muted: self.muted };
        let dirty = self.prefs_dirty;
        let _ = self.app.update(cx, |app, _| {
            let changed = app.settings.editor_volume != prefs;
            app.settings.editor_volume = prefs;
            app.draft.settings.editor_volume = prefs;
            if write_disk
                && (changed || dirty)
                && let Err(e) = crate::settings_store::save(&app.paths.settings_file, &app.settings)
            {
                tracing::warn!(error = %e, "could not save the editor volume");
            }
        });
        self.prefs_dirty = !write_disk;
    }

    /// Saves the volume to disk shortly after the last change, so a slider drag or a held key
    /// writes once.
    fn schedule_volume_save(&mut self, cx: &mut Context<Self>) {
        self.sync_volume_prefs(false, cx);
        self.persist_gen = self.persist_gen.wrapping_add(1);
        let save_gen = self.persist_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PERSIST_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.persist_gen == save_gen && this.prefs_dirty {
                    this.sync_volume_prefs(true, cx);
                }
            });
        })
        .detach();
    }

    /// Sets the volume from the slider. Raising it above 0 unmutes.
    fn set_volume(&mut self, percent: u8, cx: &mut Context<Self>) {
        let pct = clamp_volume(percent);
        if pct > 0 {
            self.muted = false;
            self.last_volume = pct;
        }
        self.volume = pct;
        self.apply_gain();
        self.schedule_volume_save(cx);
        cx.notify();
    }

    /// `M` and the speaker button. Unmuting from a volume dragged to 0 returns to the last
    /// audible level, so the button always makes sound.
    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if self.muted || self.volume == 0 {
            self.muted = false;
            if self.volume == 0 {
                self.volume = self.last_volume.clamp(1, 100);
            }
        } else {
            self.muted = true;
        }
        self.apply_gain();
        self.persist_gen = self.persist_gen.wrapping_add(1);
        self.sync_volume_prefs(true, cx);
        cx.notify();
    }

    /// Up and Down: steps the volume. Raising it unmutes.
    fn adjust_volume(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = step_volume(self.volume, delta);
        if delta > 0 {
            self.muted = false;
        }
        if next > 0 {
            self.last_volume = next;
        }
        self.volume = next;
        self.apply_gain();
        self.schedule_volume_save(cx);
        cx.notify();
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
        self.sel = None;
        let at = self.player.playhead.min(self.timeline.total());
        self.selected = self.timeline.locate(at).map(|l| l.0);
        self.seek(at, cx);
        cx.notify();
    }

    /// Maximum gap currently accepted when joining pieces at the current timeline zoom.
    fn current_join_limit(&self) -> Hns {
        let width = if self.drawn_width > 0.0 { self.drawn_width } else { FALLBACK_WIDTH };
        model::join_gap_limit(self.disp_total(), width)
    }

    /// Joins piece `i` and `i + 1`. Shared by the `J` shortcut and the seam join button.
    fn join_at(&mut self, i: usize, cx: &mut Context<Self>) {
        let max_gap = self.current_join_limit();
        self.checkpoint();
        match self.timeline.join_within(i, max_gap) {
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

    /// Joins the selected piece to the one after it.
    fn join_selected(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.selected else { return };
        self.join_at(i, cx);
    }

    /// Switches the active timeline tool (`V` = Select, `S` = Split).
    fn set_tool(&mut self, tool: TimelineTool, cx: &mut Context<Self>) {
        if self.tool != tool {
            self.tool = tool;
            cx.notify();
        }
    }

    /// Cuts the piece at timeline time `at` in two.
    fn split_at(&mut self, at: Hns, cx: &mut Context<Self>) {
        let Some((i, _)) = self.timeline.locate(at) else { return };
        let Some(piece) = self.timeline.pieces.get(i) else { return };
        let keyframes = &self.sources[piece.source].info.keyframes;
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

    /// Cuts the piece under the playhead in two.
    fn split(&mut self, cx: &mut Context<Self>) {
        self.split_at(self.player.playhead, cx);
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
            // Nothing is left to play: stop the player so it doesn't run on over a stale piece.
            self.player.pause();
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
            self.restart_if_playing(cx);
            cx.notify();
        }
    }

    /// Solos `track` in preview (`None` returns to the full mix of individual tracks).
    fn set_solo(&mut self, track: Option<usize>, cx: &mut Context<Self>) {
        self.solo = track;
        let saved = u8::try_from(track.unwrap_or(0)).unwrap_or(0);
        self.remember_prefs(cx, move |e| e.listen_track = saved);
        self.restart_if_playing(cx);
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

    /// Whether export should rebuild track 0 (Master) from the individually edited tracks `1..`.
    fn should_rebuild_master(&self) -> bool {
        self.timeline.pieces.first().is_some_and(|p| p.tracks.len() > 1)
            && (self.tracks_open
                || self
                    .timeline
                    .pieces
                    .iter()
                    .any(|p| p.tracks.iter().skip(1).any(|t| t.audio_edit() != AudioEdit::Keep)))
    }

    /// The plans for pieces `which`, or a message for the first that can't be made.
    fn plans(&self, which: &[usize]) -> Result<Vec<PiecePlan>, String> {
        let rebuild_master = self.should_rebuild_master();
        which
            .iter()
            .map(|&i| {
                let piece = self.timeline.pieces.get(i).ok_or("That piece is gone.")?;
                let info = &self.sources[piece.source].info;
                model::plan_piece(
                    piece,
                    info.duration,
                    &info.keyframes,
                    self.bitrate_bps,
                    rebuild_master,
                )
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
            let tag = format!("clip {}", self.timeline.label(i));
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
            // Esc drops a piece being dragged back where it was.
            "escape" if self.moving.is_some() => {
                self.moving = None;
                cx.notify();
            }
            "f" if plain => self.set_fullscreen(!self.fullscreen, window, cx),
            "escape" if self.fullscreen => self.set_fullscreen(false, window, cx),
            "space" if plain => self.toggle_play(cx),
            // The preview volume. The editor has no text field, so these keys are always free.
            "m" if plain => self.toggle_mute(cx),
            "up" if plain => self.adjust_volume(VOLUME_STEP, cx),
            "down" if plain => self.adjust_volume(-VOLUME_STEP, cx),
            // Watching is only watching: the editing keys wait for the Edit button.
            _ if !self.editing => {}
            "escape" if plain && self.sel.is_some() => {
                self.sel = None;
                self.sel_anchor = None;
                cx.notify();
            }
            "escape" if plain && self.tool != TimelineTool::Select => {
                self.set_tool(TimelineTool::Select, cx);
            }
            "g" if plain => self.gap_selection(cx),
            "delete" | "backspace" if plain && self.sel.is_some() => self.cut_selection(cx),
            "v" if plain => self.set_tool(TimelineTool::Select, cx),
            "s" if plain => self.set_tool(TimelineTool::Split, cx),
            "delete" | "backspace" if plain && self.range.is_some() => self.silence_range(cx),
            "delete" | "backspace" if plain => self.delete_selected(cx),
            "j" if plain => self.join_selected(cx),
            "left" if k.modifiers.alt && !k.modifiers.control => self.shift_selected(-1, cx),
            "right" if k.modifiers.alt && !k.modifiers.control => self.shift_selected(1, cx),
            "z" if k.modifiers.control && !k.modifiers.alt => {
                self.step_history(k.modifiers.shift, cx);
            }
            "y" if k.modifiers.control => self.step_history(true, cx),
            "v" if k.modifiers.control => self.paste(cx),
            _ => {}
        }
    }
}

/// Converts a piece's track edits into preview [`Sound`] entries and optional solo track id.
///
/// - Simple mode (`!separate_tracks`, or single-track clips): plays track 0 (Master).
/// - Separate-tracks mode (`separate_tracks && tracks.len() > 1`):
///   - With `solo = Some(t)`: plays only track `t`.
///   - With `solo = None`: excludes Master (track 0) and mixes all individual tracks `1..` so
///     muting, silencing, or adjusting volume on any track is heard accurately.
fn build_piece_sounds(
    tracks: &[TrackEdit],
    from_offset: Hns,
    separate_tracks: bool,
    solo: Option<usize>,
) -> (Vec<Sound>, Option<usize>) {
    if tracks.is_empty() {
        return (Vec::new(), None);
    }
    let to_sound = |track: usize, edit: &TrackEdit| Sound {
        track,
        mute: edit.mute,
        gain_db: edit.gain_db,
        line: edit.line_from(from_offset),
        silence_ranges: Vec::new(),
    };
    if !separate_tracks || tracks.len() <= 1 {
        return (vec![to_sound(0, &tracks[0])], None);
    }
    if let Some(solo_idx) = solo.filter(|&i| i < tracks.len()) {
        return (vec![to_sound(solo_idx, &tracks[solo_idx])], Some(solo_idx));
    }
    let mixed = tracks.iter().enumerate().skip(1).map(|(t, e)| to_sound(t, e)).collect();
    (mixed, None)
}

/// Computes `(x, source_timestamp)` for every `THUMB_W` tile slot across a piece `width` px wide.
fn desired_thumb_times(a: Hns, span: Hns, width: f32) -> Vec<(f32, Hns)> {
    if width < 1.0 || span <= Hns::ZERO {
        return Vec::new();
    }
    (0..(width / THUMB_W).ceil() as usize)
        .map(|k| {
            let x = k as f32 * THUMB_W;
            let mid = ((x + THUMB_W / 2.0).min(width) / width).clamp(0.0, 1.0);
            let t = Hns::from_ticks(a.ticks() + (span.ticks() as f64 * f64::from(mid)) as i64);
            (x, t)
        })
        .collect()
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

/// The thumbnails that tile a piece `width` px wide: one `THUMB_W` slot at a time, taking the
/// quantized cached frame or nearest already-cached frame for `path` (never flashing to blank
/// while new thumbnails load). Returns each slot's x within the piece.
fn thumb_tiles(
    cache: &ThumbCache<Arc<RenderImage>>,
    path: &Path,
    a: Hns,
    span: Hns,
    width: f32,
) -> Vec<(f32, Arc<RenderImage>)> {
    desired_thumb_times(a, span, width)
        .into_iter()
        .filter_map(|(x, t)| Some((x, cache.nearest_for_path(path, t)?.clone())))
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
    theme::caption(text)
}

/// Whether the preview Stop button is active: a timeline is loaded and playback is either running
/// or parked past the start of the timeline.
#[must_use]
fn can_stop(ready: bool, playing: bool, playhead: Hns) -> bool {
    ready && (playing || playhead > Hns::ZERO)
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.window_activation_sub.is_none() {
            self.window_activation_sub =
                Some(cx.observe_window_activation(window, |this, window, cx| {
                    this.handle_window_activation(window.is_window_active(), cx);
                }));
        }
        if !window.is_window_active() {
            self.handle_window_activation(false, cx);
        }
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
        self.load_volume_prefs(cx);
        // Keep visible thumbnails warm in the LRU and backfill any missing quantized slots.
        self.ensure_visible_thumbs(cx);
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        // Keys only reach the page while it has focus, which coming back from another tab loses.
        if (self.focus_pending || window.focused(cx).is_none())
            && !focus.contains_focused(window, cx)
        {
            window.focus(&focus, cx);
        }
        self.focus_pending = false;
        let viewport = window.viewport_size();
        let vp_h = f32::from(viewport.height);
        let vp_w = f32::from(viewport.width);
        let share = if self.editing { VIDEO_SHARE } else { 0.68 };
        let video_height = (vp_h * share).clamp(VIDEO_MIN, VIDEO_MAX * 1.5);
        let wide = vp_w >= WIDE;
        let split_below = vp_w >= SPLIT_BELOW;

        let empty = self.timeline.pieces.is_empty() && self.loading.is_empty();
        let tracks = self.timeline.pieces.first().map_or(0, |p| p.tracks.len());
        let shown_tracks = if self.tracks_open { tracks } else { tracks.min(1) };
        // Shell chrome: 48 px top bar + 28 px status bar.
        let available_h = (vp_h - 76.0).max(0.0);
        let extra_h = (if self.problem.is_some() { 48.0 } else { 0.0 })
            + (if self.loading.is_empty() { 0.0 } else { 48.0 })
            + (if self.picking { 248.0 } else { 0.0 })
            + (if self.confirm_close { 148.0 } else { 0.0 });
        // Flex-fill when the viewport comfortably holds the full stage + timeline; fall back to
        // page scrolling on compact windows (such as 760x500) or when many tracks are expanded.
        let est_timeline_h = 168.0 + shown_tracks as f32 * 72.0;
        let flex_empty = empty && (!self.picking || available_h >= 240.0 + 248.0 + 84.0);
        let flex_watch = !self.timeline.pieces.is_empty()
            && !self.editing
            && available_h >= VIDEO_MIN + 150.0 + extra_h;
        let flex_edit = !self.timeline.pieces.is_empty()
            && self.editing
            && wide
            && available_h >= 84.0 + est_timeline_h + extra_h + VIDEO_MIN + 64.0;
        let flex_fill = flex_empty || flex_watch || flex_edit;

        let mut page = div()
            .id("editor-page")
            .track_focus(&focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .py_3()
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
        page = if flex_fill { page.overflow_hidden() } else { page.overflow_y_scroll() };
        if let Some(problem) = &self.problem {
            page = page.child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::DANGER_SOFT))
                    .border_1()
                    .border_color(rgb(theme::DANGER_BORDER))
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::DANGER))
                    .child(icon("x", theme::DANGER).size(px(14.)))
                    .child(div().flex_1().min_w_0().child(problem.clone())),
            );
        }
        if !self.loading.is_empty() {
            page = page.child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::SURFACE_RAISED))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::FG_SOFT))
                    .child(status_dot(rgb(theme::BRAND)))
                    .child(format!("Reading {}.", self.loading.join(", "))),
            );
        }
        if empty {
            page = page.child(self.drop_zone(flex_empty, cx));
        }
        if self.picking {
            page = page.child(self.picker(cx));
        }
        if self.confirm_close {
            page = page.child(self.close_prompt(cx));
        }
        if !self.timeline.pieces.is_empty() && !self.editing {
            if flex_watch {
                page =
                    page.child(div().flex_1().min_h_0().w_full().child(self.player_view(None, cx)));
            } else {
                page = page.child(self.player_view(Some(video_height), cx));
            }
        }
        if !self.timeline.pieces.is_empty() && self.editing {
            // Wide windows pair the hero video with a slim right inspector and full-width timeline
            // below; narrow windows (such as 760x500) keep video + timeline together at top and
            // fold inspector + export underneath so nothing clips.
            if flex_edit {
                let side = div()
                    .id("editor-side")
                    .w(px(SIDE_WIDTH))
                    .flex_none()
                    .min_h_0()
                    .h_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(self.inspector(cx))
                    .child(self.finish_card(cx));
                let top = div()
                    .w_full()
                    .flex_1()
                    .min_h(px(VIDEO_MIN + 60.))
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .h_full()
                            .child(self.player_view(None, cx)),
                    )
                    .child(side);
                page = page.child(top).child(self.timeline_view(cx).flex_none());
            } else if wide {
                let side = div()
                    .w(px(SIDE_WIDTH))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(self.inspector(cx))
                    .child(self.finish_card(cx));
                let top = div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(self.player_view(Some(video_height), cx)))
                    .child(side);
                page = page.child(top).child(self.timeline_view(cx));
            } else {
                let inspector = self.inspector(cx);
                let finish = self.finish_card(cx);
                let below = match (split_below, inspector) {
                    (true, Some(insp)) => div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap_3()
                        .child(div().flex_1().min_w_0().child(insp))
                        .child(div().w(px(276.)).flex_none().child(finish)),
                    (_, Some(insp)) => {
                        div().w_full().flex().flex_col().gap_3().child(insp).child(finish)
                    }
                    (_, None) => div().w_full().child(finish),
                };
                page = page
                    .child(self.player_view(Some(video_height), cx))
                    .child(self.timeline_view(cx))
                    .child(below);
            }
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
                "{} {} \u{00b7} {}",
                self.timeline.pieces.len(),
                if self.timeline.pieces.len() == 1 { "piece" } else { "pieces" },
                human_duration(self.timeline.total())
            )
        });
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                button("editor-pick", "Add clip")
                    .child(icon("plus", theme::FG_SOFT).size(px(14.)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picking = !this.picking;
                        cx.notify();
                    })),
            )
            .children((!self.timeline.pieces.is_empty()).then(|| {
                button("editor-close", "Close")
                    .child(icon("x", theme::FG_SOFT).size(px(14.)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_close = true;
                        cx.notify();
                    }))
            }));
        div()
            .flex_none()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(theme::page_title("Editor"))
            .children(length.map(|meta| {
                div()
                    .h(px(24.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(rgb(theme::SURFACE_RAISED))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::FG_SOFT))
                    .child(meta)
            }))
            .child(div().flex_1())
            .child(actions)
    }

    /// The empty editor: a full-height dashed target for dropping a video, with a clear primary
    /// action to choose a clip and a Ctrl+V hint.
    fn drop_zone(&self, fill: bool, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let mut zone = div()
            .id("editor-drop")
            .tab_stop(true)
            .role(Role::Button)
            .aria_label("Drop a video here or choose one of your clips")
            .w_full()
            .p_6()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .rounded(px(theme::RADIUS_LG))
            .border_2()
            .border_dashed()
            .border_color(rgb(theme::BORDER_STRONG))
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .bg(rgb(theme::CARD))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)).border_color(rgb(theme::BRAND_BORDER)))
            .active(|s| s.bg(rgb(theme::ACCENT)))
            .drag_over::<ExternalPaths>(|s, _, _, _| {
                s.bg(rgb(theme::BRAND_SOFT)).border_color(rgb(theme::BRAND))
            });
        zone = if fill { zone.flex_1().min_h(px(240.)) } else { zone.min_h(px(220.)) };
        zone.child(
            div()
                .size(px(56.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(rgb(theme::BRAND_SOFT))
                .border_1()
                .border_color(rgb(theme::BRAND_BORDER))
                .child(icon("film", theme::BRAND).size(px(24.))),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_size(px(16.))
                        .line_height(px(22.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::FG))
                        .child("Drop a video here"),
                )
                .child(
                    div()
                        .text_size(px(theme::TEXT_BODY))
                        .line_height(px(18.))
                        .text_color(rgb(theme::MUTED_FG))
                        .child(if self.picking {
                            "Or pick one of your clips below"
                        } else {
                            "Drop an MP4 or MKV anywhere on this window, or choose one below"
                        }),
                ),
        )
        .child(
            button_primary(
                "editor-choose",
                if self.picking { "Hide clips" } else { "Choose a clip" },
            )
            .h(px(theme::CONTROL_H_LG))
            .px_4()
            .child(icon("folder-open", theme::PRIMARY_FG).size(px(15.)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.picking = !this.picking;
                cx.notify();
            })),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_size(px(theme::TEXT_CAPTION))
                .text_color(rgb(theme::MUTED_FG))
                .child("Or paste a file or path with")
                .child(kbd("Ctrl+V")),
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.picking = !this.picking;
            cx.notify();
        }))
    }

    /// Asks before closing the project, since closing drops every edit.
    fn close_prompt(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::DANGER_BORDER))
            .child(theme::section_heading("Close the editor?"))
            .child(muted(
                "The pieces and every edit you made are dropped. Clips on disk are not touched.",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .child(button_danger("close-yes", "Yes, close it").on_click(cx.listener(
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
            div()
                .p_3()
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::SURFACE_INSET))
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(muted("No videos yet. Saved clips will show up here."))
                .into_any_element()
        } else {
            div()
                .id("editor-picker")
                .max_h(px(180.))
                .overflow_y_scroll()
                .p_1()
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::SURFACE_INSET))
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .flex()
                .flex_col()
                .gap_1()
                .children(videos.into_iter().enumerate().map(|(i, (name, path))| {
                    let aria: SharedString = name.clone().into();
                    div()
                        .id(("editor-clip", i))
                        .tab_stop(true)
                        .border_1()
                        .border_color(transparent_black())
                        .focus_visible(|s| s.border_color(rgb(theme::RING)))
                        .role(Role::Button)
                        .aria_label(aria)
                        .h(px(theme::CONTROL_H))
                        .px_2()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .rounded(px(theme::RADIUS_SM))
                        .text_size(px(theme::TEXT_BODY))
                        .text_color(rgb(theme::FG_SOFT))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)))
                        .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)).text_color(rgb(theme::FG)))
                        .child(icon("film", theme::MUTED_FG).size(px(14.)))
                        .child(div().flex_1().min_w_0().truncate().child(name))
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
            .flex_none()
            .child(muted("Pick one, drop video files anywhere on this page, or paste a file or its path with Ctrl+V."))
            .child(body)
    }

    /// Transport Stop button matching the Play button's geometry, muted and non-interactive when
    /// already stopped at the start of the timeline.
    fn stop_button(&self, id: &'static str, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let ready = self.ffmpeg.is_some() && !self.timeline.pieces.is_empty();
        let enabled = can_stop(ready, self.player.playing, self.player.playhead);
        let base = div()
            .id(id)
            .tab_stop(enabled)
            .border_1()
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .role(Role::Button)
            .aria_label("Stop")
            .w(px(40.))
            .h(px(theme::CONTROL_H))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD));
        if enabled {
            base.bg(rgb(theme::SURFACE_RAISED))
                .border_color(rgb(theme::INPUT))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
                .active(|s| {
                    s.bg(rgb(theme::SURFACE_ACTIVE)).border_color(rgb(theme::BORDER_STRONG))
                })
                .child(icon("square", theme::FG).size(px(14.)))
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            base.bg(rgb(theme::MUTED))
                .border_color(rgb(theme::BORDER_SUBTLE))
                .opacity(0.55)
                .cursor_default()
                .child(icon("square", theme::TEXT_DIM).size(px(14.)))
        }
    }

    /// The picture, with centered transport underneath and quick playhead tools.
    fn player_view(&self, height: Option<f32>, cx: &mut Context<Self>) -> gpui::Div {
        let mut video = div()
            .w_full()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::VIDEO_BG))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        video = match height {
            // In page-scroll mode, a flex column would squeeze the picture without a fixed height.
            Some(h) => video.h(px(h)).flex_none(),
            None => video.flex_1().min_h(px(VIDEO_MIN)),
        };
        let video = match &self.player.shown {
            Some(frame) => video.child(
                img(ImageSource::Render(frame.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => video.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(icon("film", theme::MUTED_FG).size(px(24.)))
                    .child(muted("Loading the picture.")),
            ),
        };
        let total = self.timeline.total();
        let play_label: SharedString = if self.player.playing { "Pause" } else { "Play" }.into();
        let play = div()
            .id("editor-play")
            .tab_stop(true)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .role(Role::Button)
            .aria_label(play_label)
            .w(px(40.))
            .h(px(theme::CONTROL_H))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::PRIMARY))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::PRIMARY_HOVER)))
            .active(|s| s.bg(rgb(theme::PRIMARY_ACTIVE)))
            .child(icon(if self.player.playing { "pause" } else { "play" }, theme::PRIMARY_FG))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx)));

        let timecode = div()
            .h(px(theme::CONTROL_H))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::SURFACE_INSET))
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .text_size(px(theme::TEXT_CAPTION))
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::FG))
                    .child(clock(self.player.playhead)),
            )
            .child(div().text_color(rgb(theme::TEXT_DIM)).child("/"))
            .child(div().text_color(rgb(theme::MUTED_FG)).child(clock(total)));

        let tool_chip = self.editing.then(|| {
            let select_active = self.tool == TimelineTool::Select;
            let split_active = self.tool == TimelineTool::Split;
            let select_btn = div()
                .id("editor-tool-select")
                .tab_stop(true)
                .role(Role::RadioButton)
                .aria_label("Select tool (V)")
                .aria_selected(select_active)
                .h(px(24.))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.))
                .rounded(px(theme::RADIUS_SM))
                .border_1()
                .focus_visible(|s| s.border_color(rgb(theme::RING)))
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .child("Select")
                .child(kbd("V"))
                .on_click(cx.listener(|this, _, _, cx| this.set_tool(TimelineTool::Select, cx)));
            let select_btn = if select_active {
                select_btn
                    .bg(rgb(theme::SURFACE_RAISED))
                    .border_color(rgb(theme::BORDER_STRONG))
                    .text_color(rgb(theme::FG))
            } else {
                select_btn
                    .border_color(transparent_black())
                    .text_color(rgb(theme::MUTED_FG))
                    .hover(|s| s.bg(rgb(theme::CARD)).text_color(rgb(theme::FG)))
                    .active(|s| s.bg(rgb(theme::ACCENT)))
            };

            let split_btn = div()
                .id("editor-tool-split")
                .tab_stop(true)
                .role(Role::RadioButton)
                .aria_label("Split tool (S)")
                .aria_selected(split_active)
                .h(px(24.))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.))
                .rounded(px(theme::RADIUS_SM))
                .border_1()
                .focus_visible(|s| s.border_color(rgb(theme::RING)))
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .child(
                    icon("scissors", if split_active { theme::BRAND } else { theme::MUTED_FG })
                        .size(px(12.)),
                )
                .child("Split")
                .child(kbd("S"))
                .on_click(cx.listener(|this, _, _, cx| this.set_tool(TimelineTool::Split, cx)));
            let split_btn = if split_active {
                split_btn
                    .bg(rgb(theme::BRAND_SOFT))
                    .border_color(rgb(theme::BRAND_BORDER))
                    .text_color(rgb(theme::BRAND))
            } else {
                split_btn
                    .border_color(transparent_black())
                    .text_color(rgb(theme::MUTED_FG))
                    .hover(|s| s.bg(rgb(theme::CARD)).text_color(rgb(theme::FG)))
                    .active(|s| s.bg(rgb(theme::ACCENT)))
            };

            div()
                .h(px(theme::CONTROL_H))
                .p(px(3.))
                .flex()
                .flex_none()
                .flex_row()
                .items_center()
                .gap(px(2.))
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::SURFACE_INSET))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(select_btn)
                .child(split_btn)
        });

        let left_tools = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .children(self.editing.then(|| {
                button("editor-split", "Split")
                    .child(icon("scissors", theme::FG_SOFT).size(px(14.)))
                    .on_click(cx.listener(|this, _, _, cx| this.split(cx)))
            }))
            .children(self.editing.then(|| {
                button("editor-add-gap", "Add gap")
                    .aria_label("Add a 2 second black gap")
                    .child(icon("plus", theme::FG_SOFT).size(px(14.)))
                    .on_click(cx.listener(|this, _, _, cx| this.add_gap(cx)))
            }))
            .children(self.editing.then(|| {
                button("editor-make-gap", "Make gap")
                    .aria_label("Replace the picked range with a black gap (G)")
                    .opacity(if self.sel.is_some() { 1.0 } else { 0.45 })
                    .child(icon("square", theme::FG_SOFT).size(px(14.)))
                    .on_click(cx.listener(|this, _, _, cx| this.gap_selection(cx)))
            }))
            .children(self.editing.then(|| {
                button("editor-delete", "Delete")
                    .aria_label("Delete piece")
                    .text_color(rgb(theme::FG_SOFT))
                    .child(icon("trash-2", theme::MUTED_FG).size(px(14.)))
                    .hover(|s| {
                        s.bg(rgb(theme::DANGER_SOFT))
                            .border_color(rgb(theme::DANGER_BORDER))
                            .text_color(rgb(theme::DANGER))
                    })
                    .active(|s| s.bg(rgb(theme::DANGER_SOFT)).opacity(0.85))
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx)))
            }))
            .children(tool_chip);

        let center_transport = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .child(play)
            .child(self.stop_button("editor-stop", cx));

        let shown_volume = if self.muted { 0 } else { self.volume };
        let mute_aria: SharedString =
            if self.muted || self.volume == 0 { "Unmute (M)" } else { "Mute (M)" }.into();
        let speaker = div()
            .id("editor-mute")
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(mute_aria)
            .w(px(theme::CONTROL_H))
            .h(px(theme::CONTROL_H))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::ACCENT)))
            .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
            .child(icon(
                volume_icon(self.volume, self.muted),
                if shown_volume == 0 { theme::MUTED_FG } else { theme::FG_SOFT },
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx)));
        let view = cx.entity().downgrade();
        let volume_slider = slider(
            "editor-volume-slider",
            f32::from(shown_volume) / 100.0,
            &self.volume_slider,
            move |frac, _, cx| {
                let pct = (frac * 100.0).round() as u8;
                let _ = view.update(cx, |this, cx| this.set_volume(pct, cx));
            },
        )
        .w(px(108.))
        .min_w(px(88.))
        .flex_none()
        .role(Role::Slider)
        .aria_label(SharedString::from(format!("Preview volume, {shown_volume}%")));
        let volume_ctl = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap_1()
            .child(speaker)
            .child(volume_slider);

        let right_tools =
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .justify_end()
                .gap_2()
                .child(timecode)
                .child(volume_ctl)
                .child(button_ghost("editor-fullscreen", "Fullscreen").child(kbd("F")).on_click(
                    cx.listener(|this, _, window, cx| this.set_fullscreen(true, window, cx)),
                ))
                .children((!self.editing).then(|| {
                    button_primary("editor-edit", "Edit")
                        .on_click(cx.listener(|this, _, _, cx| this.start_editing(cx)))
                }));

        let controls = div()
            .flex_none()
            .w_full()
            .px_3()
            .py_2()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(left_tools)
            .child(center_transport)
            .child(right_tools);

        let mut out = if height.is_none() {
            div().size_full().min_h_0().flex().flex_col().gap_2().child(video).child(controls)
        } else {
            div().flex().flex_col().gap_2().child(video).child(controls)
        };
        if !self.editing {
            out = out.child(
                muted(
                    "Space plays, F toggles fullscreen. Click Edit to cut, trim and adjust audio.",
                )
                .flex_none(),
            );
        }
        out
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
            .aria_label("Seek timeline")
            .relative()
            .w_full()
            .h(px(28.))
            .rounded(px(theme::RADIUS_SM))
            .bg(rgb(theme::SURFACE_INSET))
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
        let ph_x = model::time_to_x(self.player.playhead, total, width);
        strip = strip
            .child(div().absolute().left(px(ph_x - 1.0)).w(px(2.)).h_full().bg(rgb(theme::FG)));
        let picture = match &self.player.shown {
            Some(frame) => div().flex_1().min_h_0().child(
                img(ImageSource::Render(frame.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(muted("Loading the picture.")),
        };
        let play_label: SharedString = if self.player.playing { "Pause" } else { "Play" }.into();
        let play = div()
            .id("editor-full-play")
            .tab_stop(true)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .role(Role::Button)
            .aria_label(play_label)
            .w(px(40.))
            .h(px(theme::CONTROL_H))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::PRIMARY))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::PRIMARY_HOVER)))
            .active(|s| s.bg(rgb(theme::PRIMARY_ACTIVE)))
            .child(icon(if self.player.playing { "pause" } else { "play" }, theme::PRIMARY_FG))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx)));
        let piece_badge = self.selected.map(|i| {
            div()
                .px_2()
                .py(px(2.))
                .rounded(px(theme::RADIUS_SM))
                .bg(rgb(theme::BRAND_SOFT))
                .border_1()
                .border_color(rgb(theme::BRAND_BORDER))
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::BRAND))
                .child(self.timeline.title(i))
        });
        let bar_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .text_size(px(theme::TEXT_CAPTION))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(theme::FG))
                                    .child(clock(self.player.playhead)),
                            )
                            .child(div().text_color(rgb(theme::TEXT_DIM)).child("/"))
                            .child(div().text_color(rgb(theme::MUTED_FG)).child(clock(total))),
                    )
                    .children(piece_badge),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(play)
                    .child(self.stop_button("editor-full-stop", cx)),
            )
            .child(div().flex_1().min_w_0().flex().flex_row().items_center().justify_end().child(
                button("editor-full-exit", "Exit fullscreen").child(kbd("Esc")).on_click(
                    cx.listener(|this, _, window, cx| this.set_fullscreen(false, window, cx)),
                ),
            ));
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(rgb(theme::VIDEO_BG))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(picture)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(theme::RADIUS))
                    .bg(rgb(theme::CARD))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .child(strip)
                    .child(bar_row),
            )
    }

    /// The pieces on a ruler with an extract button above each, then one lane per audio track.
    fn timeline_view(&self, cx: &mut Context<Self>) -> gpui::Div {
        let width = if self.drawn_width > 0.0 { self.drawn_width } else { FALLBACK_WIDTH };
        // While an edge is dragged the scale and the other pieces stay put, so the edge follows the
        // mouse and what was cut off shows as a gray area; the pieces close up on release.
        let total = self.trim.as_ref().map_or_else(|| self.disp_total(), |t| t.total);
        let xd = |d: Hns| model::time_to_x(d, total, width);
        let x_of = |t: Hns| xd(self.to_disp(t));
        let (gl, gr) = self.ghost();
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
            let start = if self.trim.is_some() { start } else { self.to_disp(start) };
            (xd(start), xd(start + span))
        };
        let tracks = self.timeline.pieces.first().map_or(0, |p| p.tracks.len());
        // The trimmed-off parts of piece `i` (drawn at `x0..x1`) as gray room around it: x range
        // and the source time range each one stands for, so the video and audio lanes can show
        // what was cut. The left one runs `gl` back from the piece's start, the right `gr` on.
        let ghosts = |i: usize, x0: f32, x1: f32| -> Vec<(f32, f32, Hns, Hns)> {
            let piece = &self.timeline.pieces[i];
            let room = xd(gl) - xd(Hns::ZERO);
            let room_r = xd(gr) - xd(Hns::ZERO);
            [(x0 - room, x0, piece.a - gl, piece.a), (x1, x1 + room_r, piece.b, piece.b + gr)]
                .into_iter()
                .filter(|(g0, g1, a, b)| g1 - g0 >= 2.0 && b > a)
                .collect()
        };

        // Which adjacent seams can be joined at the current zoom (hidden while dragging a trim edge).
        let joinable_seams: Vec<bool> = self
            .timeline
            .pieces
            .windows(2)
            .map(|pair| {
                self.trim.is_none() && model::seam_joinable(&pair[0], &pair[1], total, width)
            })
            .collect();

        // Above the ruler: each piece's letter, its length, extract button, and seam join icons
        // (placed here in the 32 px head row so they never overlap the 12 px trim handles below).
        let mut head = div().relative().w_full().h(px(HEAD_HEIGHT));
        for (i, piece) in self.timeline.pieces.iter().enumerate() {
            let (x0, x1) = place(i);
            let w = (x1 - x0).max(1.0);
            let join_left = i > 0 && joinable_seams.get(i - 1).copied().unwrap_or(false);
            let join_right = joinable_seams.get(i).copied().unwrap_or(false);
            let pad_l = if join_left && w >= 64.0 { 16.0 } else { 4.0 };
            let pad_r = if join_right && w >= 64.0 { 16.0 } else { 4.0 };
            let inner_w = (w - pad_l - pad_r).max(0.0);
            let piece_name = self.timeline.label(i);
            let label = if piece.gap {
                if inner_w >= 60.0 {
                    format!("Gap  {}", model::secs_label(piece.span()))
                } else {
                    String::new()
                }
            } else if inner_w >= 150.0 {
                let src = &self.sources[piece.source].info.duration;
                let (cut_l, cut_r) = (piece.a, (*src - piece.b).max(Hns::ZERO));
                let trimmed = if self.selected == Some(i)
                    && (cut_l >= Hns::from_millis(200) || cut_r >= Hns::from_millis(200))
                {
                    format!(
                        "  (trimmed {} / {})",
                        model::secs_label(cut_l),
                        model::secs_label(cut_r)
                    )
                } else {
                    String::new()
                };
                format!("{piece_name}  {}{trimmed}", model::secs_label(piece.span()))
            } else if inner_w >= 60.0 {
                piece_name.clone()
            } else {
                String::new()
            };
            let extract_aria: SharedString = format!("Extract piece {piece_name}").into();
            let extract = (inner_w >= 24.0 && !piece.gap).then(|| {
                div()
                    .id(("extract", i))
                    .tab_stop(true)
                    .flex_none()
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(|s| s.border_color(rgb(theme::RING)))
                    .role(Role::Button)
                    .aria_label(extract_aria)
                    .w(px(if inner_w >= 36.0 { theme::CONTROL_H } else { 24.0 }))
                    .h(px(theme::CONTROL_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_SM))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER)))
                    .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
                    .child(icon("download", theme::FG_SOFT).size(px(14.)))
                    .on_click(cx.listener(move |this, _, _, cx| this.extract(&[i], cx)))
            });
            let is_sel = self.selected == Some(i);
            let mut slot = div()
                .absolute()
                .left(px(x0))
                .w(px(w))
                .h_full()
                .pl(px(pad_l))
                .pr(px(pad_r))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .overflow_hidden()
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(if is_sel { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                .text_color(rgb(if is_sel { theme::BRAND } else { theme::MUTED_FG }));
            if label.is_empty() {
                slot = slot.justify_center().children(extract);
            } else {
                slot = slot
                    .justify_between()
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .children(extract);
            }
            head = head.child(slot);
        }
        // Seam join buttons in the head row: 24x24 visible pill inside a 32x32 invisible hit
        // target where space allows (24x32 minimum), wired to the same `join_at` path as pressing J.
        for (i, &joinable) in joinable_seams.iter().enumerate() {
            if !joinable {
                continue;
            }
            let (l0, l1) = place(i);
            let (r0, r1) = place(i + 1);
            let min_side = (l1 - l0).min(r1 - r0);
            let hit_w = if min_side >= 40.0 { 32.0 } else { 24.0 };
            let seam_x = (l1 + r0) / 2.0;
            let left = (seam_x - hit_w / 2.0).clamp(0.0, (width - hit_w).max(0.0));
            let aria: SharedString = format!(
                "Join pieces {} and {} (J)",
                self.timeline.label(i),
                self.timeline.label(i + 1)
            )
            .into();
            let join_pill = div()
                .w(px(24.))
                .h(px(24.))
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_SM))
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER_STRONG))
                .child(icon("chevron-right", theme::FG_SOFT).size(px(11.)))
                .child(div().ml(px(-4.)).child(icon("chevron-left", theme::FG_SOFT).size(px(11.))));
            head = head.child(
                div()
                    .id(("seam-join", i))
                    .tab_stop(true)
                    .role(Role::Button)
                    .aria_label(aria)
                    .absolute()
                    .top_0()
                    .left(px(left))
                    .w(px(hit_w))
                    .h(px(HEAD_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_SM))
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(|s| s.border_color(rgb(theme::RING)))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::BRAND_SOFT)))
                    .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| this.join_at(i, cx)))
                    .child(join_pill),
            );
        }

        let bounds = self.strip.clone();
        let mut video_lane = div()
            .relative()
            .w_full()
            .h(px(VIDEO_LANE_HEIGHT))
            .rounded(px(theme::RADIUS_SM))
            .bg(rgb(theme::SURFACE_INSET))
            .overflow_hidden()
            .child(canvas(move |b, _, _| bounds.set(b), |_, (), _, _| {}).absolute().size_full());
        let mut lifted = None;
        for (i, piece) in self.timeline.pieces.iter().enumerate() {
            let (x0, x1) = place(i);
            let selected = self.selected == Some(i);
            // The piece being dragged to a new place follows the mouse.
            let drag_dx = self.moving.as_ref().filter(|m| m.active && m.from == i).map(|m| m.dx);
            let (x0, x1) = match drag_dx {
                Some(dx) => {
                    let w = x1 - x0;
                    let left = (x0 + dx).clamp(0.0, (width - w).max(0.0));
                    (left, left + w)
                }
                None => (x0, x1),
            };
            // Handles on the selected piece's two edges: 12 px grab targets with crisp inner edge
            // and bold grip bars (active in Select mode only so Split mode clicks always split).
            let show_handles = selected && self.tool == TimelineTool::Select && drag_dx.is_none();
            let handles: Vec<gpui::Stateful<gpui::Div>> = if show_handles {
                [true, false]
                    .into_iter()
                    .map(|left| {
                        let piece = piece.clone();
                        let starts: Vec<Hns> = (0..self.timeline.pieces.len())
                            .map(|k| self.to_disp(self.timeline.start_of(k)))
                            .collect();
                        let total_now = self.disp_total();
                        let per_px = f64::from(total_now.ticks() as f32)
                            / f64::from(f32::from(self.strip.get().size.width).max(1.0));
                        let dragging =
                            self.trim.as_ref().is_some_and(|t| t.index == i && t.left == left);
                        let side = div()
                            .id(("trim", i * 2 + usize::from(left)))
                            .role(Role::Button)
                            .aria_label(if left { "Trim start" } else { "Trim end" })
                            .absolute()
                            .top_0();
                        let side = if left {
                            side.left_0().border_r_1().border_color(rgba(theme::SCRIM))
                        } else {
                            side.right_0().border_l_1().border_color(rgba(theme::SCRIM))
                        };
                        let grip = div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(2.))
                            .child(
                                div()
                                    .w(px(2.))
                                    .h(px(20.))
                                    .rounded_full()
                                    .bg(rgb(theme::PRIMARY_FG)),
                            )
                            .child(
                                div()
                                    .w(px(2.))
                                    .h(px(20.))
                                    .rounded_full()
                                    .bg(rgb(theme::PRIMARY_FG)),
                            );
                        side.w(px(HANDLE_W))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(if dragging { theme::PRIMARY } else { theme::BRAND }))
                            .hover(|s| s.bg(rgb(theme::BRAND_HOVER)))
                            .active(|s| s.bg(rgb(theme::PRIMARY)))
                            .cursor_col_resize()
                            .child(grip)
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
            // The part of the piece being cut off stays as a muted gray area until release.
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
                        .bg(rgb(theme::GHOST_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .opacity(0.85),
                );
            }
            // What was trimmed off the selected piece stays in place as a low-contrast muted gray
            // ghost so the scale doesn't change; the handles above still trim it.
            if selected && self.trim.is_none() {
                let src_path = &self.sources[piece.source].path;
                for (g0, g1, a, b) in ghosts(i, x0, x1) {
                    let tiles = thumb_tiles(&self.thumb_cache, src_path, a, b - a, g1 - g0);
                    video_lane = video_lane.child(
                        div()
                            .absolute()
                            .left(px(g0))
                            .w(px(g1 - g0))
                            .h_full()
                            .overflow_hidden()
                            .bg(rgb(theme::GHOST_BG))
                            .border_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .children(tiles.into_iter().map(|(x, image)| {
                                img(ImageSource::Render(image))
                                    .absolute()
                                    .left(px(x))
                                    .top_0()
                                    .w(px(THUMB_W))
                                    .h_full()
                                    .object_fit(ObjectFit::Cover)
                                    // Clearly dimmer and lower contrast than the kept region.
                                    .opacity(0.22)
                            })),
                    );
                }
            }
            let tiles = if piece.gap {
                Vec::new()
            } else {
                thumb_tiles(
                    &self.thumb_cache,
                    &self.sources[piece.source].path,
                    piece.a,
                    piece.span(),
                    x1 - x0,
                )
            };
            let piece_w = (x1 - x0 - 1.0).max(1.0);
            // A gap is not a clip: dark, striped and named, with no thumbnails.
            let gap_fill = piece.gap.then(|| {
                let stripes = (0..((piece_w / 8.0).ceil() as usize).min(300)).map(|k| {
                    div()
                        .absolute()
                        .left(px(k as f32 * 8.0))
                        .top_0()
                        .w(px(2.))
                        .h_full()
                        .bg(rgb(theme::BORDER_SUBTLE))
                });
                let name = (piece_w >= 56.0).then(|| {
                    div().absolute().size_full().flex().items_center().justify_center().child(
                        div()
                            .px(px(6.))
                            .py(px(1.))
                            .rounded(px(theme::RADIUS_SM))
                            .bg(rgb(theme::VIDEO_BG))
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::MUTED_FG))
                            .child(format!("Gap {:.1}s", piece.span().as_secs_f64())),
                    )
                });
                div().absolute().size_full().children(stripes).children(name)
            });
            let piece_badge = (piece_w >= 44.0 && !piece.gap).then(|| {
                div()
                    .ml(px(if show_handles { HANDLE_W + 4.0 } else { 6.0 }))
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(rgba(theme::SCRIM))
                    .border_1()
                    .border_color(rgb(if selected { theme::BRAND_BORDER } else { theme::BORDER }))
                    .text_size(px(theme::TEXT_MICRO))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(if selected { theme::BRAND } else { theme::FG_SOFT }))
                    .child(self.timeline.label(i))
            });
            let kept = div()
                .absolute()
                .left(px(x0))
                .w(px(piece_w))
                .h_full()
                .overflow_hidden()
                .bg(rgb(if piece.gap {
                    theme::VIDEO_BG
                } else if selected {
                    theme::BRAND_SOFT
                } else {
                    TINTS[i % 2]
                }))
                .flex()
                .items_center()
                .children(gap_fill)
                .children(tiles.into_iter().map(|(x, image)| {
                    img(ImageSource::Render(image))
                        .absolute()
                        .left(px(x))
                        .top_0()
                        .w(px(THUMB_W))
                        .h_full()
                        .object_fit(ObjectFit::Cover)
                        .opacity(if selected { 0.68 } else { 0.55 })
                }))
                .children(piece_badge)
                .children(handles);
            let kept = if selected {
                kept.border_2().border_color(rgb(theme::BRAND))
            } else {
                kept.border_1().border_color(rgb(theme::BRAND_BORDER))
            };
            if drag_dx.is_some() {
                // Lifted: translucent with a shadow, and drawn over its neighbours.
                lifted = Some(kept.opacity(0.7).shadow_md());
            } else {
                video_lane = video_lane.child(kept);
            }
        }
        video_lane = video_lane.children(lifted);
        // Where the dragged piece would land: a bar at the start of the piece it would go before,
        // or the end of the last one.
        if let Some(m) = self
            .moving
            .as_ref()
            .filter(|m| m.active && m.target != m.from && m.from < self.timeline.pieces.len())
        {
            let others: Vec<usize> =
                (0..self.timeline.pieces.len()).filter(|&k| k != m.from).collect();
            let x = match others.get(m.target) {
                Some(&k) => place(k).0,
                None => others.last().map_or(0.0, |&k| place(k).1),
            };
            video_lane = video_lane.child(
                div()
                    .absolute()
                    .top_0()
                    .left(px((x - 1.5).max(0.0)))
                    .w(px(3.))
                    .h_full()
                    .rounded_full()
                    .bg(rgb(theme::BRAND)),
            );
        }

        // Bookmarks: a crisp amber tick on the video lane wherever a kept piece holds one.
        for (i, piece) in self.timeline.pieces.iter().enumerate().filter(|(_, p)| !p.gap) {
            let (x0, x1) = place(i);
            let span = piece.span().as_secs_f64().max(0.001);
            for mark in &self.sources[piece.source].bookmarks {
                if *mark < piece.a || *mark > piece.b {
                    continue;
                }
                let x = x0 + ((*mark - piece.a).as_secs_f64() / span) as f32 * (x1 - x0);
                video_lane = video_lane
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(x - 1.0))
                            .w(px(2.))
                            .h_full()
                            .bg(rgb(theme::WARN)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(x - 3.0))
                            .w(px(6.))
                            .h(px(4.))
                            .rounded(px(1.))
                            .bg(rgb(theme::WARN)),
                    );
            }
        }

        let mut lanes = Vec::new();
        let shown = if self.tracks_open { tracks } else { tracks.min(1) };
        let track_audible = |t: usize| -> bool {
            if !self.tracks_open || tracks <= 1 {
                t == 0
            } else if let Some(s) = self.solo {
                t == s
            } else {
                t > 0
            }
        };
        for t in 0..shown {
            let audible = track_audible(t);
            let mut lane = div()
                .relative()
                .w_full()
                .h(px(LANE_HEIGHT))
                .rounded(px(theme::RADIUS_SM))
                .bg(rgb(theme::SURFACE_INSET))
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
                if piece.gap {
                    // Nothing to hear or edit: a dark block as long as the gap.
                    lane = lane.child(
                        div()
                            .absolute()
                            .left(px(x0))
                            .w(px(w))
                            .h_full()
                            .bg(rgb(theme::VIDEO_BG))
                            .border_1()
                            .border_color(rgb(if self.selected == Some(i) {
                                theme::BRAND_BORDER
                            } else {
                                theme::BORDER_SUBTLE
                            })),
                    );
                    continue;
                }
                let edit = &piece.tracks[t];
                let source = &self.sources[piece.source];
                if self.selected == Some(i) && self.trim.is_none() {
                    for (g0, g1, a, b) in ghosts(i, x0, x1) {
                        let gw = g1 - g0;
                        let mut ghost = div()
                            .absolute()
                            .left(px(g0))
                            .w(px(gw))
                            .h_full()
                            .overflow_hidden()
                            .bg(rgb(theme::GHOST_BG))
                            .border_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .opacity(0.45);
                        if let Some(Some(peaks)) = source.waves.get(t) {
                            let span = source
                                .info
                                .audio_spans
                                .get(t)
                                .copied()
                                .unwrap_or((Hns::ZERO, source.info.duration));
                            let columns =
                                model::column_peaks(peaks, span, a, b, (gw / BAR_STEP) as usize);
                            let n = columns.len().max(1);
                            ghost =
                                ghost.children(columns.into_iter().enumerate().map(|(c, peak)| {
                                    let h = (peak.min(1.0).sqrt() * LANE_HEIGHT).max(1.0);
                                    div()
                                        .absolute()
                                        .left(px((c as f32 + 0.5) / n as f32 * gw))
                                        .top(px((LANE_HEIGHT - h) / 2.0))
                                        .w(px(2.))
                                        .h(px(h))
                                        .bg(rgb(theme::TEXT_DIM))
                                }));
                        }
                        lane = lane.child(ghost);
                    }
                }
                let is_sel = self.selected == Some(i);
                let mut block = div()
                    .absolute()
                    .left(px(x0))
                    .w(px(w))
                    .h_full()
                    .overflow_hidden()
                    .bg(rgb(if is_sel { theme::BRAND_SOFT } else { theme::CARD }))
                    .border_1()
                    .border_color(rgb(if is_sel {
                        theme::BRAND_BORDER
                    } else {
                        theme::BORDER_SUBTLE
                    }))
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
                            .bg(rgb(if audible { theme::BRAND } else { theme::MUTED_FG }))
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
                            .border_1()
                            .border_color(rgb(theme::BG))
                    }));
                }
                lane = lane.child(block);
                if w >= 40.0 {
                    // Placed directly on the lane so muting dims the waveform without dimming the
                    // unmute button itself.
                    lane = lane.child(
                        div()
                            .id(("lane-mute", i * 16 + t))
                            .tab_stop(true)
                            .border_1()
                            .border_color(rgb(if edit.mute {
                                theme::BORDER_STRONG
                            } else {
                                theme::BORDER
                            }))
                            .focus_visible(|s| s.border_color(rgb(theme::RING)))
                            .role(Role::Button)
                            .aria_label(if edit.mute { "Unmute track" } else { "Mute track" })
                            .absolute()
                            .top(px(4.))
                            .left(px(x0 + 4.0))
                            .w(px(theme::CONTROL_H))
                            .h(px(theme::CONTROL_H))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(theme::RADIUS_SM))
                            .bg(if edit.mute {
                                rgb(theme::SURFACE_RAISED)
                            } else {
                                rgba(theme::SCRIM)
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
                            .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
                            // The strip scrubs on press; this button must not.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                icon(
                                    if edit.mute { "volume-x" } else { "volume-2" },
                                    theme::FG_SOFT,
                                )
                                .size(px(14.)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_track(i, t, cx, |e| e.mute = !e.mute);
                            })),
                    );
                }
            }
            if let Some((_, i, a, b)) = self.range.filter(|r| r.0 == t && r.2 != r.3)
                && i < self.timeline.pieces.len()
            {
                let start = self.to_disp(self.timeline.start_of(i));
                let (x0, x1) = (xd(start + a.min(b)), xd(start + a.max(b)));
                lane = lane.child(
                    div()
                        .absolute()
                        .left(px(x0))
                        .w(px((x1 - x0).max(1.0)))
                        .h_full()
                        .bg(rgb(theme::BRAND))
                        .border_1()
                        .border_color(rgb(theme::BRAND))
                        .opacity(0.28),
                );
            }
            let listened = audible;
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
                            .gap_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .child(
                                icon(
                                    "volume-2",
                                    if listened { theme::BRAND } else { theme::MUTED_FG },
                                )
                                .size(px(14.)),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(if listened {
                                        theme::FG
                                    } else {
                                        theme::FG_SOFT
                                    }))
                                    .child(
                                        self.sources
                                            .first()
                                            .map_or_else(|| track_name(t), |s| track_label(s, t)),
                                    ),
                            )
                            .children((t == 0).then(|| {
                                let hint = if !self.tracks_open || tracks <= 1 {
                                    "whole mix, plays by default"
                                } else if self.solo == Some(0) {
                                    "soloed \u{00b7} original mix"
                                } else {
                                    "bypassed in preview \u{00b7} rebuilt on export"
                                };
                                div().text_color(rgb(theme::MUTED_FG)).child(hint)
                            })),
                    )
                    .child(lane),
            );
        }

        let ph_x = x_of(self.player.playhead);
        let playhead_line =
            div().absolute().top_0().h_full().left(px(ph_x - 1.0)).w(px(2.)).bg(rgb(theme::FG));
        let playhead_cap = div()
            .absolute()
            .top(px(-3.))
            .left(px(ph_x - 4.0))
            .w(px(8.))
            .h(px(6.))
            .rounded(px(2.))
            .bg(rgb(theme::FG));

        // The picked range: a brand tint across every lane, a draggable edge each side, and a
        // label with its start, end and length.
        let sel_overlay = self.sel.filter(|(a, b)| a != b).map(|(a, b)| {
            let (a, b) = (a.min(b), a.max(b));
            let (x0, x1) = (x_of(a), x_of(b));
            let w = (x1 - x0).max(1.0);
            let edge = |left: bool| {
                // Dragging an edge keeps the other one fixed.
                let fixed = if left { b } else { a };
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(8.))
                    .left(px(if left { -4.0 } else { w - 4.0 }))
                    .flex()
                    .justify_center()
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.begin_range(fixed);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(div().w(px(2.)).h_full().bg(rgb(theme::BRAND)))
            };
            div()
                .absolute()
                .top_0()
                .h_full()
                .left(px(x0))
                .w(px(w))
                .child(div().size_full().bg(rgb(theme::BRAND)).opacity(0.22))
                .child(edge(true))
                .child(edge(false))
                .child(
                    div()
                        .absolute()
                        .top(px(3.))
                        .left(px(6.))
                        .px(px(6.))
                        .py(px(1.))
                        .whitespace_nowrap()
                        .rounded(px(theme::RADIUS_SM))
                        .bg(rgba(theme::SCRIM))
                        .border_1()
                        .border_color(rgb(theme::BRAND_BORDER))
                        .text_size(px(theme::TEXT_MICRO))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::FG))
                        .child(format!(
                            "{} \u{2013} {}  \u{00b7}  {}",
                            clock(a),
                            clock(b),
                            model::secs_label(b - a)
                        )),
                )
        });

        // The Split tool's razor: a line at the mouse across every lane with its time and a
        // scissors, red and faint where a click would not split (too near an edge, a gap, or
        // nothing there).
        let razor = self
            .hover
            .filter(|_| {
                self.tool == TimelineTool::Split
                    && self.trim.is_none()
                    && self.sel_anchor.is_none()
                    && self.editing
            })
            .map(|hx| {
                let bx = f32::from(hx - self.strip.get().origin.x).clamp(0.0, width);
                let t = self.time_at(hx);
                let ok = self.can_split(t);
                let colour = if ok { theme::BRAND } else { theme::DANGER };
                let pill_x = if bx + 96.0 > width { bx - 90.0 } else { bx + 6.0 };
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .opacity(if ok { 1.0 } else { 0.6 })
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .h_full()
                            .left(px(bx - 0.75))
                            .w(px(1.5))
                            .bg(rgb(colour)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(3.))
                            .left(px(pill_x))
                            .px(px(5.))
                            .py(px(1.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .whitespace_nowrap()
                            .rounded(px(theme::RADIUS_SM))
                            .bg(rgba(theme::SCRIM))
                            .border_1()
                            .border_color(rgb(colour))
                            .text_size(px(theme::TEXT_MICRO))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(colour))
                            .child(icon("scissors", colour).size(px(11.)))
                            .child(clock(t)),
                    )
            });

        let strip = div()
            .id("editor-strip")
            .aria_label("Timeline")
            .relative()
            .flex()
            .flex_col()
            .gap_2()
            .w_full();
        let strip = if self.tool == TimelineTool::Split {
            strip.cursor_crosshair()
        } else {
            strip.cursor_pointer()
        };
        let strip = strip
            .child(video_lane)
            .children(lanes)
            .children(sel_overlay)
            .children(razor)
            .child(playhead_line)
            .child(playhead_cap)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.hover.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let t = this.time_at(ev.position.x);
                    if ev.modifiers.alt {
                        // Alt+drag picks a range, over pieces and gaps alike.
                        let at = this.snapped_time_at(ev.position.x);
                        this.begin_range(at);
                        cx.notify();
                        return;
                    }
                    this.sel = None;
                    if this.tool == TimelineTool::Split {
                        this.click_split(t, cx);
                    } else if ev.modifiers.shift {
                        // Shift keeps the old press-and-drag scrub.
                        this.scrubbing = true;
                        this.seek(t, cx);
                    } else {
                        // A click selects; moving 4 px or more turns the press into a reorder.
                        this.seek(t, cx);
                        this.moving = this.timeline.locate(t).map(|(from, _)| MoveDrag {
                            from,
                            x0: f32::from(ev.position.x),
                            active: false,
                            dx: 0.0,
                            target: from,
                        });
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.tool == TimelineTool::Split {
                    this.hover = Some(ev.position.x);
                    cx.notify();
                }
                if this.sel_anchor.is_some() && ev.dragging() {
                    this.drag_range(ev.position.x);
                    cx.notify();
                } else if this.drag.is_some() && ev.dragging() {
                    this.drag_point(ev);
                    cx.notify();
                } else if this.trim.is_some() && ev.dragging() {
                    this.drag_edge(ev);
                    cx.notify();
                } else if this.moving.is_some() && ev.dragging() {
                    this.drag_move(ev);
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
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.release(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.release(cx)));

        let toggle = (tracks > 1).then(|| {
            button_ghost(
                "tracks-toggle",
                if self.tracks_open {
                    "Hide other tracks".to_owned()
                } else {
                    format!("Edit {} tracks separately", tracks - 1)
                },
            )
            .text_size(px(theme::TEXT_CAPTION))
            .child(icon("sliders-horizontal", theme::FG_SOFT).size(px(14.)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.tracks_open = !this.tracks_open;
                // Entering separate-tracks mode plays all individual tracks mixed together (not
                // Master); clear any prior solo so the mix is heard immediately.
                this.solo = None;
                this.restart_if_playing(cx);
                cx.notify();
            }))
        });

        let shortcut_hint = |key: &'static str, action: &'static str| {
            div().flex().flex_row().items_center().gap_1().child(kbd(key)).child(
                div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::MUTED_FG))
                    .child(action),
            )
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(theme::section_heading("Timeline"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .ml_1()
                            .child(shortcut_hint("Space", "Play"))
                            .child(shortcut_hint("V", "Select"))
                            .child(shortcut_hint("S", "Split"))
                            .child(shortcut_hint("J", "Join"))
                            .child(shortcut_hint("Alt+\u{2190}\u{2192}", "Move"))
                            .child(shortcut_hint("Ctrl+Z", "Undo")),
                    )
                    .child(div().flex_1())
                    .children(toggle),
            )
            .child(head)
            .child(strip)
            .child(
                div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .line_height(px(17.))
                    .text_color(rgb(theme::MUTED_FG))
                    .child(
                        "Drag a piece to reorder it (Shift+drag scrubs) \u{00b7} Alt+drag picks a range: Delete cuts it, G makes it a gap \u{00b7} Drag a lane and press Delete to silence a stretch \u{00b7} Click the volume line to add a point",
                    ),
            )
    }

    /// The selected piece: where it comes from, order, and each audio track's volume and mute.
    fn inspector(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.selected?;
        let piece = self.timeline.pieces.get(i)?;
        let source = &self.sources[piece.source];
        let mut list = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .px_2()
                                    .py(px(2.))
                                    .rounded(px(theme::RADIUS_SM))
                                    .bg(rgb(theme::BRAND_SOFT))
                                    .border_1()
                                    .border_color(rgb(theme::BRAND_BORDER))
                                    .text_size(px(theme::TEXT_CAPTION))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::BRAND))
                                    .child(self.timeline.title(i)),
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .text_size(px(theme::TEXT_CAPTION))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(theme::FG_SOFT))
                                    .child(model::secs_label(piece.span())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(theme::FG_SOFT))
                                    .child(if piece.gap {
                                        "Black and silent".to_owned()
                                    } else {
                                        source.name.clone()
                                    }),
                            )
                            .children((!piece.gap).then(|| {
                                div().flex_none().text_color(rgb(theme::MUTED_FG)).child(format!(
                                    "{} \u{2013} {}",
                                    model::secs_label(piece.a),
                                    model::secs_label(piece.b)
                                ))
                            })),
                    ),
            )
            .child({
                let can_earlier = i > 0;
                let can_later = i + 1 < self.timeline.pieces.len();
                let piece_row = div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        button("piece-left", "Earlier")
                            .aria_label("Move earlier")
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .gap_1()
                            .text_size(px(theme::TEXT_CAPTION))
                            .flex_row_reverse()
                            .opacity(if can_earlier { 1.0 } else { 0.45 })
                            .child(icon("chevron-left", theme::FG_SOFT).size(px(14.)))
                            .on_click(cx.listener(|this, _, _, cx| this.shift_selected(-1, cx))),
                    )
                    .child(
                        button("piece-right", "Later")
                            .aria_label("Move later")
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .gap_1()
                            .text_size(px(theme::TEXT_CAPTION))
                            .opacity(if can_later { 1.0 } else { 0.45 })
                            .child(icon("chevron-right", theme::FG_SOFT).size(px(14.)))
                            .on_click(cx.listener(|this, _, _, cx| this.shift_selected(1, cx))),
                    )
                    .child(
                        button("piece-join", "Join")
                            .aria_label("Join with next")
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .on_click(cx.listener(|this, _, _, cx| this.join_selected(cx))),
                    );
                let history_row = div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        button("undo", "Undo")
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .opacity(if self.undo.is_empty() { 0.45 } else { 1.0 })
                            .on_click(cx.listener(|this, _, _, cx| this.step_history(false, cx))),
                    )
                    .child(
                        button("redo", "Redo")
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .opacity(if self.redo.is_empty() { 0.45 } else { 1.0 })
                            .on_click(cx.listener(|this, _, _, cx| this.step_history(true, cx))),
                    );
                div().flex().flex_col().gap_2().child(piece_row).child(history_row)
            })
            .child(div().h(px(1.)).w_full().bg(rgb(theme::BORDER_SUBTLE)));

        let shown = if piece.gap {
            0
        } else if self.tracks_open {
            piece.tracks.len()
        } else {
            piece.tracks.len().min(1)
        };
        if shown > 0 {
            list = list.child(
                div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::FG_SOFT))
                    .child("Audio"),
            );
        }
        let multi_track = self.tracks_open && piece.tracks.len() > 1;
        for (t, edit) in piece.tracks.iter().enumerate().take(shown) {
            let soloed = multi_track && self.solo == Some(t);
            let in_mix = if multi_track {
                self.solo.is_none() && t > 0 && !edit.mute
            } else {
                t == 0 && !edit.mute
            };
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::SURFACE_INSET))
                    .border_1()
                    .border_color(rgb(if soloed {
                        theme::BRAND_BORDER
                    } else {
                        theme::BORDER_SUBTLE
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(theme::TEXT_BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(if soloed || in_mix {
                                        theme::FG
                                    } else {
                                        theme::MUTED_FG
                                    }))
                                    .child(track_label(source, t)),
                            )
                            .children(multi_track.then(|| {
                                if soloed {
                                    div()
                                        .id(("listen-solo", t))
                                        .tab_stop(true)
                                        .role(Role::Button)
                                        .aria_label("Stop soloing track")
                                        .h(px(theme::CONTROL_H))
                                        .px(px(10.))
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(6.))
                                        .rounded(px(theme::RADIUS_MD))
                                        .bg(rgb(theme::BRAND_SOFT))
                                        .border_1()
                                        .border_color(rgb(theme::BRAND_BORDER))
                                        .focus_visible(|s| s.border_color(rgb(theme::RING)))
                                        .text_size(px(theme::TEXT_CAPTION))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(theme::BRAND))
                                        .cursor_pointer()
                                        .hover(|s| s.border_color(rgb(theme::BRAND)))
                                        .active(|s| s.opacity(0.85))
                                        .child(icon("headphones", theme::BRAND).size(px(14.)))
                                        .child("Listening")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.set_solo(None, cx)),
                                        )
                                        .into_any_element()
                                } else {
                                    button_ghost(("listen", t), "Listen")
                                        .px(px(10.))
                                        .text_size(px(theme::TEXT_CAPTION))
                                        .child(icon("headphones", theme::FG_SOFT).size(px(14.)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_solo(Some(t), cx)
                                        }))
                                        .into_any_element()
                                }
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_2()
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
                                "Mute",
                                edit.mute,
                                true,
                                cx.listener(move |this, on: &bool, _, cx| {
                                    let on = *on;
                                    this.edit_track(i, t, cx, |e| e.mute = on);
                                }),
                            )),
                    ),
            );
        }
        Some(
            list.child(
                div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .line_height(px(17.))
                    .text_color(rgb(theme::MUTED_FG))
                    .child(if piece.gap {
                        "Drag the gap's edges on the timeline to change its length."
                    } else {
                        "Muting here applies to this piece only."
                    }),
            )
            .into_any_element(),
        )
    }

    /// Export CTA, output choice, thin progress bar, and success/failure states.
    fn finish_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let running = matches!(self.export, Export::Running(_));
        let mut done = button_primary_if(
            "editor-done",
            if running { "Exporting\u{2026}" } else { "Export" },
            !running,
        )
        .w_full()
        .h(px(theme::CONTROL_H_LG))
        .child(icon("download", if running { theme::TEXT_DIM } else { theme::PRIMARY_FG }));
        if !running {
            done = done.on_click(cx.listener(|this, _, _, cx| this.done(cx)));
        }
        let mut out = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(theme::section_heading("Export"))
            .child(done);
        if self.finish != FinishChoice::Ask {
            out = out.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(muted(if self.finish == FinishChoice::SaveAs {
                        "Saves one new file."
                    } else {
                        "Saves each piece separately."
                    }))
                    .child(
                        button_ghost("editor-ask", "Ask each time")
                            .px_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.finish = FinishChoice::Ask;
                                this.remember_prefs(cx, |e| e.finish = FinishChoice::Ask);
                                cx.notify();
                            })),
                    ),
            );
        }
        if self.finishing {
            out = out.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::SURFACE_INSET))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG_SOFT))
                            .child("How should this export?"),
                    )
                    .child(
                        button_primary("choose-save", "Save as a new file").w_full().on_click(
                            cx.listener(|this, _, _, cx| this.choose(FinishChoice::SaveAs, cx)),
                        ),
                    )
                    .child(
                        button("choose-extract", "Extract pieces, keep editing")
                            .w_full()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose(FinishChoice::Extract, cx)
                            })),
                    )
                    .child(
                        button_ghost("choose-cancel", "Cancel")
                            .w_full()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.finishing = false;
                                cx.notify();
                            })),
                    )
                    .child(muted(
                        "Save joins pieces in order. Extract writes one file per piece and keeps your timeline.",
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
                    )),
            );
        }
        let status: Option<AnyElement> = match &self.export {
            Export::Idle => None,
            Export::Running(label) => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::SURFACE_INSET))
                    .border_1()
                    .border_color(rgb(theme::BORDER_SUBTLE))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .child(status_dot(rgb(theme::BRAND)))
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(4.))
                            .rounded_full()
                            .bg(rgb(theme::MUTED))
                            .overflow_hidden()
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(0.45))
                                    .rounded_full()
                                    .bg(rgb(theme::BRAND)),
                            ),
                    )
                    .child(muted("Long clips take a moment when audio is edited."))
                    .into_any_element(),
            ),
            Export::Done { files, took } => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::BRAND_SOFT))
                    .border_1()
                    .border_color(rgb(theme::BRAND_BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .child(icon("check", theme::BRAND).size(px(14.)))
                            .child(div().flex_1().min_w_0().child(format!(
                                "Saved {} in {:.1} s.",
                                if files.len() == 1 {
                                    files[0].file_name().map_or_else(String::new, |n| {
                                        n.to_string_lossy().into_owned()
                                    })
                                } else {
                                    format!("{} clips", files.len())
                                },
                                took.as_secs_f64()
                            ))),
                    )
                    .child(
                        button("editor-show", "Show in folder")
                            .w_full()
                            .text_size(px(theme::TEXT_CAPTION))
                            .child(icon("folder-open", theme::FG_SOFT).size(px(14.)))
                            .on_click({
                                let file = files.first().cloned();
                                cx.listener(move |_, _, _, _| {
                                    if let Some(file) = &file
                                        && let Err(error) = zc_win::shell::reveal(file)
                                    {
                                        tracing::warn!(%error, "could not show the file");
                                    }
                                })
                            }),
                    )
                    .into_any_element(),
            ),
            Export::Failed(error) => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::DANGER_SOFT))
                    .border_1()
                    .border_color(rgb(theme::DANGER_BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::DANGER))
                            .child(icon("x", theme::DANGER).size(px(14.)))
                            .child("Export failed"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .text_color(rgb(theme::FG_SOFT))
                            .child(error.clone()),
                    )
                    .into_any_element(),
            ),
        };
        out.children(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_tracks_preview_excludes_master_and_mixes_individual_tracks() {
        let mut tracks = vec![TrackEdit::default(); 3];
        tracks[1].gain_db = -3.0;
        tracks[2].mute = true;

        // Simple mode plays Master (track 0) only.
        let (simple, solo) = build_piece_sounds(&tracks, Hns::ZERO, false, None);
        assert_eq!(solo, None);
        assert_eq!(simple.len(), 1);
        assert_eq!(simple[0].track, 0);

        // Separate-tracks mode without solo excludes Master (track 0) and returns tracks 1..
        let (mixed, solo) = build_piece_sounds(&tracks, Hns::ZERO, true, None);
        assert_eq!(solo, None);
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed[0].track, 1);
        assert_eq!(mixed[0].gain_db, -3.0);
        assert!(!mixed[0].mute);
        assert_eq!(mixed[1].track, 2);
        assert!(mixed[1].mute);

        // Solo plays only the soloed track.
        let (solo_sounds, solo) = build_piece_sounds(&tracks, Hns::ZERO, true, Some(1));
        assert_eq!(solo, Some(1));
        assert_eq!(solo_sounds.len(), 1);
        assert_eq!(solo_sounds[0].track, 1);
    }

    #[test]
    fn stop_button_is_disabled_when_empty_or_stopped_at_zero() {
        assert!(!can_stop(false, false, Hns::ZERO));
        assert!(!can_stop(false, true, Hns::from_secs(3)));
        assert!(!can_stop(true, false, Hns::ZERO));
        assert!(!can_stop(true, false, Hns::from_secs(-1)));
        assert!(can_stop(true, true, Hns::ZERO));
        assert!(can_stop(true, true, Hns::from_secs(3)));
        assert!(can_stop(true, false, Hns::from_millis(1)));
    }
}
