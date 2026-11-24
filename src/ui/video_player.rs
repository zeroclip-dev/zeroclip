//! The video player: a plain player for watching a clip, apart from the editor. Play and pause,
//! a bar to seek by, 10-second skips (arrow keys), a speed choice, hold-the-picture for 2x,
//! volume boost and fullscreen. The decoding and sound are the editor's `Player` engine.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window, canvas, div,
    img, px, relative, rgb,
};
use zc_core::Hns;
use zc_editor::ffmpeg::{Ffmpeg, FfmpegError};

use super::components::{
    button::{button, button_ghost, button_primary},
    segmented::segmented,
};
use super::editor::player::{Item, Player, Sound};
use super::icons::icon;
use super::theme;
use crate::app::ZeroClipApp;

const TICK: Duration = Duration::from_millis(16);
/// Holding the picture this long speeds playback up.
const HOLD: Duration = Duration::from_millis(350);
/// What an arrow key skips.
const SKIP: Hns = Hns::from_secs(10);
const BOOST: f64 = 2.0;
const SPEEDS: [f64; 5] = [0.5, 1.0, 1.25, 1.5, 2.0];
/// Volume choices: label and the gain they add, in decibels.
const VOLUMES: [(&str, f64); 3] = [("100%", 0.0), ("200%", 6.0), ("400%", 12.0)];

pub struct VideoPlayerView {
    app: WeakEntity<ZeroClipApp>,
    path: Option<PathBuf>,
    name: String,
    ffmpeg: Option<Ffmpeg>,
    duration: Hns,
    /// Moments marked while the clip was recorded, earliest first.
    bookmarks: Vec<Hns>,
    /// The clip has sound to play.
    has_sound: bool,
    problem: Option<String>,
    loading: bool,
    /// Bumped on every open, so a slow probe of an earlier clip is ignored.
    generation: u64,
    player: Player,
    /// The speed picked with the buttons.
    speed: f64,
    volume: usize,
    /// When the mouse went down on the picture, while it is still down.
    hold: Option<Instant>,
    /// Playing at the hold speed.
    boosted: bool,
    ticking: bool,
    fullscreen: bool,
    focus: Option<FocusHandle>,
    bar: Rc<Cell<Bounds<gpui::Pixels>>>,
    /// The seek bar is being dragged.
    scrubbing: bool,
}

