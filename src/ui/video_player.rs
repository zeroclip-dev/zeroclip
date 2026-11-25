//! The video player: a plain player for watching a clip, apart from the editor. Play and pause,
//! a bar to seek by, 10-second skips (arrow keys), a speed choice, hold-the-picture for 2x,
//! live volume/mute with optional audio track selection, and fullscreen. Video decoding uses the
//! editor's `Player` engine while audio runs through a live-scaled PCM pipe so volume and mute
//! changes apply immediately without restarting playback.

use std::cell::Cell;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Bounds, Context, FocusHandle, FontFeatures, FontWeight, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement as _, Styled,
    StyledImage as _, WeakEntity, Window, canvas, div, img, px, relative, rgb, rgba,
    transparent_black,
};
use zc_core::Hns;
use zc_core::settings::PlayerSettings;
use zc_editor::ffmpeg::{Ffmpeg, FfmpegError};

use super::components::{
    button::{button, button_primary},
    card::kbd,
    segmented::segmented,
    slider::{SliderState, slider},
    status_dot::status_dot,
};
use super::editor::player::{Item, Player, Sound, pause_on_focus_loss, stop_audio_tree};
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
/// Volume keyboard step in percentage points (`Up` / `Down`).
const VOLUME_STEP: i32 = 5;
/// Debounce before flushing rapid slider/key volume changes to `settings.toml`.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(200);
/// PCM stream parameters for the live-volume audio pipe (`48 kHz` stereo `f32le`).
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: usize = 2;
/// Frames processed per live audio chunk (~21 ms at 48 kHz for responsive volume and mute).
const AUDIO_CHUNK_FRAMES: usize = 1024;
/// Viewport width below which the transport and speed/volume groups split onto two tidy rows.
const COMPACT_W: f32 = 940.0;
/// Wider breakpoint when bookmark jump buttons are also visible beside the speed controls.
const COMPACT_W_WITH_BOOKMARKS: f32 = 1_320.0;
/// Horizontal inset inside the seek bar so the circular thumb stays inside the focus ring.
const SCRUB_INSET: f32 = 8.0;
/// Approximate width of the floating scrubber hover tooltip, for edge clamping.
const TOOLTIP_W: f32 = 54.0;

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
    /// Labels for each audio stream in the clip (track 0 is Master).
    audio_tracks: Vec<String>,
    /// Index of the audio stream currently selected (`0` = Master).
    selected_track: usize,
    /// Whether the floating audio-track picker popover is open.
    track_menu_open: bool,
    problem: Option<String>,
    loading: bool,
    /// Bumped on every open, so a slow probe of an earlier clip is ignored.
    generation: u64,
    player: Player,
    /// Live PCM audio pipeline (`ffmpeg` -> scaled `f32le` -> `ffplay`), updated without restarts.
    audio: Option<LiveAudio>,
    /// Shared linear gain (`f32::to_bits`) read by the live audio thread on every chunk.
    live_gain: Arc<AtomicU32>,
    /// The speed picked with the buttons.
    speed: f64,
    /// Persisted output volume (`0..=100`).
    volume_percent: u8,
    /// Most recent non-zero volume (`1..=100`), restored when unmuting from 0%.
    last_nonzero_volume: u8,
    /// Persisted mute flag.
    muted: bool,
    /// Drag/bounds state for the reusable horizontal volume slider.
    volume_slider: SliderState,
    /// Whether persisted player preferences have been hydrated from `ZeroClipApp`.
    prefs_loaded: bool,
    /// Whether in-memory volume/mute changes are waiting to be flushed to `settings.toml`.
    prefs_dirty: bool,
    /// Monotonic counter used to debounce disk saves while dragging the slider.
    persist_gen: u64,
    /// When the mouse went down on the picture, while it is still down.
    hold: Option<Instant>,
    /// The user left this screen (`pause`) since the clip was opened, so a clip that finishes
    /// reading now must not start playing behind another tab.
    left: bool,
    /// Playing at the hold speed.
    boosted: bool,
    ticking: bool,
    fullscreen: bool,
    focus: Option<FocusHandle>,
    /// Generation that last claimed initial keyboard focus, so render ticks don't steal Tab focus.
    focused_generation: Option<u64>,
    bar: Rc<Cell<Bounds<gpui::Pixels>>>,
    /// The seek bar is being dragged.
    scrubbing: bool,
    /// Cursor position along the seek bar (`0.0..=1.0`) while hovering, for the preview tooltip.
    hover_frac: Option<f32>,
    /// Subscription pausing playback when the OS window loses focus or is minimised.
    window_activation_sub: Option<gpui::Subscription>,
    /// Last observed OS window activation state.
    window_active: bool,
    /// Remembers whether playback was running when the window lost focus (never auto-resumes).
    paused_on_blur: bool,
}

impl Drop for VideoPlayerView {
    fn drop(&mut self) {
        self.stop_audio();
    }
}

