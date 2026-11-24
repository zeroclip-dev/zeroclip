//! The Editor tab (Phase 5): open a clip, trim it on keyframes, edit its audio tracks, and save
//! the result as a copy. Video is always stream-copied; only edited audio tracks are re-encoded
//! (D38). Playback is external (D22): "Preview" opens ffplay or the default player.
//!
//! Every ffmpeg call runs on a background thread. The view polls nothing: the task that started
//! the job updates the view when the job returns.
//!
//! ponytail: no thumbnail strip yet (P5.13); the timeline shows a tick per keyframe.

mod model;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Bounds, Context, FocusHandle, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, canvas, div, img, px, rgb,
};
use zc_core::Hns;
use zc_editor::ffmpeg::{Ffmpeg, FfmpegError, ProbeInfo};
use zc_editor::waveform::{file_key, rms_peaks};
use zc_store::Store;

use self::model::{CutMode, Handle, Plan, TrackEdit};
use super::components::{
    button::{button, button_ghost, button_primary},
    card::card,
    field_row::field_row,
    labeled_toggle::labeled_toggle,
    number_stepper::{Stepper, number_stepper},
    segmented::segmented,
    text_field::text_field,
};
use super::{human_duration, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

/// Peaks stored per waveform, and the sample rate they are computed from (P5.10).
const BUCKETS: usize = 1_000;
const SAMPLE_RATE: u32 = 8_000;
/// Bar width before the first paint has measured the real one.
const FALLBACK_WIDTH: f32 = 720.0;
const RULER_HEIGHT: f32 = 72.0;
const LANE_HEIGHT: f32 = 40.0;
/// One waveform bar and one keyframe tick are at least this far from the previous one.
const BAR_STEP: f32 = 3.0;
const TICK_GAP: f32 = 3.0;
const FRAME_HEIGHT: f32 = 300.0;
static FRAME_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

enum Load {
    Loading,
    Failed(String),
    Ready,
}

enum Export {
    Idle,
    Running,
    Done { file: PathBuf, took: Duration },
    Failed(String),
}

/// The clip being edited.
struct Clip {
    path: PathBuf,
    name: String,
    load: Load,
    info: Option<ProbeInfo>,
    in_pt: Hns,
    out_pt: Hns,
    edits: Vec<TrackEdit>,
    /// One per audio track; `None` until computed, or if the track has no usable audio.
    waves: Vec<Option<Vec<f32>>>,
    /// The video frame shown above the timeline, and the time it was taken at.
    frame: Option<PathBuf>,
    /// Where the preview should be. A new frame is made when this differs from what is shown.
    frame_want: Hns,
    frame_busy: bool,
}

pub struct EditorView {
    app: WeakEntity<ZeroClipApp>,
    ffmpeg_path: Option<PathBuf>,
    ffmpeg: Option<Ffmpeg>,
    store: Option<Arc<Mutex<Store>>>,
    /// The recording's AAC bitrate, reused when an edited track is re-encoded.
    bitrate_bps: u32,
    clip: Option<Clip>,
    /// Bumped by `open`, so a slow job for a clip that was replaced is ignored.
    generation: u64,
    /// The ruler's bounds from the last paint, to turn mouse positions into times.
    ruler: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<Handle>,
    picking: bool,
    export: Export,
    /// Cut fields, two per track; made on render.
    focus: Vec<FocusHandle>,
}

impl EditorView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            ffmpeg_path: None,
            ffmpeg: None,
            store: None,
            bitrate_bps: 160_000,
            clip: None,
            generation: 0,
            ruler: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            picking: false,
            export: Export::Idle,
            focus: Vec::new(),
        }
    }

    /// Opens `path`: probes it, then computes its waveforms, both off the UI thread.
    pub fn open(
        &mut self,
        path: PathBuf,
        ffmpeg_path: Option<PathBuf>,
        store: Option<Arc<Mutex<Store>>>,
        bitrate_bps: u32,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        self.ffmpeg_path = ffmpeg_path.clone();
        self.store = store;
        self.bitrate_bps = bitrate_bps;
        self.picking = false;
        self.drag = None;
        self.export = Export::Idle;
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.clip = Some(Clip {
            path: path.clone(),
            name,
            load: Load::Loading,
            info: None,
            in_pt: Hns::ZERO,
            out_pt: Hns::ZERO,
            edits: Vec::new(),
            waves: Vec::new(),
            frame: None,
            frame_want: Hns::ZERO,
            frame_busy: false,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let ffmpeg = Ffmpeg::locate(ffmpeg_path.as_deref())?;
                    let info = ffmpeg.probe(&path)?;
                    Ok::<_, FfmpegError>((ffmpeg, info))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.probed(generation, result, cx));
        })
        .detach();
    }

    fn probed(
        &mut self,
        generation: u64,
        result: Result<(Ffmpeg, ProbeInfo), FfmpegError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        let Some(clip) = self.clip.as_mut() else { return };
        match result {
            Ok((ffmpeg, info)) => {
                let tracks = usize::from(info.stream.audio_tracks);
                clip.out_pt = info.duration;
                clip.edits = vec![TrackEdit::default(); tracks];
                clip.waves = vec![None; tracks];
                clip.info = Some(info);
                clip.load = Load::Ready;
                for track in 0..tracks {
                    self.spawn_waveform(generation, &ffmpeg, track, cx);
                }
                self.ffmpeg = Some(ffmpeg);
                self.request_frame(Hns::ZERO, cx);
            }
            Err(error) => {
                let hint = if matches!(error, FfmpegError::NotFound) {
                    " Set its path in Settings, Advanced, Editor."
                } else {
                    ""
                };
                clip.load = Load::Failed(format!("{error}.{hint}"));
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
        track: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.clip.as_ref().map(|c| c.path.clone()) else { return };
        let (ffmpeg, store) = (ffmpeg.clone(), self.store.clone());
        cx.spawn(async move |this, cx| {
            let peaks = cx
                .background_executor()
                .spawn(async move { waveform(&ffmpeg, store.as_deref(), &path, track) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation == this.generation
                    && let Some(slot) = this.clip.as_mut().and_then(|c| c.waves.get_mut(track))
                {
                    *slot = peaks;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The mouse is at window x `x` over the ruler: start (`press`) or continue a handle drag.
    fn pointer(&mut self, x: Pixels, press: bool, cx: &mut Context<Self>) {
        let bounds = self.ruler.get();
        let width = f32::from(bounds.size.width);
        let Some(clip) = self.clip.as_mut() else { return };
        let Some(duration) = clip.info.as_ref().map(|i| i.duration) else { return };
        if width <= 0.0 {
            return;
        }
        let x = f32::from(x - bounds.origin.x);
        let t = model::x_to_time(x, duration, width);
        if press {
            let (in_x, out_x) = (
                model::time_to_x(clip.in_pt, duration, width),
                model::time_to_x(clip.out_pt, duration, width),
            );
            self.drag = Some(model::nearest_handle(x, in_x, out_x));
        }
        if let Some(handle) = self.drag {
            (clip.in_pt, clip.out_pt) =
                model::move_handle(handle, t, clip.in_pt, clip.out_pt, duration);
            let shown = if handle == Handle::In { clip.in_pt } else { clip.out_pt };
            self.request_frame(shown, cx);
            cx.notify();
        }
    }

    fn set_handle(&mut self, handle: Handle, secs: f64, cx: &mut Context<Self>) {
        if let Some(clip) = self.clip.as_mut()
            && let Some(duration) = clip.info.as_ref().map(|i| i.duration)
        {
            (clip.in_pt, clip.out_pt) = model::move_handle(
                handle,
                Hns::from_secs_f64(secs),
                clip.in_pt,
                clip.out_pt,
                duration,
            );
            let shown = if handle == Handle::In { clip.in_pt } else { clip.out_pt };
            self.request_frame(shown, cx);
            cx.notify();
        }
    }

    /// Shows the video frame at `at`. One ffmpeg job runs at a time; while it runs, only the
    /// latest wanted time is kept, so dragging a handle never queues a pile of jobs.
    fn request_frame(&mut self, at: Hns, cx: &mut Context<Self>) {
        let (Some(clip), Some(ffmpeg)) = (self.clip.as_mut(), self.ffmpeg.clone()) else { return };
        clip.frame_want = at;
        if clip.frame_busy {
            return;
        }
        clip.frame_busy = true;
        let (generation, path) = (self.generation, clip.path.clone());
        let out = std::env::temp_dir().join(format!(
            "zeroclip-editor-{}-{}.jpg",
            std::process::id(),
            FRAME_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        cx.spawn(async move |this, cx| {
            let target = out.clone();
            let done = cx
                .background_executor()
                .spawn(async move {
                    ffmpeg.thumbnail(&path, at, 960, &target).is_ok() && target.is_file()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    let _ = std::fs::remove_file(&out);
                    return;
                }
                let Some(clip) = this.clip.as_mut() else { return };
                clip.frame_busy = false;
                if done && let Some(old) = clip.frame.replace(out) {
                    let _ = std::fs::remove_file(old);
                }
                if clip.frame_want != at {
                    let want = clip.frame_want;
                    this.request_frame(want, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn edit_track(&mut self, track: usize, cx: &mut Context<Self>, f: impl FnOnce(&mut TrackEdit)) {
        if let Some(edit) = self.clip.as_mut().and_then(|c| c.edits.get_mut(track)) {
            f(edit);
            cx.notify();
        }
    }

    /// The plan the current UI state describes.
    fn plan(&self) -> Result<Plan, String> {
        let clip = self.clip.as_ref().ok_or("No clip is open.")?;
        let info = clip.info.as_ref().ok_or("The clip hasn't loaded.")?;
        model::build_plan(
            info.duration,
            &info.keyframes,
            clip.in_pt,
            clip.out_pt,
            &clip.edits,
            self.bitrate_bps,
        )
    }

    /// "Save as copy": trim, then audio edits, in one background job.
    fn export(&mut self, cx: &mut Context<Self>) {
        if matches!(self.export, Export::Running) {
            return;
        }
        let plan = match self.plan() {
            Ok(plan) => plan,
            Err(message) => {
                self.export = Export::Failed(message);
                cx.notify();
                return;
            }
        };
        let (Some(clip), Some(ffmpeg)) = (self.clip.as_ref(), self.ffmpeg.clone()) else { return };
        let source = clip.path.clone();
        let output = model::edited_name(&source, |p| p.exists());
        self.export = Export::Running;
        cx.notify();
        let out = output.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { run_export(&ffmpeg, &source, &out, &plan) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.export = match result {
                    Ok(took) => Export::Done { file: output, took },
                    Err(error) => Export::Failed(error.to_string()),
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
            Export::Done { file, took } => (
                NoticeLevel::Info,
                format!(
                    "Saved {} in {:.1} s.",
                    file.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
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

    /// Plays the selection in ffplay if it sits beside ffmpeg, else opens the file in the default
    /// player (D22). Neither is waited for.
    fn preview(&mut self, cx: &mut Context<Self>) {
        let (Some(clip), Some(ffmpeg)) = (self.clip.as_ref(), self.ffmpeg.as_ref()) else { return };
        let Some(info) = clip.info.as_ref() else { return };
        let start = model::snapped_start(&info.keyframes, clip.in_pt);
        let ffplay = ffmpeg.exe.with_file_name(if cfg!(windows) { "ffplay.exe" } else { "ffplay" });
        let spawned = if ffplay.is_file() {
            std::process::Command::new(ffplay)
                .args(["-autoexit", "-ss"])
                .arg(format!("{:.3}", start.as_secs_f64()))
                .arg("-t")
                .arg(format!("{:.3}", (clip.out_pt - start).as_secs_f64()))
                .arg(&clip.path)
                .spawn()
        } else {
            std::process::Command::new("explorer").arg(&clip.path).spawn()
        };
        if let Err(error) = spawned {
            self.export = Export::Failed(format!("Couldn't start a player: {error}"));
            cx.notify();
        }
    }
}

/// Track `track`'s waveform: from the store (track 0 only), else decoded and, for track 0, stored.
fn waveform(
    ffmpeg: &Ffmpeg,
    store: Option<&Mutex<Store>>,
    path: &std::path::Path,
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

/// Runs `plan` into `output`, verifies the result opens, and returns how long it took. A failed
/// run leaves no half-written output behind.
fn run_export(
    ffmpeg: &Ffmpeg,
    source: &std::path::Path,
    output: &std::path::Path,
    plan: &Plan,
) -> Result<Duration, FfmpegError> {
    let started = Instant::now();
    let result = (|| {
        match (plan.trim, plan.has_audio_edits()) {
            (None, false) => ffmpeg.remux_for_export(source, output)?,
            (Some((start, end)), false) => ffmpeg.trim(source, start, end, output)?,
            (None, true) => ffmpeg.edit_audio(source, output, &plan.tracks)?,
            (Some((start, end)), true) => {
                let scratch = model::trim_scratch(output);
                let edited = ffmpeg
                    .trim(source, start, end, &scratch)
                    .and_then(|()| ffmpeg.edit_audio(&scratch, output, &plan.tracks));
                // The scratch file is ours; failing to delete it must not fail the export.
                let _ = std::fs::remove_file(&scratch);
                edited?;
            }
        }
        // Opening the output proves it is a real file before we report success (DESIGN §9).
        ffmpeg.probe(output).map(drop)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result.map(|()| started.elapsed())
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tracks = self.clip.as_ref().map_or(0, |c| c.edits.len());
        while self.focus.len() < tracks * 2 {
            self.focus.push(cx.focus_handle());
        }

        let mut page = div()
            .id("editor-page")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(self.header(cx));
        if self.picking || self.clip.is_none() {
            page = page.child(self.picker(cx));
        }
        if let Some(clip) = &self.clip {
            page = match &clip.load {
                Load::Loading => page.child(muted("Reading the clip.")),
                Load::Failed(error) => {
                    page.child(div().text_sm().text_color(rgb(theme::DANGER)).child(error.clone()))
                }
                Load::Ready => page
                    .child(self.frame_view(clip))
                    .child(self.timeline(clip, cx))
                    .child(self.selection_card(clip, cx))
                    .children(self.track_cards(clip, window, cx))
                    .child(self.export_card(clip, cx)),
            };
        }
        page
    }
}

fn muted(text: impl Into<SharedString>) -> gpui::Div {
    div().text_sm().text_color(rgb(theme::MUTED_FG)).child(text.into())
}

impl EditorView {
    fn header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let title = self.clip.as_ref().map_or_else(|| "Editor".to_owned(), |c| c.name.clone());
        let length = self
            .clip
            .as_ref()
            .and_then(|c| c.info.as_ref())
            .map(|i| format!("{} long", human_duration(i.duration)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
            .children(length.map(muted))
            .child(div().flex_1())
            .child(button("editor-pick", "Choose clip").on_click(cx.listener(|this, _, _, cx| {
                this.picking = !this.picking;
                cx.notify();
            })))
    }

    /// The video clips of the output folder; picking one opens it.
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
                .max_h(px(160.))
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
                            this.open(path, ffmpeg, store, bitrate, cx);
                        }))
                }))
                .into_any_element()
        };
        card("Choose a clip").child(body)
    }

    /// Keyframe ticks, the selection, the two handles and the snapped start, with one waveform
    /// lane per audio track under the ruler.
    /// The video frame at the handle you last moved.
    fn frame_view(&self, clip: &Clip) -> gpui::Div {
        let frame = div()
            .w_full()
            .h(px(FRAME_HEIGHT))
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        match &clip.frame {
            Some(path) => {
                frame.child(img(path.clone()).h_full().object_fit(gpui::ObjectFit::Contain))
            }
            None => frame.child(muted("Loading the picture.")),
        }
    }

    fn timeline(&self, clip: &Clip, cx: &mut Context<Self>) -> gpui::Div {
        let Some(info) = clip.info.as_ref() else { return div() };
        let measured = f32::from(self.ruler.get().size.width);
        let width = if measured > 0.0 { measured } else { FALLBACK_WIDTH };
        let duration = info.duration;
        let x_of = |t: Hns| model::time_to_x(t, duration, width);
        let snapped = model::snapped_start(&info.keyframes, clip.in_pt);

        let bounds = self.ruler.clone();
        let mut ruler = div()
            .id("editor-ruler")
            .relative()
            .w_full()
            .h(px(RULER_HEIGHT))
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .overflow_hidden()
            .cursor_pointer()
            .child(canvas(move |b, _, _| bounds.set(b), |_, (), _, _| {}).absolute().size_full())
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(px(x_of(snapped)))
                    .w(px((x_of(clip.out_pt) - x_of(snapped)).max(0.)))
                    .bg(rgb(theme::BRAND_SOFT)),
            );
        for x in model::tick_xs(&info.keyframes, duration, width, TICK_GAP) {
            ruler = ruler.child(
                div().absolute().bottom_0().h(px(16.)).left(px(x)).w(px(1.)).bg(rgb(theme::RING)),
            );
        }
        // The cut really starts on the keyframe, so that marker is the honest one.
        ruler = ruler
            .child(marker(x_of(snapped), theme::WARN, 2.))
            .child(marker(x_of(clip.in_pt), theme::BRAND, 4.))
            .child(marker(x_of(clip.out_pt), theme::BRAND, 4.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.pointer(ev.position.x, true, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if ev.dragging() {
                    this.pointer(ev.position.x, false, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None));

        let lanes = clip.waves.iter().enumerate().map(|(i, wave)| {
            let lane = div()
                .relative()
                .w_full()
                .h(px(LANE_HEIGHT))
                .rounded(px(theme::RADIUS_SM))
                .bg(rgb(theme::CARD))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_1()
                        .left_2()
                        .text_xs()
                        .text_color(rgb(theme::MUTED_FG))
                        .child(format!("Track {}", i + 1)),
                );
            match wave {
                Some(peaks) => {
                    let columns = model::downsample(peaks, (width / BAR_STEP) as usize);
                    let n = columns.len();
                    lane.children(columns.into_iter().enumerate().map(|(c, peak)| {
                        let h = (peak.min(1.0) * LANE_HEIGHT).max(1.0);
                        div()
                            .absolute()
                            .left(px((c as f32 + 0.5) / n as f32 * width))
                            .top(px((LANE_HEIGHT - h) / 2.0))
                            .w(px(2.))
                            .h(px(h))
                            .bg(rgb(theme::MUTED_FG))
                    }))
                }
                None => lane,
            }
        });

        div().flex().flex_col().gap_1().child(ruler).children(lanes)
    }

    fn selection_card(&self, clip: &Clip, cx: &mut Context<Self>) -> gpui::Div {
        let Some(info) = clip.info.as_ref() else { return div() };
        let snapped = model::snapped_start(&info.keyframes, clip.in_pt);
        let max = info.duration.as_secs_f64();
        let stepper = |id, value: Hns, handle: Handle, cx: &mut Context<Self>| {
            number_stepper(
                id,
                (value.as_secs_f64() * 10.0).round() / 10.0,
                Stepper { min: 0.0, max, step: 1.0, suffix: " s", enabled: true },
                cx.listener(move |this, v: &f64, _, cx| this.set_handle(handle, *v, cx)),
            )
        };
        let line = if snapped == clip.in_pt {
            format!("The cut starts at {}.", model::secs_label(snapped))
        } else {
            format!(
                "The cut starts at {}, the nearest keyframe before your in point.",
                model::secs_label(snapped)
            )
        };
        card("Selection")
            .child(field_row("In point", stepper("in-point", clip.in_pt, Handle::In, cx)))
            .child(field_row("Out point", stepper("out-point", clip.out_pt, Handle::Out, cx)))
            .child(muted(line))
            .child(muted(format!(
                "Keeps {} of {}. Cuts land on keyframes, so no video is re-encoded.",
                model::secs_label(clip.out_pt - snapped),
                model::secs_label(info.duration)
            )))
    }

    /// One card for every audio track, a row each: volume and mute. A track's cut range
    /// (silence or remove) stays folded until its "Cut a range" button is pressed, or it already has one.
    fn track_cards(&self, clip: &Clip, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if clip.edits.is_empty() {
            return Vec::new();
        }
        let mut list = card("Audio tracks");
        for (i, edit) in clip.edits.iter().enumerate() {
            let has_cut = !edit.cut_start.trim().is_empty() || !edit.cut_end.trim().is_empty();
            let open = edit.cut_open || has_cut;
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(64.)).text_sm().child(format!("Track {}", i + 1)))
                    .child(number_stepper(
                        format!("gain-{i}"),
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
                            this.edit_track(i, cx, |e| e.gain_db = v);
                        }),
                    ))
                    .child(labeled_toggle(
                        ("mute", i),
                        "Mute",
                        edit.mute,
                        true,
                        cx.listener(move |this, on: &bool, _, cx| {
                            let on = *on;
                            this.edit_track(i, cx, |e| e.mute = on);
                        }),
                    ))
                    .child(
                        div().ml_auto().child(
                            button_ghost(
                                ("cut-open", i),
                                if open { "Hide cut" } else { "Cut a range" },
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.edit_track(i, cx, |e| e.cut_open = !e.cut_open);
                                },
                            )),
                        ),
                    ),
            );
            if open {
                let cut_field = |slot: usize,
                                 id: &'static str,
                                 value: &str,
                                 ph: &str,
                                 cx: &mut Context<Self>| {
                    text_field(
                        (id, i),
                        &self.focus[i * 2 + slot],
                        window,
                        value,
                        ph,
                        96.,
                        cx.listener(move |this, v: &String, _, cx| {
                            let v = v.clone();
                            this.edit_track(i, cx, |e| {
                                if slot == 0 { e.cut_start = v } else { e.cut_end = v }
                            });
                        }),
                    )
                };
                let start = cut_field(0, "cut-start", &edit.cut_start, "Start, s", cx);
                let end = cut_field(1, "cut-end", &edit.cut_end, "End, s", cx);
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .pl(px(64.))
                        .child(start)
                        .child(end)
                        .child(segmented(
                            format!("cut-mode-{i}"),
                            vec!["Silence".into(), "Remove".into()],
                            Some(usize::from(edit.cut_mode == CutMode::Remove)),
                            true,
                            cx.listener(move |this, ix: &usize, _, cx| {
                                let mode =
                                    if *ix == 0 { CutMode::Silence } else { CutMode::Remove };
                                this.edit_track(i, cx, |e| e.cut_mode = mode);
                            }),
                        )),
                );
            }
        }
        vec![list
            .child(muted(
                "Cut times are seconds of the original clip. Silence keeps the length; remove shortens only that track.",
            ))
            .into_any_element()]
    }

    fn export_card(&self, clip: &Clip, cx: &mut Context<Self>) -> gpui::Div {
        let running = matches!(self.export, Export::Running);
        let target = model::edited_name(&clip.path, |p| p.exists());
        let target_name =
            target.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let save = button_primary("editor-export", if running { "Saving" } else { "Save as copy" });
        let save = if running {
            save.opacity(0.5)
        } else {
            save.on_click(cx.listener(|this, _, _, cx| this.export(cx)))
        };
        let preview = button("editor-preview", "Preview")
            .on_click(cx.listener(|this, _, _, cx| this.preview(cx)));
        let status: Option<AnyElement> = match &self.export {
            Export::Idle => None,
            Export::Running => Some(
                muted("Saving. Long clips take a moment when audio is edited.").into_any_element(),
            ),
            Export::Done { file, took } => Some(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(rgb(theme::BRAND))
                    .child(format!(
                        "Saved {} in {:.1} s.",
                        file.file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                        took.as_secs_f64()
                    ))
                    .child(button_ghost("editor-play-copy", "Play it").on_click({
                        let file = file.clone();
                        cx.listener(move |this, _, _, cx| {
                            if let Err(error) =
                                std::process::Command::new("explorer").arg(&file).spawn()
                            {
                                this.export =
                                    Export::Failed(format!("Couldn't open the file: {error}"));
                                cx.notify();
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
        card("Export")
            .child(muted(format!(
                "Saves a new file, {target_name}. The original is never changed."
            )))
            .child(div().flex().flex_row().gap_2().child(save).child(preview))
            .children(status)
    }
}

/// A vertical line on the ruler, `thickness` px wide, centred on `x`.
fn marker(x: f32, colour: u32, thickness: f32) -> gpui::Div {
    div().absolute().top_0().h_full().left(px(x - thickness / 2.0)).w(px(thickness)).bg(rgb(colour))
}