impl VideoPlayerView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            path: None,
            name: String::new(),
            ffmpeg: None,
            duration: Hns::ZERO,
            bookmarks: Vec::new(),
            has_sound: false,
            problem: None,
            loading: false,
            generation: 0,
            player: Player::default(),
            speed: 1.0,
            volume: 0,
            hold: None,
            boosted: false,
            ticking: false,
            fullscreen: false,
            focus: None,
            bar: Rc::new(Cell::new(Bounds::default())),
            scrubbing: false,
        }
    }

    /// Opens `path` and starts playing it.
    pub fn open(&mut self, path: PathBuf, ffmpeg_path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.player.reset();
        self.generation += 1;
        let generation = self.generation;
        self.name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.path = Some(path.clone());
        self.problem = None;
        self.loading = true;
        self.duration = Hns::ZERO;
        self.bookmarks = zc_core::bookmarks::load(&path);
        self.boosted = false;
        self.hold = None;
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
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok((ffmpeg, info)) => {
                        this.duration = info.duration;
                        this.has_sound = info.stream.audio_tracks > 0;
                        this.ffmpeg = Some(ffmpeg);
                        this.play_from(Hns::ZERO, cx);
                    }
                    Err(error) => this.problem = Some(format!("Couldn't open this clip: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stops playback, picture and sound (when the user leaves this screen).
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.player.pause();
            cx.notify();
        }
    }

    fn effective_speed(&self) -> f64 {
        if self.boosted { BOOST } else { self.speed }
    }

    /// Plays from `at`, at the current speed and volume.
    fn play_from(&mut self, at: Hns, cx: &mut Context<Self>) {
        let (Some(ffmpeg), Some(path)) = (self.ffmpeg.as_ref(), self.path.clone()) else {
            return;
        };
        let at = at.clamp(Hns::ZERO, self.duration);
        let sound = self.has_sound.then(|| Sound {
            // Track 0 is the Master mix, which has everything that was recorded.
            track: 0,
            gain_db: VOLUMES[self.volume].1,
            line: None,
        });
        self.player.speed = self.effective_speed();
        self.player.play(ffmpeg, vec![Item { path, from: at, to: self.duration, sound }], at);
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Plays or pauses. At the very end it starts over.
    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.player.pause();
            cx.notify();
        } else if self.ffmpeg.is_some() {
            let at = self.player.playhead;
            let at = if self.duration - at < Hns::from_millis(200) { Hns::ZERO } else { at };
            self.play_from(at, cx);
        }
    }

    /// Moves to `t`: keeps playing if it was, else shows that frame.
    fn seek(&mut self, t: Hns, cx: &mut Context<Self>) {
        let t = t.clamp(Hns::ZERO, self.duration);
        if self.player.playing {
            self.play_from(t, cx);
        } else if let (Some(ffmpeg), Some(path)) = (self.ffmpeg.as_ref(), self.path.as_ref()) {
            self.player.show_at(ffmpeg, path, t, t);
            self.ensure_ticking(cx);
            cx.notify();
        }
    }

    /// Jumps to the next bookmark after the playhead, or the last one before it.
    fn jump_to_bookmark(&mut self, forward: bool, cx: &mut Context<Self>) {
        let now = self.player.playhead;
        let target = if forward {
            self.bookmarks.iter().find(|b| **b > now + Hns::from_millis(500))
        } else {
            self.bookmarks.iter().rev().find(|b| **b + Hns::from_secs(1) < now)
        };
        if let Some(t) = target.copied() {
            self.seek(t, cx);
        }
    }

    /// Restarts at the same place so a new speed or volume takes effect.
    fn restart(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.play_from(self.player.playhead, cx);
        }
    }

    fn set_fullscreen(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.fullscreen = on;
        if window.is_fullscreen() != on {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

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
                        // Held long enough on the picture: play at the hold speed.
                        if let Some(since) = this.hold
                            && !this.boosted
                            && this.player.playing
                            && since.elapsed() >= HOLD
                        {
                            this.boosted = true;
                            this.restart(cx);
                        }
                        let busy = this.player.tick();
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

    fn key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            return;
        }
        let now = self.player.playhead;
        match k.key.as_str() {
            "space" | "k" => self.toggle(cx),
            "right" | "l" => self.seek(now + SKIP, cx),
            "left" | "j" => self.seek(now - SKIP, cx),
            "f" => self.set_fullscreen(!self.fullscreen, window, cx),
            "escape" if self.fullscreen => self.set_fullscreen(false, window, cx),
            _ => {}
        }
    }

    fn seek_to_x(&mut self, x: gpui::Pixels, cx: &mut Context<Self>) {
        let bounds = self.bar.get();
        let width = f32::from(bounds.size.width).max(1.0);
        let frac = (f32::from(x - bounds.origin.x) / width).clamp(0.0, 1.0);
        self.seek(Hns::from_secs_f64(self.duration.as_secs_f64() * f64::from(frac)), cx);
    }
}