impl VideoPlayerView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        let defaults = PlayerSettings::default();
        let initial_vol = clamp_volume(defaults.volume);
        let initial_gain = volume_gain(initial_vol, defaults.muted);
        Self {
            app,
            path: None,
            name: String::new(),
            ffmpeg: None,
            duration: Hns::ZERO,
            bookmarks: Vec::new(),
            has_sound: false,
            audio_tracks: Vec::new(),
            selected_track: 0,
            track_menu_open: false,
            problem: None,
            loading: false,
            generation: 0,
            player: Player::default(),
            audio: None,
            live_gain: Arc::new(AtomicU32::new(initial_gain.to_bits())),
            speed: 1.0,
            volume_percent: initial_vol,
            last_nonzero_volume: initial_vol.max(1),
            muted: defaults.muted,
            volume_slider: SliderState::default(),
            prefs_loaded: false,
            prefs_dirty: false,
            persist_gen: 0,
            hold: None,
            left: false,
            boosted: false,
            ticking: false,
            fullscreen: false,
            focus: None,
            focused_generation: None,
            bar: Rc::new(Cell::new(Bounds::default())),
            scrubbing: false,
            hover_frac: None,
            window_activation_sub: None,
            window_active: true,
            paused_on_blur: false,
        }
    }

    /// Hydrates saved volume and mute once from `ZeroClipApp` (must not run synchronously inside
    /// `open`, which is invoked while `ZeroClipApp` is mutably borrowed).
    fn load_audio_prefs(&mut self, cx: &mut Context<Self>) {
        if self.prefs_loaded {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let prefs = app.read(cx).settings.player;
        self.prefs_loaded = true;
        self.volume_percent = clamp_volume(prefs.volume);
        if self.volume_percent > 0 {
            self.last_nonzero_volume = self.volume_percent;
        }
        self.muted = prefs.muted;
        self.apply_live_gain();
    }

    /// Pushes the current effective gain to the live PCM audio thread without restarting playback.
    fn apply_live_gain(&self) {
        let gain = volume_gain(self.volume_percent, self.muted);
        self.live_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Updates `ZeroClipApp`'s in-memory settings and draft so later Settings saves keep our
    /// volume/mute state, and optionally writes `settings.toml` immediately.
    fn sync_audio_prefs(&mut self, write_disk: bool, cx: &mut Context<Self>) {
        let prefs = PlayerSettings { volume: self.volume_percent, muted: self.muted };
        let _ = self.app.update(cx, |app, _| {
            let changed = app.settings.player != prefs;
            app.settings.player = prefs;
            app.draft.settings.player = prefs;
            if write_disk
                && (changed || self.prefs_dirty)
                && let Err(e) = crate::settings_store::save(&app.paths.settings_file, &app.settings)
            {
                tracing::warn!(error = %e, "couldn't save player audio preferences");
            }
        });
        self.prefs_dirty = !write_disk;
    }

    /// Persists volume and mute right away (used on mute click and when leaving the player).
    fn persist_audio_prefs_now(&mut self, cx: &mut Context<Self>) {
        self.persist_gen = self.persist_gen.wrapping_add(1);
        self.sync_audio_prefs(true, cx);
    }

    /// Debounces disk writes during continuous slider drags or held Up/Down keys while keeping
    /// in-memory app settings immediately up to date.
    fn schedule_persist_audio_prefs(&mut self, cx: &mut Context<Self>) {
        self.sync_audio_prefs(false, cx);
        self.persist_gen = self.persist_gen.wrapping_add(1);
        let save_gen = self.persist_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PERSIST_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.persist_gen == save_gen && this.prefs_dirty {
                    this.sync_audio_prefs(true, cx);
                }
            });
        })
        .detach();
    }

    fn stop_audio(&mut self) {
        if let Some(audio) = self.audio.take() {
            audio.shutdown();
        }
    }

    /// Sets volume from dragging or clicking the horizontal slider. Dragging above 0 unmutes.
    fn set_volume_from_slider(&mut self, percent: u8, cx: &mut Context<Self>) {
        let pct = clamp_volume(percent);
        if pct > 0 {
            self.muted = false;
            self.last_nonzero_volume = pct;
        }
        self.volume_percent = pct;
        self.apply_live_gain();
        self.schedule_persist_audio_prefs(cx);
        cx.notify();
    }

    /// Toggles mute (`M` key or speaker button). Unmuting when volume was dragged to 0 restores
    /// the last non-zero level so the speaker button always produces audible sound.
    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if self.muted || self.volume_percent == 0 {
            self.muted = false;
            if self.volume_percent == 0 {
                self.volume_percent = self.last_nonzero_volume.clamp(1, 100);
            }
        } else {
            self.muted = true;
        }
        self.apply_live_gain();
        self.persist_audio_prefs_now(cx);
        cx.notify();
    }

    /// Steps volume by `delta` percentage points (`Up` / `Down`). Increasing volume unmutes.
    fn adjust_volume(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = step_volume(self.volume_percent, delta);
        if delta > 0 {
            self.muted = false;
        }
        if next > 0 {
            self.last_nonzero_volume = next;
        }
        self.volume_percent = next;
        self.apply_live_gain();
        self.schedule_persist_audio_prefs(cx);
        cx.notify();
    }

    /// Switches which audio stream in a multi-track clip is playing (`0` = Master).
    fn select_track(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.audio_tracks.len() {
            return;
        }
        self.track_menu_open = false;
        if self.selected_track != index {
            self.selected_track = index;
            self.restart(cx);
        }
        cx.notify();
    }

    /// Opens `path` and starts playing it.
    pub fn open(&mut self, path: PathBuf, ffmpeg_path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.stop_audio();
        self.player.reset();
        self.generation += 1;
        let generation = self.generation;
        self.name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.path = Some(path.clone());
        self.problem = None;
        self.loading = true;
        self.duration = Hns::ZERO;
        self.bookmarks = zc_core::bookmarks::load(&path);
        self.has_sound = false;
        self.audio_tracks.clear();
        self.selected_track = 0;
        self.track_menu_open = false;
        self.boosted = false;
        self.hold = None;
        self.paused_on_blur = false;
        self.left = false;
        // Nothing of the last clip may play under this one if this one fails to open.
        self.ffmpeg = None;
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
                this.load_audio_prefs(cx);
                this.loading = false;
                match result {
                    Ok((ffmpeg, info)) => {
                        this.duration = info.duration;
                        this.audio_tracks =
                            build_audio_tracks(info.stream.audio_tracks, &info.audio_names);
                        this.has_sound = !this.audio_tracks.is_empty();
                        this.selected_track = 0;
                        this.ffmpeg = Some(ffmpeg);
                        if this.window_active && !this.left {
                            this.play_from(Hns::ZERO, cx);
                        } else if let (Some(ffmpeg), Some(path)) =
                            (this.ffmpeg.as_ref(), this.path.as_ref())
                        {
                            this.paused_on_blur = !this.window_active;
                            this.player.show_at(ffmpeg, path, Hns::ZERO, Hns::ZERO);
                            this.ensure_ticking(cx);
                        }
                    }
                    Err(error) => this.problem = Some(format!("Couldn't open this clip: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Closes the clip when it is `path`, a file that was just deleted.
    pub fn forget(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.path.as_deref() != Some(path) {
            return;
        }
        self.pause(cx);
        self.generation += 1;
        self.player.reset();
        self.path = None;
        self.ffmpeg = None;
        self.name.clear();
        self.loading = false;
        self.problem = Some("This clip was deleted.".to_owned());
        cx.notify();
    }

    /// Stops playback, picture and sound (when the user leaves this screen).
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.left = true;
        self.hold = None;
        self.boosted = false;
        self.track_menu_open = false;
        if self.prefs_dirty {
            self.persist_audio_prefs_now(cx);
        }
        let had_audio = self.audio.is_some();
        self.stop_audio();
        if self.player.playing || had_audio {
            self.player.pause();
            cx.notify();
        }
    }

    /// Pauses when the window loses OS focus or is minimised, remembering that playback was
    /// running; never auto-resumes when focus returns.
    fn handle_window_activation(&mut self, active: bool, cx: &mut Context<Self>) {
        self.window_active = active;
        if pause_on_focus_loss(self.player.playing, active, &mut self.paused_on_blur) {
            self.hold = None;
            self.boosted = false;
            self.track_menu_open = false;
            self.stop_audio();
            self.player.pause();
            cx.notify();
        }
    }

    fn effective_speed(&self) -> f64 {
        if self.boosted { BOOST } else { self.speed }
    }

    /// Plays from `at`, at the current speed, selected audio track, and live volume.
    fn play_from(&mut self, at: Hns, cx: &mut Context<Self>) {
        let (Some(ffmpeg), Some(path)) = (self.ffmpeg.clone(), self.path.clone()) else {
            return;
        };
        let at = at.clamp(Hns::ZERO, self.duration);
        self.paused_on_blur = false;
        self.stop_audio();
        let speed = self.effective_speed();
        self.player.speed = speed;
        // Video only in `Player`: pass a muted `Sound::track` so `Player` spawns no `ffplay`
        // while `LiveAudio` below scales PCM samples live without restarting playback.
        let mut muted_track = Sound::track(0, 0.0, None);
        muted_track.mute = true;
        self.player.play(
            &ffmpeg,
            vec![Item {
                path: path.clone(),
                from: at,
                to: self.duration,
                sounds: vec![muted_track],
                solo: None,
            }],
            at,
        );
        if self.has_sound && self.player.playing {
            self.apply_live_gain();
            let track = self.selected_track.min(self.audio_tracks.len().saturating_sub(1));
            self.audio = LiveAudio::start(
                &ffmpeg,
                &path,
                at,
                self.duration,
                track,
                speed,
                self.live_gain.clone(),
            );
        }
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Plays or pauses. At the very end it starts over.
    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.player.playing {
            self.hold = None;
            self.boosted = false;
            self.stop_audio();
            self.player.pause();
            cx.notify();
        } else if self.ffmpeg.is_some() {
            let at = self.player.playhead;
            let at = if self.duration - at < Hns::from_millis(200) { Hns::ZERO } else { at };
            self.play_from(at, cx);
        }
    }

    /// Stops playback, shuts down audio processes, and seeks the playhead back to the start.
    fn stop(&mut self, cx: &mut Context<Self>) {
        let ready = self.ffmpeg.is_some() && self.problem.is_none();
        if !can_stop(ready, self.player.playing, self.player.playhead) {
            return;
        }
        self.hold = None;
        self.boosted = false;
        self.scrubbing = false;
        self.track_menu_open = false;
        self.paused_on_blur = false;
        self.stop_audio();
        self.player.pause();
        if let (Some(ffmpeg), Some(path)) = (self.ffmpeg.as_ref(), self.path.as_ref()) {
            self.player.show_at(ffmpeg, path, Hns::ZERO, Hns::ZERO);
        } else {
            self.player.playhead = Hns::ZERO;
        }
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Moves to `t`: keeps playing if it was, else shows that frame.
    fn seek(&mut self, t: Hns, cx: &mut Context<Self>) {
        let t = t.clamp(Hns::ZERO, self.duration);
        if self.player.playing {
            self.play_from(t, cx);
        } else if self.ffmpeg.is_some() && self.path.is_some() {
            self.stop_audio();
            if let (Some(ffmpeg), Some(path)) = (self.ffmpeg.as_ref(), self.path.as_ref()) {
                self.player.show_at(ffmpeg, path, t, t);
            }
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

    /// Restarts at the same place so a new speed or audio track takes effect.
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
                            this.stop_audio();
                            this.ticking = false;
                            return false;
                        }
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
                        if !this.player.playing {
                            this.stop_audio();
                        } else if let Some(audio) = this.audio.as_mut()
                            && this.duration - this.player.playhead > Hns::from_secs(1)
                            && let Ok(Some(status)) = audio.try_wait_sink()
                        {
                            let said = std::fs::read_to_string(audio_log()).unwrap_or_default();
                            tracing::warn!(
                                %status,
                                said = said.trim(),
                                "the player's audio stopped early"
                            );
                            this.stop_audio();
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

    fn key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            return;
        }
        let now = self.player.playhead;
        let sound_ready = self.ffmpeg.is_some() && self.problem.is_none() && self.has_sound;
        match k.key.as_str() {
            "space" | "k" => self.toggle(cx),
            "right" | "l" => self.seek(now + SKIP, cx),
            "left" | "j" => self.seek(now - SKIP, cx),
            "m" if sound_ready => self.toggle_mute(cx),
            "up" if sound_ready => self.adjust_volume(VOLUME_STEP, cx),
            "down" if sound_ready => self.adjust_volume(-VOLUME_STEP, cx),
            "f" => self.set_fullscreen(!self.fullscreen, window, cx),
            "escape" if self.track_menu_open => {
                self.track_menu_open = false;
                cx.notify();
            }
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

    fn hover_frac_at(&self, pos: gpui::Point<gpui::Pixels>) -> Option<f32> {
        let b = self.bar.get();
        hover_frac_in_rect(
            f32::from(pos.x),
            f32::from(pos.y),
            f32::from(b.origin.x),
            f32::from(b.origin.y),
            f32::from(b.size.width),
            f32::from(b.size.height),
        )
    }
}

/// Live-volume audio playback for the video player: `ffmpeg` decodes the chosen track to raw
/// `48 kHz` stereo `f32le` PCM, a dedicated thread scales each 21 ms chunk by the shared atomic
/// gain with a per-frame linear ramp (so mute and slider drags apply immediately without clicks
/// or playback restarts), and pipes the result to `ffplay`.
struct LiveAudio {
    sink: Child,
    decoder: Child,
    stop: Arc<AtomicBool>,
}

impl LiveAudio {
    fn start(
        ffmpeg: &Ffmpeg,
        path: &Path,
        from: Hns,
        to: Hns,
        track: usize,
        rate: f64,
        live_gain: Arc<AtomicU32>,
    ) -> Option<Self> {
        let duration_secs = (to - from).as_secs_f64();
        if duration_secs <= 0.0 {
            return None;
        }
        let ffplay = ffmpeg.exe.with_file_name(if cfg!(windows) { "ffplay.exe" } else { "ffplay" });
        if !ffplay.is_file() {
            return None;
        }

        let mut dec_cmd = Command::new(&ffmpeg.exe);
        dec_cmd
            .args(["-hide_banner", "-nostdin", "-loglevel", "error"])
            .arg("-ss")
            .arg(format!("{:.6}", from.as_secs_f64()))
            // `-t` before `-i` counts source time: after it, it would count output time, which
            // `atempo` stretches, and a slow speed would cut the sound short.
            .arg("-t")
            .arg(format!("{duration_secs:.6}"))
            .arg("-i")
            .arg(zc_editor::ffmpeg::input_arg(path))
            .args(["-vn", "-sn", "-map"])
            .arg(format!("0:a:{track}"));
        let tempo_filters = atempo_filters(rate);
        if !tempo_filters.is_empty() {
            dec_cmd.arg("-af").arg(tempo_filters.join(","));
        }
        dec_cmd
            .args([
                "-ar",
                &AUDIO_SAMPLE_RATE.to_string(),
                "-ac",
                &AUDIO_CHANNELS.to_string(),
                "-f",
                "f32le",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        no_window(&mut dec_cmd);
        let mut decoder = dec_cmd.spawn().ok()?;
        let Some(mut dec_out) = decoder.stdout.take() else {
            let _ = decoder.kill();
            let _ = decoder.wait();
            return None;
        };

        let log = std::fs::File::create(audio_log()).map_or_else(|_| Stdio::null(), Stdio::from);
        let mut sink_cmd = Command::new(&ffplay);
        // Low-latency raw PCM sink so audio starts in step with video and volume updates land in
        // ~21 ms instead of sitting behind ffplay's default probe buffer.
        sink_cmd
            .args([
                "-nodisp",
                "-autoexit",
                "-loglevel",
                "error",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-fflags",
                "nobuffer",
                "-f",
                "f32le",
                "-sample_rate",
                &AUDIO_SAMPLE_RATE.to_string(),
                "-ch_layout",
                "stereo",
                "-i",
                "pipe:0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log);
        no_window(&mut sink_cmd);
        let Ok(mut sink) = sink_cmd.spawn() else {
            let _ = decoder.kill();
            let _ = decoder.wait();
            return None;
        };
        let Some(mut sink_in) = sink.stdin.take() else {
            stop_audio_tree(sink, vec![decoder]);
            return None;
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let _ = std::thread::Builder::new().name("zc-player-audio".into()).spawn(move || {
            let chunk_samples = AUDIO_CHUNK_FRAMES * AUDIO_CHANNELS;
            let frame_bytes = AUDIO_CHANNELS * size_of::<f32>();
            let chunk_bytes = chunk_samples * size_of::<f32>();
            let mut raw_buf = vec![0u8; chunk_bytes];
            let mut pcm_buf = vec![0.0_f32; chunk_samples];
            let mut current_gain = f32::from_bits(live_gain.load(Ordering::Relaxed));

            while !stop_flag.load(Ordering::Relaxed) {
                let got = read_chunk(&mut dec_out, &mut raw_buf);
                let aligned_bytes = (got / frame_bytes) * frame_bytes;
                if aligned_bytes == 0 {
                    break;
                }
                let samples = aligned_bytes / size_of::<f32>();
                for (dst, bytes) in
                    pcm_buf[..samples].iter_mut().zip(raw_buf[..aligned_bytes].as_chunks::<4>().0)
                {
                    *dst = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                }
                let target_gain = f32::from_bits(live_gain.load(Ordering::Relaxed));
                scale_pcm_chunk(&mut pcm_buf[..samples], current_gain, target_gain, AUDIO_CHANNELS);
                current_gain = target_gain;
                for (dst, &sample) in raw_buf[..aligned_bytes]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(&pcm_buf[..samples])
                {
                    dst.copy_from_slice(&sample.to_le_bytes());
                }
                if sink_in.write_all(&raw_buf[..aligned_bytes]).is_err() {
                    break;
                }
            }
        });

        Some(Self { sink, decoder, stop })
    }

    fn try_wait_sink(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.sink.try_wait()
    }

    fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        stop_audio_tree(self.sink, vec![self.decoder]);
    }
}

/// Keeps `ffmpeg`/`ffplay`/`taskkill` from flashing a console window in GUI builds.
/// <https://learn.microsoft.com/windows/win32/procthread/process-creation-flags>
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn read_chunk(reader: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
    total
}

fn atempo_filters(rate: f64) -> Vec<String> {
    let mut filters = Vec::new();
    let mut left = if rate > 0.0 { rate } else { 1.0 };
    while left > 2.0 {
        filters.push("atempo=2.0".to_owned());
        left /= 2.0;
    }
    while left < 0.5 {
        filters.push("atempo=0.5".to_owned());
        left /= 0.5;
    }
    if (left - 1.0).abs() > 1e-6 {
        filters.push(format!("atempo={left:.4}"));
    }
    filters
}

fn audio_log() -> PathBuf {
    std::env::temp_dir().join("zeroclip-player-ffplay.log")
}

/// Clamps a volume percentage into `0..=100`.
fn clamp_volume(percent: u8) -> u8 {
    percent.min(100)
}

/// Effective audible volume percentage (`0` when muted, else clamped `0..=100`).
fn effective_volume_percent(percent: u8, muted: bool) -> u8 {
    if muted { 0 } else { clamp_volume(percent) }
}

/// Converts a `0..=100` volume percentage and mute flag into a linear PCM gain in `0.0..=1.0`.
///
/// Uses a quadratic perceptual taper (`(p / 100)^2`) so 50% sounds roughly half as loud (-12 dB)
/// rather than barely quieter (-6 dB), while keeping 0% exact silence and 100% exact unity gain.
fn volume_gain(percent: u8, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    let normalized = f32::from(clamp_volume(percent)) / 100.0;
    normalized * normalized
}

/// Steps `percent` by `delta` percentage points, clamped to `0..=100`.
fn step_volume(percent: u8, delta: i32) -> u8 {
    (i32::from(clamp_volume(percent)) + delta).clamp(0, 100) as u8
}

/// Slider position (`0.0..=1.0`) for the current volume and mute state.
fn volume_to_fraction(percent: u8, muted: bool) -> f32 {
    f32::from(effective_volume_percent(percent, muted)) / 100.0
}

/// Converts a slider drag fraction (`0.0..=1.0`) into a whole percentage (`0..=100`).
fn fraction_to_volume(frac: f32) -> u8 {
    if !frac.is_finite() {
        return 0;
    }
    (frac.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// Picks the Lucide speaker icon name for the current volume and mute state.
fn volume_icon(percent: u8, muted: bool) -> &'static str {
    let eff = effective_volume_percent(percent, muted);
    if eff == 0 {
        "volume-x"
    } else if eff <= 50 {
        "volume-1"
    } else {
        "volume-2"
    }
}

/// Scales interleaved `f32` PCM samples in place, linearly ramping gain from `start_gain` to
/// `end_gain` across the chunk's frames so volume and mute transitions never click.
fn scale_pcm_chunk(samples: &mut [f32], start_gain: f32, end_gain: f32, channels: usize) {
    if channels == 0 || samples.is_empty() {
        return;
    }
    let start = if start_gain.is_finite() { start_gain.clamp(0.0, 1.0) } else { 0.0 };
    let end = if end_gain.is_finite() { end_gain.clamp(0.0, 1.0) } else { 0.0 };
    if start == 0.0 && end == 0.0 {
        samples.fill(0.0);
        return;
    }
    let frames = samples.len() / channels;
    let denom = frames.saturating_sub(1).max(1) as f32;
    for f in 0..frames {
        let gain = if (end - start).abs() <= f32::EPSILON {
            end
        } else {
            start + (end - start) * (f as f32 / denom)
        };
        let base = f * channels;
        for ch in 0..channels {
            let s = samples[base + ch];
            samples[base + ch] = if s.is_finite() { (s * gain).clamp(-1.0, 1.0) } else { 0.0 };
        }
    }
}

/// Builds user-facing labels for every audio stream reported by `ffprobe` (`0` is Master).
fn build_audio_tracks(count: u8, names: &[String]) -> Vec<String> {
    (0..usize::from(count))
        .map(|i| match names.get(i) {
            Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
            _ => match i {
                0 => "Master".to_owned(),
                1 => "Game".to_owned(),
                2 => "Mic".to_owned(),
                n => format!("Track {}", n + 1),
            },
        })
        .collect()
}

/// Returns the clamped `0.0..=1.0` hover fraction only when `(x, y)` lies over the seek bar
/// (including its horizontal thumb insets); `None` outside so tooltips never linger.
fn hover_frac_in_rect(x: f32, y: f32, bx: f32, by: f32, bw: f32, bh: f32) -> Option<f32> {
    if bw <= 1.0 || bh <= 1.0 {
        return None;
    }
    if y < by || y > by + bh || x < bx - SCRUB_INSET || x > bx + bw + SCRUB_INSET {
        return None;
    }
    Some(((x - bx) / bw).clamp(0.0, 1.0))
}

fn clamped_tooltip_left(frac: f32, track_width: f32) -> f32 {
    if track_width <= TOOLTIP_W {
        return 0.0;
    }
    let center = frac.clamp(0.0, 1.0) * track_width;
    (center - TOOLTIP_W / 2.0).clamp(0.0, track_width - TOOLTIP_W)
}

fn is_compact_controls(viewport_width: f32, has_bookmarks: bool) -> bool {
    let threshold = if has_bookmarks { COMPACT_W_WITH_BOOKMARKS } else { COMPACT_W };
    viewport_width < threshold
}

/// Whether the Stop button is active: a clip is loaded and playback is either running or parked
/// past the start of the clip.
#[must_use]
fn can_stop(ready: bool, playing: bool, playhead: Hns) -> bool {
    ready && (playing || playhead > Hns::ZERO)
}

fn tabular_features() -> FontFeatures {
    FontFeatures(std::sync::Arc::new(vec![("tnum".to_owned(), 1)]))
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

/// Subtle 1 px vertical separator between control groups on the wide bar.
fn group_divider() -> gpui::Div {
    div().w(px(1.)).h(px(20.)).flex_none().bg(rgb(theme::BORDER))
}

/// Small uppercase-free label beside a segmented control group (`Speed`, `Volume`).
fn group_label(text: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .whitespace_nowrap()
        .text_size(px(theme::TEXT_CAPTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::MUTED_FG))
        .child(text)
}

/// A keyboard-shortcut legend entry: one or more tactile `kbd` chips plus a readable action label.
fn shortcut_hint(keys: &[&'static str], action: &'static str) -> gpui::Div {
    div().flex().flex_row().items_center().gap_1().children(keys.iter().map(|k| kbd(*k))).child(
        div()
            .ml(px(2.))
            .text_size(px(theme::TEXT_CAPTION))
            .text_color(rgb(theme::MUTED_FG))
            .child(action),
    )
}

/// A disabled secondary button shell matching [`button`]'s geometry while clearly non-interactive.
fn disabled_transport(
    id: &'static str,
    label: &'static str,
    aria: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria)
        .h(px(theme::CONTROL_H_LG))
        .px_3()
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .whitespace_nowrap()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::MUTED))
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::TEXT_DIM))
        .opacity(0.55)
        .cursor_default()
        .child(label)
}

impl Render for VideoPlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_audio_prefs(cx);
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
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        // Claim focus once per opened clip so Space/arrow/F shortcuts work immediately without
        // stealing focus back on every 16 ms frame when the user Tabs to the top bar.
        if self.focused_generation != Some(self.generation) {
            self.focused_generation = Some(self.generation);
            window.focus(&focus, cx);
        }
        let playing = self.player.playing;
        let ready = self.ffmpeg.is_some() && self.problem.is_none();
        let at = self.player.playhead.min(self.duration);
        let frac = if self.duration > Hns::ZERO {
            (at.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let viewport_w = f32::from(window.viewport_size().width);
        let compact = is_compact_controls(viewport_w, !self.bookmarks.is_empty());

        let header = (!self.fullscreen).then(|| {
            let path = self.path.clone();
            let title: SharedString =
                if self.name.is_empty() { "Video player".into() } else { self.name.clone().into() };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_4()
                .py_2()
                .bg(rgb(theme::BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(icon("film", theme::MUTED_FG))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .truncate()
                                .text_size(px(theme::TEXT_HEADING))
                                .line_height(px(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::FG))
                                .child(title),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .children(path.map(|path| {
                            button_primary("player-edit", "Edit clip")
                                .h(px(theme::CONTROL_H))
                                .px_3()
                                .flex_none()
                                .whitespace_nowrap()
                                .child(icon("scissors", theme::PRIMARY_FG).size(px(14.)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pause(cx);
                                    let path = path.clone();
                                    let _ = this
                                        .app
                                        .update(cx, |app, cx| app.open_in_editor(path, false, cx));
                                }))
                        }))
                        .child(
                            button("player-close", "Close")
                                .h(px(theme::CONTROL_H))
                                .px_3()
                                .flex_none()
                                .whitespace_nowrap()
                                .child(icon("x", theme::FG_SOFT).size(px(14.)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pause(cx);
                                    let _ = this.app.update(cx, |app, cx| {
                                        app.tab = crate::app::Tab::Clips;
                                        cx.notify();
                                    });
                                })),
                        ),
                )
        });

        let picture = div()
            .id("player-picture")
            .flex_1()
            .min_h(px(120.))
            .bg(rgb(theme::MEDIA_BG))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .relative()
            .when(ready, |d| d.cursor_pointer())
            // Press and hold for the fast speed; a short click plays or pauses.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    if let Some(focus) = &this.focus
                        && !focus.contains_focused(window, cx)
                    {
                        window.focus(focus, cx);
                    }
                    if this.track_menu_open {
                        this.track_menu_open = false;
                        cx.notify();
                    }
                    this.hold = Some(Instant::now());
                }),
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
            )
            // Released outside the picture: the hold is over, but it was no click.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.hold = None;
                    if this.boosted {
                        this.boosted = false;
                        this.restart(cx);
                    }
                }),
            );
        let picture = match (&self.player.shown, &self.problem) {
            (_, Some(problem)) => picture.child(
                div()
                    .max_w(px(480.))
                    .mx_4()
                    .p_4()
                    .rounded(px(theme::RADIUS))
                    .bg(rgb(theme::DANGER_SOFT))
                    .border_1()
                    .border_color(rgb(theme::DANGER_BORDER))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .size(px(24.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(rgb(theme::DANGER_BORDER))
                                    .child(icon("x", theme::DANGER).size(px(14.))),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::TEXT_HEADING))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::FG))
                                    .child("Playback unavailable"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .line_height(px(18.))
                            .text_color(rgb(theme::FG_SOFT))
                            .child(problem.clone()),
                    ),
            ),
            (Some(frame), _) => picture.child(
                img(gpui::ImageSource::Render(frame.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Contain),
            ),
            (None, _) if self.loading => picture.child(
                div()
                    .px_4()
                    .py_2()
                    .rounded_full()
                    .bg(rgb(theme::CARD))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(theme::TEXT_BODY))
                    .text_color(rgb(theme::FG_SOFT))
                    .child(status_dot(rgb(theme::BRAND)))
                    .child("Opening clip\u{2026}"),
            ),
            (None, _) => picture.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .child(
                        div()
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(theme::CARD))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .child(icon("film", theme::MUTED_FG)),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .text_color(rgb(theme::FG_SOFT))
                            .child("Select a clip to watch."),
                    ),
            ),
        };
        let picture = picture.children(self.boosted.then(|| {
            div()
                .absolute()
                .top_3()
                .right_3()
                .px_3()
                .py_1()
                .rounded_full()
                .bg(rgba(theme::SCRIM))
                .border_1()
                .border_color(rgb(theme::BORDER_STRONG))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::FG))
                .child(status_dot(rgb(theme::BRAND)))
                .child(format!("{BOOST}x"))
        }));

        let bar_bounds = self.bar.clone();
        let track_width = f32::from(self.bar.get().size.width);
        let scrubbing = self.scrubbing;
        let hover_frac = if ready { self.hover_frac } else { None };
        let preview_frac = if ready && self.duration > Hns::ZERO {
            if scrubbing { Some(frac) } else { hover_frac }
        } else {
            None
        };
        let scrubber_group = "player-scrubber";
        let track_top = if scrubbing { 11.0 } else { 12.0 };
        let track_h = if scrubbing { 8.0 } else { 6.0 };

        let seek_track = div()
            .absolute()
            .left(px(SCRUB_INSET))
            .right(px(SCRUB_INSET))
            .top_0()
            .bottom_0()
            .child(
                canvas(move |b, _, _| bar_bounds.set(b), |_, (), _, _| {}).absolute().size_full(),
            )
            // Base track: recessed empty rail when unloaded, distinct buffered rail when ready.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(track_top))
                    .h(px(track_h))
                    .rounded_full()
                    .when(ready, |d| {
                        d.bg(rgb(theme::SCRUB_BUFFERED))
                            .group_hover(scrubber_group, |s| s.top(px(11.)).h(px(8.)))
                    })
                    .when(!ready, |d| {
                        d.bg(rgb(theme::SURFACE_INSET))
                            .border_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                    }),
            )
            // Hover preview fill ahead of the playhead.
            .children(hover_frac.filter(|hf| *hf > frac).map(|hf| {
                div()
                    .absolute()
                    .left_0()
                    .top(px(track_top))
                    .h(px(track_h))
                    .w(relative(hf))
                    .rounded_full()
                    .bg(rgb(theme::SCRUB_PREVIEW))
                    .group_hover(scrubber_group, |s| s.top(px(11.)).h(px(8.)))
            }))
            // Played progress fill.
            .when(ready, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(track_top))
                        .h(px(track_h))
                        .w(relative(frac))
                        .rounded_full()
                        .bg(rgb(if scrubbing { theme::BRAND_HOVER } else { theme::BRAND }))
                        .group_hover(scrubber_group, |s| {
                            s.top(px(11.)).h(px(8.)).bg(rgb(theme::BRAND_HOVER))
                        }),
                )
            })
            // Bookmark ticks.
            .when(ready, |d| {
                d.children(self.bookmarks.iter().map(|b| {
                    let at =
                        (b.as_secs_f64() / self.duration.as_secs_f64().max(0.001)).clamp(0.0, 1.0);
                    div()
                        .absolute()
                        .top(px(9.))
                        .left(relative(at as f32))
                        .ml(px(-1.5))
                        .w(px(3.))
                        .h(px(12.))
                        .rounded_full()
                        .bg(rgb(theme::WARN))
                }))
            })
            // Subtle hover cursor marker on the rail.
            .children(hover_frac.filter(|_| !scrubbing).map(|hf| {
                div()
                    .absolute()
                    .top(px(10.))
                    .left(relative(hf))
                    .ml(px(-1.))
                    .w(px(2.))
                    .h(px(10.))
                    .rounded_full()
                    .bg(rgb(theme::FG_SOFT))
            }))
            // Scrubber thumb.
            .when(ready, |d| {
                d.child(
                    div()
                        .absolute()
                        .left(relative(frac))
                        .top(px(if scrubbing { 7. } else { 8. }))
                        .ml(px(if scrubbing { -8. } else { -7. }))
                        .size(px(if scrubbing { 16. } else { 14. }))
                        .rounded_full()
                        .bg(rgb(theme::PRIMARY))
                        .border_2()
                        .border_color(rgb(if scrubbing { theme::RING } else { theme::CARD }))
                        .group_hover(scrubber_group, |s| {
                            s.top(px(7.)).ml(px(-8.)).size(px(16.)).border_color(rgb(theme::RING))
                        }),
                )
            })
            // Floating hover/scrub timestamp preview tooltip.
            .children(preview_frac.filter(|_| track_width > 1.0).map(|pf| {
                let preview_time = Hns::from_secs_f64(self.duration.as_secs_f64() * f64::from(pf));
                let left = clamped_tooltip_left(pf, track_width);
                div()
                    .absolute()
                    .top(px(-28.))
                    .left(px(left))
                    .min_w(px(TOOLTIP_W))
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_SM))
                    .bg(rgb(theme::SURFACE_RAISED))
                    .border_1()
                    .border_color(rgb(theme::BORDER_STRONG))
                    .shadow(theme::floating_shadow())
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::MEDIUM)
                    .font_features(tabular_features())
                    .text_color(rgb(theme::FG))
                    .whitespace_nowrap()
                    .child(clock(preview_time))
            }));

        let seek_aria: SharedString =
            format!("Seek timeline, {} of {}", clock(at), clock(self.duration)).into();
        let seek_bar = div()
            .id("player-bar")
            .group(scrubber_group)
            .tab_stop(ready)
            .role(Role::Slider)
            .aria_label(seek_aria)
            .relative()
            .flex_1()
            .min_w(px(120.))
            .h(px(theme::CONTROL_H))
            .rounded(px(theme::RADIUS_SM))
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .when(ready, |d| d.cursor_pointer())
            .when(!ready, |d| d.cursor_default())
            .child(seek_track)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.hover_frac.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if !ready {
                        return;
                    }
                    this.scrubbing = true;
                    this.hover_frac = this.hover_frac_at(ev.position);
                    this.seek_to_x(ev.position.x, cx);
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                if !ready {
                    return;
                }
                if this.scrubbing && ev.dragging() {
                    this.seek_to_x(ev.position.x, cx);
                } else {
                    let next_hover = this.hover_frac_at(ev.position);
                    let was_scrubbing = this.scrubbing;
                    this.scrubbing = false;
                    let moved = match (this.hover_frac, next_hover) {
                        (Some(a), Some(b)) => (a - b).abs() > 0.002,
                        (None, None) => false,
                        _ => true,
                    };
                    if moved || was_scrubbing {
                        this.hover_frac = next_hover;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.scrubbing {
                        this.scrubbing = false;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.scrubbing {
                        this.scrubbing = false;
                        cx.notify();
                    }
                }),
            );

        let time_min_w = if self.duration >= Hns::from_secs(3_600) { 56.0 } else { 44.0 };
        let timeline_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .min_w(px(time_min_w))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::SEMIBOLD)
                    .font_features(tabular_features())
                    .text_color(rgb(if ready { theme::FG } else { theme::MUTED_FG }))
                    .child(clock(at)),
            )
            .child(seek_bar)
            .child(
                div()
                    .min_w(px(time_min_w))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::MEDIUM)
                    .font_features(tabular_features())
                    .text_color(rgb(if ready { theme::FG_SOFT } else { theme::MUTED_FG }))
                    .child(clock(self.duration)),
            );

        let play = {
            let base = div()
                .id("player-play")
                .tab_stop(ready)
                .border_1()
                .when(ready, |d| d.border_color(transparent_black()))
                .when(!ready, |d| d.border_color(rgb(theme::BORDER_SUBTLE)))
                .focus_visible(|s| s.border_color(rgb(theme::RING)))
                .role(Role::Button)
                .aria_label(if playing { "Pause" } else { "Play" })
                .w(px(40.))
                .h(px(theme::CONTROL_H_LG))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_MD));
            if ready {
                base.bg(rgb(theme::PRIMARY))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::PRIMARY_HOVER)))
                    .active(|s| s.bg(rgb(theme::PRIMARY_ACTIVE)))
                    .child(icon(if playing { "pause" } else { "play" }, theme::PRIMARY_FG))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
            } else {
                base.bg(rgb(theme::MUTED))
                    .opacity(0.55)
                    .cursor_default()
                    .child(icon("play", theme::TEXT_DIM))
            }
        };

        let stop_enabled = can_stop(ready, playing, at);
        let stop = {
            let base = div()
                .id("player-stop")
                .tab_stop(stop_enabled)
                .border_1()
                .focus_visible(|s| s.border_color(rgb(theme::RING)))
                .role(Role::Button)
                .aria_label("Stop")
                .w(px(40.))
                .h(px(theme::CONTROL_H_LG))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_MD));
            if stop_enabled {
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
        };

        let skip = |id: &'static str, label: &'static str, aria: &'static str, back: bool| {
            if ready {
                button(id, label)
                    .aria_label(aria)
                    .h(px(theme::CONTROL_H_LG))
                    .px_3()
                    .flex_none()
                    .whitespace_nowrap()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let now = this.player.playhead;
                        this.seek(if back { now - SKIP } else { now + SKIP }, cx);
                    }))
            } else {
                disabled_transport(id, label, aria)
            }
        };

        let bookmarks_group = (!self.bookmarks.is_empty()).then(|| {
            let prev = if ready {
                button("player-mark-prev", "\u{25c2} Bookmark")
                    .aria_label("Previous bookmark")
                    .h(px(theme::CONTROL_H_LG))
                    .px_3()
                    .flex_none()
                    .whitespace_nowrap()
                    .on_click(cx.listener(|this, _, _, cx| this.jump_to_bookmark(false, cx)))
            } else {
                disabled_transport("player-mark-prev", "\u{25c2} Bookmark", "Previous bookmark")
            };
            let next = if ready {
                button("player-mark-next", "Bookmark \u{25b8}")
                    .aria_label("Next bookmark")
                    .h(px(theme::CONTROL_H_LG))
                    .px_3()
                    .flex_none()
                    .whitespace_nowrap()
                    .on_click(cx.listener(|this, _, _, cx| this.jump_to_bookmark(true, cx)))
            } else {
                disabled_transport("player-mark-next", "Bookmark \u{25b8}", "Next bookmark")
            };
            div()
                .flex()
                .flex_none()
                .flex_row()
                .items_center()
                .gap_2()
                .when(!compact, |d| d.child(group_divider()))
                .child(prev)
                .child(next)
        });

        let transport_group = div()
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .child(skip("player-back", "\u{2212}10 s", "Skip back 10 seconds", true))
            .child(play)
            .child(stop)
            .child(skip("player-fwd", "+10 s", "Skip forward 10 seconds", false));

        let speed_group = div()
            .id("player-speed-group")
            .aria_label("Playback speed")
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap_2()
            .child(group_label("Speed"))
            .child(segmented(
                "player-speed",
                SPEEDS.iter().map(|s| speed_label(*s)).collect(),
                SPEEDS.iter().position(|s| (*s - self.speed).abs() < 1e-9),
                ready,
                cx.listener(|this, i: &usize, _, cx| {
                    this.speed = SPEEDS.get(*i).copied().unwrap_or(1.0);
                    this.restart(cx);
                }),
            ));

        let volume_enabled = ready && self.has_sound;
        let effective_vol = effective_volume_percent(self.volume_percent, self.muted);
        let is_silent = effective_vol == 0;
        let volume_group = if ready && !self.has_sound {
            div()
                .id("player-volume-group")
                .aria_label("No audio")
                .flex()
                .flex_none()
                .flex_row()
                .items_center()
                .gap_1()
                .child(icon("volume-x", theme::MUTED_FG))
                .child(group_label("No audio"))
        } else {
            let speaker_icon = volume_icon(self.volume_percent, self.muted);
            let speaker_color =
                if !volume_enabled || is_silent { theme::MUTED_FG } else { theme::FG };
            let mute_aria: SharedString = if is_silent {
                "Unmute audio".into()
            } else {
                format!("Mute audio, currently {effective_vol}%").into()
            };
            let mute_btn = {
                let base = div()
                    .id("player-mute")
                    .tab_stop(volume_enabled)
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
                    .child(icon(speaker_icon, speaker_color));
                if volume_enabled {
                    base.cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::ACCENT)))
                        .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx)))
                } else {
                    base.opacity(0.45).cursor_default()
                }
            };

            let view = cx.entity().downgrade();
            let volume_aria: SharedString = format!("Volume, {effective_vol}%").into();
            let vol_slider = slider(
                "player-volume-slider",
                volume_to_fraction(self.volume_percent, self.muted),
                &self.volume_slider,
                move |frac, _, cx| {
                    if !volume_enabled {
                        return;
                    }
                    let pct = fraction_to_volume(frac);
                    let _ = view.update(cx, |this, cx| this.set_volume_from_slider(pct, cx));
                },
            )
            .w(px(108.))
            .min_w(px(88.))
            .flex_none()
            .role(Role::Slider)
            .aria_label(volume_aria)
            .when(!volume_enabled, |d| d.opacity(0.45).cursor_default());

            let pct_label = div()
                .min_w(px(34.))
                .flex_none()
                .flex()
                .justify_end()
                .whitespace_nowrap()
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .font_features(tabular_features())
                .text_color(rgb(if !volume_enabled || is_silent {
                    theme::MUTED_FG
                } else {
                    theme::FG_SOFT
                }))
                .child(format!("{effective_vol}%"));

            let track_picker = (volume_enabled && self.audio_tracks.len() > 1).then(|| {
                let current_track = self.selected_track.min(self.audio_tracks.len() - 1);
                let current_label: SharedString = self.audio_tracks[current_track].clone().into();
                let trigger_aria: SharedString = format!("Audio track, {current_label}").into();
                let menu_open = self.track_menu_open;
                let trigger = div()
                    .id("player-track-picker")
                    .tab_stop(true)
                    .role(Role::Button)
                    .aria_label(trigger_aria)
                    .h(px(theme::CONTROL_H))
                    .px_2()
                    .flex()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(if menu_open { theme::ACCENT } else { theme::SURFACE_RAISED }))
                    .border_1()
                    .border_color(rgb(if menu_open { theme::BORDER_STRONG } else { theme::INPUT }))
                    .focus_visible(|s| s.border_color(rgb(theme::RING)))
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::FG))
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
                    .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
                    .child(icon("headphones", theme::FG_SOFT).size(px(14.)))
                    .child(current_label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.track_menu_open = !this.track_menu_open;
                        cx.notify();
                    }));

                let menu = menu_open.then(|| {
                    let rows = self.audio_tracks.iter().enumerate().map(|(idx, name)| {
                        let selected = idx == current_track;
                        let label: SharedString = name.clone().into();
                        div()
                            .id(("player-track-option", idx))
                            .tab_stop(true)
                            .role(Role::RadioButton)
                            .aria_selected(selected)
                            .h(px(28.))
                            .px_2()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .rounded(px(theme::RADIUS_SM))
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(|s| s.border_color(rgb(theme::RING)))
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(if selected {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(rgb(if selected { theme::FG } else { theme::FG_SOFT }))
                            .when(selected, |d| d.bg(rgb(theme::ACCENT)))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)))
                            .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
                            .child(div().whitespace_nowrap().child(label))
                            .child(if selected {
                                icon("check", theme::BRAND).size(px(14.)).into_any_element()
                            } else {
                                div().size(px(14.)).flex_none().into_any_element()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_track(idx, cx);
                            }))
                    });
                    div()
                        .id("player-track-menu")
                        .absolute()
                        .bottom(px(theme::CONTROL_H + 6.0))
                        .right_0()
                        .min_w(px(156.))
                        .p_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .rounded(px(theme::RADIUS_MD))
                        .bg(rgb(theme::SURFACE_RAISED))
                        .border_1()
                        .border_color(rgb(theme::BORDER_STRONG))
                        .shadow(theme::floating_shadow())
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .text_size(px(theme::TEXT_MICRO))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::MUTED_FG))
                                .child("Audio track"),
                        )
                        .children(rows)
                });

                div().relative().flex_none().child(trigger).children(menu)
            });

            div()
                .id("player-volume-group")
                .aria_label("Volume")
                .flex()
                .flex_none()
                .flex_row()
                .items_center()
                .gap_2()
                .child(mute_btn)
                .child(vol_slider)
                .child(pct_label)
                .children(track_picker)
        };

        let fullscreen_btn = button(
            "player-fullscreen",
            if self.fullscreen { "Exit fullscreen" } else { "Fullscreen" },
        )
        .aria_label(if self.fullscreen { "Exit fullscreen" } else { "Enter fullscreen" })
        .h(px(theme::CONTROL_H))
        .px_3()
        .flex_none()
        .whitespace_nowrap()
        .child(kbd(if self.fullscreen { "Esc" } else { "F" }))
        .on_click(
            cx.listener(|this, _, window, cx| this.set_fullscreen(!this.fullscreen, window, cx)),
        );

        let controls = if compact {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0())
                        .child(transport_group)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_end()
                                .child(fullscreen_btn),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap_x_4()
                        .gap_y_2()
                        .child(speed_group)
                        .children(bookmarks_group)
                        .child(volume_group),
                )
        } else {
            div()
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
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(speed_group)
                        .children(bookmarks_group),
                )
                .child(transport_group)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap_2()
                        .child(volume_group)
                        .child(group_divider())
                        .child(fullscreen_btn),
                )
        };

        let control_bar = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_3()
            .bg(rgb(theme::CARD))
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(timeline_row)
            .child(controls)
            .when(!self.fullscreen, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_x_4()
                        .gap_y_1()
                        .pt_2()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .child(shortcut_hint(&["Space"], "Play / Pause"))
                        .child(shortcut_hint(&["\u{2190}", "\u{2192}"], "Skip 10 s"))
                        .child(shortcut_hint(&["\u{2191}", "\u{2193}"], "Volume"))
                        .child(shortcut_hint(&["M"], "Mute"))
                        .child(shortcut_hint(&["F"], "Fullscreen"))
                        .child(shortcut_hint(&["Hold video"], "2x speed")),
                )
            });

        div()
            .id("video-player")
            .track_focus(&focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme::BG))
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, window, cx| this.key(ev, window, cx)),
            )
            .children(header)
            .child(picture)
            .child(control_bar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(Hns::ZERO), "0:00");
        assert_eq!(clock(Hns::from_secs(2)), "0:02");
        assert_eq!(clock(Hns::from_secs(86)), "1:26");
        assert_eq!(clock(Hns::from_secs(3_665)), "1:01:05");
        assert_eq!(clock(Hns::from_secs(-5)), "0:00");
    }

    #[test]
    fn hover_frac_respects_bar_bounds_and_insets() {
        assert_eq!(hover_frac_in_rect(50.0, 15.0, 10.0, 0.0, 0.0, 30.0), None);
        assert_eq!(hover_frac_in_rect(10.0, 15.0, 10.0, 0.0, 200.0, 30.0), Some(0.0));
        assert_eq!(hover_frac_in_rect(110.0, 15.0, 10.0, 0.0, 200.0, 30.0), Some(0.5));
        assert_eq!(hover_frac_in_rect(210.0, 15.0, 10.0, 0.0, 200.0, 30.0), Some(1.0));
        // Inside horizontal inset clamps to track endpoints.
        assert_eq!(hover_frac_in_rect(5.0, 15.0, 10.0, 0.0, 200.0, 30.0), Some(0.0));
        assert_eq!(hover_frac_in_rect(215.0, 15.0, 10.0, 0.0, 200.0, 30.0), Some(1.0));
        // Outside vertically or beyond inset yields None so tooltips clear cleanly.
        assert_eq!(hover_frac_in_rect(110.0, -5.0, 10.0, 0.0, 200.0, 30.0), None);
        assert_eq!(hover_frac_in_rect(110.0, 40.0, 10.0, 0.0, 200.0, 30.0), None);
        assert_eq!(hover_frac_in_rect(-20.0, 15.0, 10.0, 0.0, 200.0, 30.0), None);
    }

    #[test]
    fn tooltip_clamps_inside_track_edges() {
        assert_eq!(clamped_tooltip_left(0.0, 400.0), 0.0);
        assert_eq!(clamped_tooltip_left(1.0, 400.0), 400.0 - TOOLTIP_W);
        assert_eq!(clamped_tooltip_left(0.5, 400.0), 200.0 - TOOLTIP_W / 2.0);
        assert_eq!(clamped_tooltip_left(0.5, 30.0), 0.0);
    }

    #[test]
    fn compact_controls_activate_at_min_window_size() {
        assert!(is_compact_controls(760.0, false));
        assert!(is_compact_controls(760.0, true));
        assert!(!is_compact_controls(1_936.0, false));
        assert!(!is_compact_controls(1_936.0, true));
        assert!(is_compact_controls(1_000.0, true));
        assert!(!is_compact_controls(1_000.0, false));
    }

    #[test]
    fn volume_math_converts_percent_clamps_and_respects_mute() {
        assert_eq!(clamp_volume(0), 0);
        assert_eq!(clamp_volume(73), 73);
        assert_eq!(clamp_volume(100), 100);
        assert_eq!(clamp_volume(150), 100);
        assert_eq!(clamp_volume(u8::MAX), 100);

        // Mute always silences output regardless of stored percentage.
        assert_eq!(volume_gain(100, true), 0.0);
        assert_eq!(volume_gain(50, true), 0.0);
        assert_eq!(effective_volume_percent(80, true), 0);
        assert_eq!(volume_to_fraction(80, true), 0.0);

        // Unmuted endpoints and perceptual curve monotonicity.
        assert_eq!(volume_gain(0, false), 0.0);
        assert!((volume_gain(50, false) - 0.25).abs() < 1e-6);
        assert_eq!(volume_gain(100, false), 1.0);
        assert_eq!(volume_gain(200, false), 1.0);
        assert!(volume_gain(25, false) < volume_gain(50, false));
        assert!(volume_gain(50, false) < volume_gain(75, false));
        assert!(volume_gain(75, false) < volume_gain(100, false));

        // Keyboard +/-5% stepping clamps at 0 and 100.
        assert_eq!(step_volume(0, -VOLUME_STEP), 0);
        assert_eq!(step_volume(3, -VOLUME_STEP), 0);
        assert_eq!(step_volume(50, VOLUME_STEP), 55);
        assert_eq!(step_volume(50, -VOLUME_STEP), 45);
        assert_eq!(step_volume(98, VOLUME_STEP), 100);
        assert_eq!(step_volume(150, VOLUME_STEP), 100);

        // Slider fraction conversions handle boundaries and non-finite inputs.
        assert_eq!(fraction_to_volume(-0.2), 0);
        assert_eq!(fraction_to_volume(0.0), 0);
        assert_eq!(fraction_to_volume(0.5), 50);
        assert_eq!(fraction_to_volume(1.0), 100);
        assert_eq!(fraction_to_volume(1.8), 100);
        assert_eq!(fraction_to_volume(f32::NAN), 0);
        assert_eq!(volume_to_fraction(75, false), 0.75);
        assert_eq!(volume_to_fraction(140, false), 1.0);
    }

    #[test]
    fn volume_icon_reflects_mute_low_and_high_levels() {
        assert_eq!(volume_icon(100, true), "volume-x");
        assert_eq!(volume_icon(0, false), "volume-x");
        assert_eq!(volume_icon(1, false), "volume-1");
        assert_eq!(volume_icon(50, false), "volume-1");
        assert_eq!(volume_icon(51, false), "volume-2");
        assert_eq!(volume_icon(100, false), "volume-2");
    }

    #[test]
    fn scale_pcm_chunk_ramps_smoothly_and_silences_mute() {
        let mut muted = [0.5_f32, -0.5, 0.25, -0.25];
        scale_pcm_chunk(&mut muted, 0.0, 0.0, 2);
        assert_eq!(muted, [0.0, 0.0, 0.0, 0.0]);

        let mut ramp = [1.0_f32, -1.0, 1.0, -1.0, 1.0, -1.0];
        scale_pcm_chunk(&mut ramp, 0.0, 1.0, 2);
        assert_eq!(ramp[0], 0.0);
        assert_eq!(ramp[1], 0.0);
        assert!((ramp[2] - 0.5).abs() < 1e-6);
        assert!((ramp[3] + 0.5).abs() < 1e-6);
        assert_eq!(ramp[4], 1.0);
        assert_eq!(ramp[5], -1.0);

        let mut bad = [f32::NAN, f32::INFINITY, 2.0, -2.0];
        scale_pcm_chunk(&mut bad, 1.0, 1.0, 2);
        assert_eq!(bad, [0.0, 0.0, 1.0, -1.0]);
    }

    #[test]
    fn build_audio_tracks_prefers_metadata_and_falls_back_to_defaults() {
        assert!(build_audio_tracks(0, &[]).is_empty());
        assert_eq!(build_audio_tracks(1, &[]), vec!["Master"]);
        assert_eq!(
            build_audio_tracks(4, &["Master".into(), "".into(), "Voice".into(), "Discord".into()]),
            vec!["Master", "Game", "Voice", "Discord"]
        );
    }

    #[test]
    fn stop_button_is_disabled_when_unloaded_or_stopped_at_zero() {
        assert!(!can_stop(false, false, Hns::ZERO));
        assert!(!can_stop(false, true, Hns::from_secs(5)));
        assert!(!can_stop(true, false, Hns::ZERO));
        assert!(!can_stop(true, false, Hns::from_secs(-1)));
        assert!(can_stop(true, true, Hns::ZERO));
        assert!(can_stop(true, true, Hns::from_secs(5)));
        assert!(can_stop(true, false, Hns::from_millis(1)));
    }
}