fn clock(t: Hns) -> String {
    let secs = t.as_secs_f64().max(0.0) as u64;
    if secs >= 3_600 {
        format!("{}:{:02}:{:02}", secs / 3_600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn speed_label(s: f64) -> SharedString {
    format!("{s}x").into()
}

impl Render for VideoPlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Frames that left the screen are removed from the GPU cache, or playback fills it.
        for old in self.player.spent.drain(..) {
            let _ = window.drop_image(old);
        }
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if !focus.contains_focused(window, cx) {
            window.focus(&focus, cx);
        }
        let playing = self.player.playing;
        let at = self.player.playhead.min(self.duration);
        let frac = if self.duration > Hns::ZERO {
            (at.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        let picture = div()
            .id("player-picture")
            .flex_1()
            .min_h_0()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(0x000000))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .relative()
            // Press and hold for the fast speed; a short click plays or pauses.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, _| this.hold = Some(Instant::now())),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    let held = this.hold.take();
                    if this.boosted {
                        this.boosted = false;
                        this.restart(cx);
                    } else if held.is_some() {
                        this.toggle(cx);
                    }
                }),
            );
        let picture = match (&self.player.shown, &self.problem) {
            (_, Some(problem)) => {
                picture.child(div().text_sm().text_color(rgb(theme::DANGER)).child(problem.clone()))
            }
            (Some(frame), _) => picture.child(
                img(gpui::ImageSource::Render(frame.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Contain),
            ),
            (None, _) => picture.child(
                div().text_sm().text_color(rgb(theme::MUTED_FG)).child(if self.loading {
                    "Opening the clip."
                } else {
                    "Nothing to play."
                }),
            ),
        };
        let picture = picture.children(self.boosted.then(|| {
            div()
                .absolute()
                .top_3()
                .right_3()
                .px_2()
                .py_1()
                .rounded(px(theme::RADIUS_SM))
                .bg(rgb(theme::BG))
                .text_sm()
                .child(format!("{BOOST}x"))
        }));

        let bar_bounds = self.bar.clone();
        let seek_bar = div()
            .id("player-bar")
            .relative()
            .w_full()
            .h(px(14.))
            .cursor_pointer()
            .child(
                canvas(move |b, _, _| bar_bounds.set(b), |_, (), _, _| {}).absolute().size_full(),
            )
            .child(
                div()
                    .absolute()
                    .top(px(5.))
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(theme::MUTED)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(5.))
                    .h(px(4.))
                    .w(relative(frac))
                    .rounded_full()
                    .bg(rgb(theme::BRAND)),
            )
            .children(self.bookmarks.iter().map(|b| {
                let at = (b.as_secs_f64() / self.duration.as_secs_f64().max(0.001)).clamp(0.0, 1.0);
                div()
                    .absolute()
                    .top(px(1.))
                    .left(relative(at as f32))
                    .ml(px(-1.))
                    .w(px(3.))
                    .h(px(12.))
                    .rounded_full()
                    .bg(rgb(theme::WARN))
            }))
            .child(
                div()
                    .absolute()
                    .top(px(1.))
                    .left(relative(frac))
                    .ml(px(-6.))
                    .w(px(12.))
                    .h(px(12.))
                    .rounded_full()
                    .bg(rgb(theme::FG)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.scrubbing = true;
                    this.seek_to_x(ev.position.x, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.scrubbing && ev.dragging() {
                    this.seek_to_x(ev.position.x, cx);
                } else {
                    this.scrubbing = false;
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.scrubbing = false),
            );

        let play = div()
            .id("player-play")
            .w(px(40.))
            .h(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::PRIMARY))
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            .child(icon(if playing { "pause" } else { "play" }, theme::PRIMARY_FG))
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)));
        let skip = |id: &'static str, label: &'static str, back: bool| {
            button_ghost(id, label).on_click(cx.listener(move |this, _, _, cx| {
                let now = this.player.playhead;
                this.seek(if back { now - SKIP } else { now + SKIP }, cx);
            }))
        };

        let controls =
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(play)
                .child(skip("player-back", "\u{2212}10 s", true))
                .child(skip("player-fwd", "+10 s", false))
                .children((!self.bookmarks.is_empty()).then(|| {
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(button_ghost("player-mark-prev", "\u{25c2} Bookmark").on_click(
                            cx.listener(|this, _, _, cx| this.jump_to_bookmark(false, cx)),
                        ))
                        .child(button_ghost("player-mark-next", "Bookmark \u{25b8}").on_click(
                            cx.listener(|this, _, _, cx| this.jump_to_bookmark(true, cx)),
                        ))
                }))
                .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                    "{} / {}",
                    clock(at),
                    clock(self.duration)
                )))
                .child(div().flex_1())
                .child(segmented(
                    "player-speed",
                    SPEEDS.iter().map(|s| speed_label(*s)).collect(),
                    SPEEDS.iter().position(|s| (*s - self.speed).abs() < 1e-9),
                    true,
                    cx.listener(|this, i: &usize, _, cx| {
                        this.speed = SPEEDS.get(*i).copied().unwrap_or(1.0);
                        this.restart(cx);
                    }),
                ))
                .child(segmented(
                    "player-volume",
                    VOLUMES.iter().map(|(l, _)| SharedString::from(*l)).collect(),
                    Some(self.volume),
                    true,
                    cx.listener(|this, i: &usize, _, cx| {
                        this.volume = (*i).min(VOLUMES.len() - 1);
                        this.restart(cx);
                    }),
                ))
                .child(button_ghost("player-fullscreen", "Fullscreen (F)").on_click(cx.listener(
                    |this, _, window, cx| this.set_fullscreen(!this.fullscreen, window, cx),
                )));

        let header = (!self.fullscreen).then(|| {
            let path = self.path.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().flex_1().truncate().text_lg().child(self.name.clone()))
                .children(path.map(|path| {
                    button_primary("player-edit", "Edit")
                        .child(icon("scissors", theme::PRIMARY_FG))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pause(cx);
                            let path = path.clone();
                            let _ =
                                this.app.update(cx, |app, cx| app.open_in_editor(path, false, cx));
                        }))
                }))
                .child(button("player-close", "Close").on_click(cx.listener(|this, _, _, cx| {
                    this.pause(cx);
                    let _ = this.app.update(cx, |app, cx| {
                        app.tab = crate::app::Tab::Clips;
                        cx.notify();
                    });
                })))
        });

        div()
            .id("video-player")
            .track_focus(&focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, window, cx| this.key(ev, window, cx)),
            )
            .children(header)
            .child(picture)
            .child(seek_bar)
            .child(controls)
            .child(div().text_xs().text_color(rgb(theme::MUTED_FG)).child(
                "Space plays, \u{2190} \u{2192} skip 10 seconds, F is fullscreen. Hold the picture for 2x.",
            ))
    }
}
