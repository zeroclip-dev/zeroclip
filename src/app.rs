//! The GPUI application root: owns the engine handle and the UI state.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use gpui::{Context, FocusHandle, IntoElement, PathPromptOptions, Pixels, Point, Render, Window};
use zc_core::control::{Command, EngineEvent, EngineStatus, FocusChoice};
use zc_core::settings::{CaptureMode, Settings, UiMode};
use zc_engine::{Engine, EngineConfig, EngineHandle};
use zc_mux::repair::{Repaired, repair_partial};

use crate::paths::AppPaths;
use crate::tray::{self, TrayCommand};
use crate::ui;
use crate::ui::components::screens::Screens;

/// The top-level tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Home,
    Clips,
    Profiles,
    Editor,
    Settings,
    /// Not in the top bar: reached by playing a clip.
    Player,
    /// Not in the top bar: reached by viewing a screenshot.
    Viewer,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Home, Tab::Clips, Tab::Profiles, Tab::Editor, Tab::Settings];

    /// The Lucide icon for this tab's top-bar button.
    pub fn icon(self) -> &'static str {
        match self {
            Tab::Home => "house",
            Tab::Clips => "film",
            Tab::Profiles => "sliders-horizontal",
            Tab::Editor => "scissors",
            Tab::Settings => "settings",
            Tab::Player => "play",
            Tab::Viewer => "image",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Clips => "Clips",
            Tab::Profiles => "Profiles",
            Tab::Editor => "Editor",
            Tab::Settings => "Settings",
            Tab::Player => "Player",
            Tab::Viewer => "Viewer",
        }
    }
}

/// How a line in the activity feed is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// A line in the activity feed.
#[derive(Clone, Debug)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
}

const MAX_NOTICES: usize = 50;

pub struct ZeroClipApp {
    /// One view per tab; see `ui/components/screens.rs`.
    pub screens: Screens,
    pub engine: EngineHandle,
    pub paths: AppPaths,
    /// Settings as saved to disk and applied to the engine.
    pub settings: Settings,
    /// Settings as being edited in the UI.
    pub draft: ui::settings::Draft,
    /// Latest status from the engine.
    pub status: EngineStatus,
    pub tab: Tab,
    /// Newest first.
    pub notices: VecDeque<Notice>,
    pub clips: ui::clips::ClipList,
    /// The clip the right-click menu is open for, and where it was opened. Any screen sets it
    /// through [`Self::open_clip_menu`]; the shell draws it (`ui/clip_menu.rs`).
    pub clip_menu: Option<(PathBuf, Point<Pixels>)>,
    /// Focus for the open menu, so Esc reaches it.
    pub menu_focus: FocusHandle,
    /// The metadata database (waveform cache). `None` when it couldn't be opened.
    pub store: Option<Arc<Mutex<zc_store::Store>>>,
    pub open_apps: ui::components::open_apps::OpenAppsList,
    /// The focus prompt on screen, if a second game asked to take over (D35).
    pub focus_prompt: Option<ui::focus_prompt::FocusPrompt>,
    /// Footage waiting for Keep / Discard / Later (P4.12); the first one is on screen.
    pub rescues: Vec<ui::rescue::Rescue>,
    /// Recent log lines, dumped next to a rescue (P4.13).
    flight: Arc<crate::logging::FlightRecorder>,
    /// The startup scan's result, moved into `notices` and `rescues` when it arrives.
    repairs: mpsc::Receiver<StartupScan>,
    /// The tray icon (P4.15). `None` when it couldn't be created: the app then quits on close.
    tray: Option<tray::Tray>,
    /// Set by the tray's Quit, so the window closes for real instead of hiding.
    quitting: bool,
    /// The "keeps running in the tray" notice is shown once per session.
    tray_hint_shown: bool,
    /// Which failures the overlay popup has shown lately, so a flapping device can't storm it.
    alerts: ui::overlay::Coalescer,
}

impl ZeroClipApp {
    pub fn new(
        paths: AppPaths,
        settings: Settings,
        flight: Arc<crate::logging::FlightRecorder>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Self> {
        // The engine (and the repair thread) wake the UI by sending on `wake_tx`; the task below
        // drains events and repaints. Otherwise the UI sleeps.
        let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
        let wake = {
            let wake_tx = wake_tx.clone();
            Arc::new(move |()| {
                let _ = wake_tx.try_send(());
            })
        };
        let config = EngineConfig {
            settings: settings.clone(),
            default_output_dir: paths.default_output_dir.clone(),
        };
        let engine = Engine::spawn(config, crate::wiring::ports(&settings), wake)?;

        // Repairing reads the output folder, so it runs off the UI thread (D18).
        let (repairs_tx, repairs) = mpsc::channel();
        let output_dir = settings
            .recording
            .output_dir
            .clone()
            .unwrap_or_else(|| paths.default_output_dir.clone());
        // The tray's menu and icon wake the same task, so its commands drain with the window hidden.
        let tray_wake: Arc<dyn Fn() + Send + Sync> = {
            let wake_tx = wake_tx.clone();
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            })
        };
        spawn_repair(output_dir, repairs_tx, wake_tx);

        let mut notices = VecDeque::new();
        let tray = match tray::create(tray_wake) {
            Ok(tray) => Some(tray),
            Err(error) => {
                tracing::warn!(%error, "the tray icon couldn't start");
                notices.push_front(Notice {
                    level: NoticeLevel::Warning,
                    text: format!("The tray icon couldn't start: {error:#}"),
                });
                None
            }
        };
        if tray.is_none() {
            // `--tray` starts the window hidden. With no tray icon there would be no way back, so show it.
            cx.spawn(async move |_, _| {
                let _ = zc_win::main_window::set_shown(true);
            })
            .detach();
        }

        cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_ok() {
                // Several wakes can pile up; one drain handles them all.
                while wake_rx.try_recv().is_ok() {}
                let alive = this.update(cx, |app, cx| {
                    app.drain_engine_events(cx);
                    app.drain_repair_notices();
                    app.drain_tray(cx);
                    cx.notify();
                });
                if alive.is_err() {
                    return; // the app has closed
                }
            }
        })
        .detach();

        let store = match open_store(&paths.db_file) {
            Ok(store) => Some(Arc::new(Mutex::new(store))),
            Err(error) => {
                notices.push_front(Notice {
                    level: NoticeLevel::Error,
                    text: format!("Clip metadata unavailable: {error:#}"),
                });
                None
            }
        };

        Ok(Self {
            screens: Screens::new(cx.weak_entity(), cx),
            engine,
            paths,
            draft: ui::settings::Draft::from_settings(&settings),
            settings,
            status: EngineStatus::default(),
            tab: Tab::Home,
            notices,
            tray,
            quitting: false,
            tray_hint_shown: false,
            alerts: ui::overlay::Coalescer::default(),
            clips: ui::clips::ClipList::default(),
            clip_menu: None,
            menu_focus: cx.focus_handle(),
            store,
            open_apps: Default::default(),
            focus_prompt: None,
            rescues: Vec::new(),
            flight,
            repairs,
        })
    }

    /// Opens `path` in the Editor tab: as a plain player when `watch`, else ready to edit (the Clips
    /// tab's Watch and Edit buttons).
    pub fn open_in_editor(&mut self, path: PathBuf, watch: bool, cx: &mut Context<Self>) {
        let ffmpeg = self.settings.editor.ffmpeg_path.clone();
        let store = self.store.clone();
        let bitrate_bps = self.settings.audio.bitrate_kbps * 1_000;
        let prefs = (self.settings.editor.finish, usize::from(self.settings.editor.listen_track));
        self.screens.editor().update(cx, |editor, cx| {
            editor.open(path, watch, Some(prefs), ffmpeg, store, bitrate_bps, cx)
        });
        self.tab = Tab::Editor;
        cx.notify();
    }

    /// Opens the clip menu for `path` at `at` (window coordinates).
    pub fn open_clip_menu(
        &mut self,
        path: PathBuf,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clip_menu = Some((path, at));
        self.menu_focus.focus(window, cx);
        cx.notify();
    }

    /// Plays `path` in the video player.
    pub fn play_clip(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let ffmpeg = self.settings.editor.ffmpeg_path.clone();
        self.screens.player().update(cx, |player, cx| player.open(path, ffmpeg, cx));
        self.tab = Tab::Player;
        cx.notify();
    }

    /// Opens the screenshot at `index` inside `screenshots` in the full-size image viewer.
    pub fn view_screenshot(
        &mut self,
        screenshots: Vec<ui::clips::ClipFile>,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.screens.viewer().update(cx, |viewer, cx| viewer.open(screenshots, index, cx));
        self.tab = Tab::Viewer;
        cx.notify();
    }

    /// Saves an editor preference (the Done choice, the listened-to track) at once. It writes the
    /// file and nothing else: no engine restart, no notice, and the Settings screen keeps its
    /// unsaved edits.
    pub fn remember_editor(&mut self, f: impl FnOnce(&mut zc_core::settings::EditorSettings)) {
        let mut settings = self.settings.clone();
        f(&mut settings.editor);
        if settings.editor == self.settings.editor {
            return;
        }
        if let Err(e) = crate::settings_store::save(&self.paths.settings_file, &settings) {
            self.notify(NoticeLevel::Error, format!("Couldn't save settings: {e:#}"));
            return;
        }
        self.draft.settings.editor.finish = settings.editor.finish;
        self.draft.settings.editor.listen_track = settings.editor.listen_track;
        self.settings = settings;
    }

    /// Where recordings are written right now.
    pub fn output_dir(&self) -> PathBuf {
        self.settings
            .recording
            .output_dir
            .clone()
            .unwrap_or_else(|| self.paths.default_output_dir.clone())
    }

    pub fn send(&mut self, command: Command) {
        if let Err(e) = self.engine.send(command) {
            self.notify(NoticeLevel::Error, e.to_string());
        }
    }

    pub fn notify(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notices.push_front(Notice { level, text: text.into() });
        self.notices.truncate(MAX_NOTICES);
    }

    /// Validates the draft, writes it to disk and applies it to the engine.
    pub fn save_settings(&mut self) {
        let settings = match self.draft.build() {
            Ok(settings) => settings,
            Err(problems) => {
                self.draft.problems = problems;
                return;
            }
        };
        self.apply_settings(settings);
    }

    /// Writes `settings` to disk, then applies them to the engine and the draft.
    fn apply_settings(&mut self, settings: Settings) {
        if let Err(e) = crate::settings_store::save(&self.paths.settings_file, &settings) {
            self.notify(NoticeLevel::Error, format!("Couldn't save settings: {e:#}"));
            return;
        }
        if settings.app.start_with_windows != self.settings.app.start_with_windows
            && let Err(error) = set_start_with_windows(settings.app.start_with_windows)
        {
            self.notify(
                NoticeLevel::Error,
                format!("Couldn't change start with Windows: {error:#}"),
            );
        }
        self.draft = ui::settings::Draft::from_settings(&settings);
        self.settings = settings.clone();
        self.clips.invalidate(); // the output folder may have changed
        self.send(Command::ApplySettings(Box::new(settings)));
        self.notify(NoticeLevel::Info, "Settings saved.");
    }

    /// Switches between Simple and Advanced and saves the choice like any other setting. The
    /// Settings draft keeps its unsaved edits, as in [`Self::clip_desktop`].
    pub fn set_ui_mode(&mut self, mode: UiMode) {
        if self.settings.ui_mode == mode {
            return;
        }
        let mut settings = self.settings.clone();
        settings.ui_mode = mode;
        let draft = self.draft.clone();
        self.apply_settings(settings);
        self.draft = draft;
        self.draft.settings.ui_mode = mode;
    }

    /// Asks where clips should be saved and switches to it at once. Settings edits that are not
    /// saved yet stay in the draft, as in [`Self::set_ui_mode`].
    pub fn choose_output_dir(&mut self, cx: &mut Context<Self>) {
        let current = self.output_dir();
        pick_folder(cx, move |app, dir, cx| {
            if dir == current {
                return;
            }
            let mut settings = app.settings.clone();
            settings.recording.output_dir = Some(dir.clone());
            let draft = app.draft.clone();
            app.apply_settings(settings);
            app.draft = draft;
            app.draft.output_dir = dir.display().to_string();
            app.notify(NoticeLevel::Info, format!("New clips go to {}", dir.display()));
            cx.notify();
        });
    }

    /// The window's close button (P4.15). With close-to-tray on and a tray icon to come back from,
    /// the window hides and the app keeps running. Otherwise the window closes and the app quits.
    /// Returns whether the window may close.
    pub fn close_requested(&mut self, cx: &mut Context<Self>) -> bool {
        if self.quitting || !self.settings.app.close_to_tray || self.tray.is_none() {
            return true;
        }
        if let Err(error) = zc_win::main_window::set_shown(false) {
            self.notify(
                NoticeLevel::Error,
                format!("Couldn't hide the window to the tray: {error}"),
            );
            return false;
        }
        if !self.tray_hint_shown {
            self.tray_hint_shown = true;
            self.notify(NoticeLevel::Info, "ZeroClip keeps running in the tray.");
        }
        cx.notify();
        false
    }

    /// Handles the tray's menu and icon commands (P4.15).
    fn drain_tray(&mut self, cx: &mut Context<Self>) {
        let commands: Vec<TrayCommand> = match &self.tray {
            Some(tray) => tray.commands.try_iter().collect(),
            None => return,
        };
        for command in commands {
            match command {
                TrayCommand::Show => {
                    if let Err(error) = zc_win::main_window::set_shown(true) {
                        self.notify(
                            NoticeLevel::Error,
                            format!("Couldn't show the window: {error}"),
                        );
                    }
                    cx.activate(true);
                }
                TrayCommand::SaveClip => self.send(Command::SaveClip),
                TrayCommand::Quit => {
                    self.quitting = true;
                    cx.quit();
                }
            }
        }
    }

    /// The focus prompt's "Clip desktop" (D35): the capture mode becomes the whole screen, the
    /// same as choosing it in Settings.
    pub fn clip_desktop(&mut self) {
        let mut settings = self.settings.clone();
        settings.video.capture_mode = CaptureMode::Screen;
        // apply_settings replaces the draft; keep the user's unsaved Settings-tab edits.
        let draft = self.draft.clone();
        self.apply_settings(settings);
        self.draft = draft;
        self.draft.settings.video.capture_mode = CaptureMode::Screen;
    }

    /// Moves the startup repair's notices into the activity feed.
    fn drain_repair_notices(&mut self) {
        while let Ok(scan) = self.repairs.try_recv() {
            for notice in scan.notices {
                self.notify(notice.level, notice.text);
            }
            for path in scan.rescues {
                self.add_rescue(ui::rescue::Rescue { path, game: None, duration: None });
            }
        }
    }

    /// Queues footage for the prompt, unless that file is already waiting.
    fn add_rescue(&mut self, rescue: ui::rescue::Rescue) {
        if !self.rescues.iter().any(|r| r.path == rescue.path) {
            self.rescues.push(rescue);
        }
    }

    /// Answers the rescue prompt. Whatever happens, the prompt is dropped (no loop on an io error).
    pub fn answer_rescue(&mut self, choice: ui::rescue::RescueChoice) {
        if self.rescues.is_empty() {
            return;
        }
        let rescue = self.rescues.remove(0);
        let (notice, kept) = ui::rescue::resolve(&rescue.path, choice);
        if kept {
            self.clips.invalidate();
        }
        if let Some(notice) = notice {
            self.notify(notice.level, notice.text);
        }
    }

    /// Answers the focus prompt (D35). Keep and Switch tell the engine; Clip desktop changes the
    /// capture mode. Any answer closes the banner.
    pub fn answer_focus_prompt(&mut self, choice: FocusChoice) {
        self.focus_prompt = None;
        self.send(Command::FocusChoice(choice));
    }

    /// Tells a player in a game that something failed: the overlay popup, and for an error a
    /// Windows notification when ZeroClip is not the window in front. Repeats are coalesced.
    fn popup(&mut self, level: ui::overlay::AlertLevel, text: &str, cx: &mut gpui::App) {
        let Some(count) = self.alerts.admit(std::time::Instant::now(), level, text) else {
            return;
        };
        ui::overlay::show_alert(cx, level, text, count);
        if level == ui::overlay::AlertLevel::Error
            && count == 1
            && !zc_win::notify::app_is_foreground()
        {
            let (title, detail) = ui::overlay::split_alert(level, text);
            if let Err(error) = zc_win::notify::error(&title, &detail) {
                tracing::warn!(%error, "desktop notification not shown");
            }
        }
    }

    /// Puts a toast on screen, replacing the one that is up. Off when the setting is off.
    fn show_toast(&mut self, kind: ui::overlay::ToastKind, cx: &mut gpui::App) {
        if !self.settings.overlay_toasts {
            return;
        }
        use zc_win::chime::Chime;
        // The chime waits for the clip to be written.
        let chime = match kind {
            ui::overlay::ToastKind::Saving => None,
            ui::overlay::ToastKind::ClipSaved { .. } => Some(Chime::ClipSaved),
            ui::overlay::ToastKind::RecordingStarted => Some(Chime::RecordingStarted),
            ui::overlay::ToastKind::RecordingStopped { .. } => Some(Chime::RecordingStopped),
            ui::overlay::ToastKind::ScreenshotTaken => Some(Chime::Screenshot),
            ui::overlay::ToastKind::Bookmarked { .. } => None,
        };
        if let Some(chime) = chime {
            zc_win::chime::play(chime);
        }
        ui::overlay::show(cx, kind);
    }

    /// Rewrites a just-saved clip as a plain MP4 in the background, so any player can seek in it.
    fn finish_clip(&self, path: PathBuf, cx: &mut Context<Self>) {
        if path.extension().is_none_or(|e| !e.eq_ignore_ascii_case("mp4")) {
            return;
        }
        let ffmpeg = self.settings.editor.ffmpeg_path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ui::clips::make_plain_in_place(ffmpeg.as_deref(), &path) })
                .await;
            let _ = this.update(cx, |app, cx| {
                match result {
                    Ok(()) => app.clips.invalidate(),
                    Err(e) => {
                        tracing::warn!("couldn't make the clip seekable in other players: {e}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Records which game `clip` came from (the game in focus now) so Clips can group by game.
    fn tag_game(&self, clip: &Path) {
        if let Some(game) = &self.status.game {
            let name =
                game.strip_suffix(".exe").or_else(|| game.strip_suffix(".EXE")).unwrap_or(game);
            zc_core::game_tag::save(clip, name);
        }
    }

    fn drain_engine_events(&mut self, cx: &mut Context<Self>) {
        use ui::overlay::ToastKind;
        let events: Vec<EngineEvent> = self.engine.poll_events().collect();
        let mut raw_keys = Vec::new();
        for event in events {
            match event {
                EngineEvent::Status(status) => self.status = status,
                EngineEvent::FocusPrompt { current, candidate } => {
                    self.focus_prompt = Some(ui::focus_prompt::FocusPrompt { current, candidate });
                    // ponytail: egui flashed the taskbar here; GPUI has no equivalent yet.
                    // Never raise or activate the window (D35).
                }
                EngineEvent::ClipSaving => self.show_toast(ToastKind::Saving, cx),
                EngineEvent::ClipSaved { path, duration } => {
                    self.tag_game(&path);
                    self.finish_clip(path.clone(), cx);
                    self.show_toast(ToastKind::ClipSaved { length: duration }, cx);
                    self.clips.invalidate();
                    self.notify(
                        NoticeLevel::Info,
                        format!("Saved a {duration} clip: {}", path.display()),
                    );
                }
                EngineEvent::BookmarkAdded { at } => {
                    self.show_toast(ToastKind::Bookmarked { at }, cx);
                    self.notify(
                        NoticeLevel::Info,
                        format!("Bookmarked {}", ui::human_duration(at)),
                    );
                }
                EngineEvent::ScreenshotSaved { path } => {
                    self.show_toast(ToastKind::ScreenshotTaken, cx);
                    self.clips.invalidate();
                    self.notify(NoticeLevel::Info, format!("Screenshot saved: {}", path.display()));
                }
                EngineEvent::RecordingStarted { path } => {
                    self.show_toast(ToastKind::RecordingStarted, cx);
                    self.notify(NoticeLevel::Info, format!("Recording to {}", path.display()));
                }
                EngineEvent::RecordingStopped { path, duration } => {
                    self.tag_game(&path);
                    self.finish_clip(path.clone(), cx);
                    self.show_toast(ToastKind::RecordingStopped { length: duration }, cx);
                    self.clips.invalidate();
                    self.notify(
                        NoticeLevel::Info,
                        format!("Recorded {duration}: {}", path.display()),
                    );
                }
                EngineEvent::ProfileApplied { exe, window, clamped } => {
                    let note = if clamped { " (reduced to fit the memory limit)" } else { "" };
                    self.notify(
                        NoticeLevel::Info,
                        format!("{exe}: keeping the last {}{note}", ui::human_duration(window)),
                    );
                }
                EngineEvent::RecoveryReady { path, game, duration } => {
                    // A failed dump is a log line, never a blocked prompt.
                    let log_note = match self.flight.dump_to_dir(&self.paths.logs_dir, "") {
                        Ok(log) => format!(
                            " Diagnostics: {}",
                            log.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Err(error) => {
                            tracing::warn!(%error, "couldn't dump the flight recorder");
                            String::new()
                        }
                    };
                    self.notify(
                        NoticeLevel::Warning,
                        format!("{game} closed unexpectedly; its footage was saved.{log_note}"),
                    );
                    self.add_rescue(ui::rescue::Rescue {
                        path,
                        game: Some(game),
                        duration: Some(duration),
                    });
                    // ponytail: egui flashed the taskbar here; GPUI has no equivalent yet.
                    // Never raise or activate the window (the user may be relaunching the game).
                }
                EngineEvent::CaptureKey { key, down } => raw_keys.push((key, down)),
                EngineEvent::Warning(text) => {
                    self.popup(ui::overlay::AlertLevel::Warning, &text, cx);
                    self.notify(NoticeLevel::Warning, text);
                }
                EngineEvent::Error(text) => {
                    self.popup(ui::overlay::AlertLevel::Error, &text, cx);
                    self.notify(NoticeLevel::Error, text);
                }
                EngineEvent::NotImplemented { feature, phase } => self.notify(
                    NoticeLevel::Info,
                    format!("{feature}: not built yet (arrives in Phase {phase})."),
                ),
            }
        }
        if !raw_keys.is_empty() {
            // Deferred: the view sends engine commands through this entity, which is mid-update.
            let settings = self.screens.settings().clone();
            cx.defer(move |cx| {
                settings.update(cx, |view, cx| {
                    for (key, down) in raw_keys {
                        view.on_capture_key(key, down, cx);
                    }
                });
            });
        }
    }
}

impl Render for ZeroClipApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ui::focus_prompt::clear_if_left(self);
        // A hotkey field that was listening when the user left Settings (by any route) must not
        // leave the global hotkeys suspended. Deferred: the view updates this entity.
        if self.tab != Tab::Settings {
            let settings = self.screens.settings().clone();
            if settings.read(cx).is_listening() {
                cx.defer(move |cx| settings.update(cx, |view, cx| view.leave(cx)));
            }
        }
        ui::shell(self, window, cx)
    }
}

/// What the startup pass over the output folder found.
#[derive(Default)]
struct StartupScan {
    /// One per repaired `*.partial` file.
    notices: Vec<Notice>,
    /// `recovery-*.tmp` files the user hasn't answered for yet.
    rescues: Vec<PathBuf>,
}

/// Adds or removes ZeroClip's Run value, so Windows starts it at sign-in (P4.16).
fn set_start_with_windows(enabled: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    zc_win::autostart::set(enabled, &zc_win::autostart::run_value(&exe))?;
    Ok(())
}

/// Runs [`startup_scan`] on a background thread and sends the result to the UI. It asks for
/// a repaint at the end, so the result shows even when the window is idle.
fn spawn_repair(dir: PathBuf, result: mpsc::Sender<StartupScan>, wake: async_channel::Sender<()>) {
    let spawned = std::thread::Builder::new().name("zc-repair".into()).spawn(move || {
        if result.send(startup_scan(&dir)).is_err() {
            return; // the app has closed
        }
        let _ = wake.try_send(());
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "couldn't start the startup repair");
    }
}

const MAX_SCAN_DEPTH: usize = 4;

/// One pass over `dir`: repairs every `*.partial` file (D18, one notice per file) and collects
/// the `recovery-*.tmp` files for the rescue prompt. A folder that doesn't exist yet has nothing.
fn startup_scan(dir: &Path) -> StartupScan {
    let mut scan = StartupScan::default();
    if let Err(error) = walk_startup_scan(dir, 0, &mut scan)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, dir = %dir.display(), "startup repair can't read the output folder");
    }
    scan
}

/// Walks `dir` up to `MAX_SCAN_DEPTH` sub-folders deep for partial and rescue files.
fn walk_startup_scan(dir: &Path, depth: usize, scan: &mut StartupScan) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth < MAX_SCAN_DEPTH {
                let _ = walk_startup_scan(&path, depth + 1, scan);
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "partial") {
            scan.notices.push(repair_notice(&path));
        } else if ui::rescue::kept_name(&path).is_some() {
            scan.rescues.push(path);
        }
    }
    Ok(())
}

/// Repairs one `.partial` file and words the outcome for the activity feed.
fn repair_notice(partial: &Path) -> Notice {
    match repair_partial(partial) {
        Ok(Repaired::Truncated { path, .. }) => {
            let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
            Notice {
                level: NoticeLevel::Info,
                text: format!(
                    "Recovered '{name}' after an unexpected exit; up to about 1 s at the end may be missing."
                ),
            }
        }
        Ok(Repaired::Unrecoverable) => Notice {
            level: NoticeLevel::Warning,
            text: format!(
                "Couldn't recover {}: it has no complete fragment, so it was left as it is.",
                partial.display()
            ),
        },
        Err(error) => Notice {
            level: NoticeLevel::Error,
            text: format!("Couldn't repair {}: {error}", partial.display()),
        },
    }
}

/// Opens the metadata database, creating its folder first.
fn open_store(path: &Path) -> anyhow::Result<zc_store::Store> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(zc_store::Store::open(path)?)
}

/// Opens the system folder picker and calls `then` with the chosen folder. GPUI runs the dialog
/// off the event handler; a blocking `IFileOpenDialog::Show` inside a click handler re-enters
/// GPUI's window while it is borrowed and aborts the app.
pub fn pick_folder<T: 'static>(
    cx: &mut Context<T>,
    then: impl FnOnce(&mut T, std::path::PathBuf, &mut Context<T>) + 'static,
) {
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Select folder".into()),
    });
    cx.spawn(async move |this, cx| match chosen.await {
        Ok(Ok(Some(mut paths))) => {
            if let Some(dir) = paths.pop() {
                let _ = this.update(cx, |this, cx| then(this, dir, cx));
            }
        }
        Ok(Err(error)) => tracing::warn!(%error, "folder picker failed"),
        _ => {}
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_repair_reports_each_partial_file_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("zc-app-repair-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(dir.join("broken.mp4.partial"), b"not a recording").expect("partial file");
        std::fs::write(dir.join("notes.txt"), b"not a recording either").expect("other file");
        std::fs::write(dir.join("recovery-9.mp4.tmp"), b"x").expect("rescue file");
        let scan = startup_scan(&dir);
        let notices = scan.notices;
        let missing = startup_scan(&dir.join("missing"));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].level, NoticeLevel::Warning);
        assert!(notices[0].text.contains("broken.mp4.partial"), "{}", notices[0].text);
        assert_eq!(scan.rescues, vec![dir.join("recovery-9.mp4.tmp")]);
        assert!(missing.notices.is_empty() && missing.rescues.is_empty());
    }

    #[test]
    fn startup_scan_recurses_subfolders_up_to_depth_limit() {
        let dir = std::env::temp_dir().join(format!("zc-app-repair-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("Clips").join("Valorant");
        let d4 = dir.join("a").join("b").join("c").join("d");
        let d5 = d4.join("e");
        let hidden = dir.join(".hidden");
        for d in [&sub, &d4, &d5, &hidden] {
            std::fs::create_dir_all(d).expect("scratch dir");
        }
        std::fs::write(sub.join("nested.mp4.partial"), b"not a recording").expect("nested partial");
        std::fs::write(sub.join("recovery-2.mp4.tmp"), b"x").expect("nested rescue");
        std::fs::write(d4.join("d4.mp4.partial"), b"not a recording").expect("d4 partial");
        std::fs::write(d5.join("d5.mp4.partial"), b"not a recording").expect("d5 partial");
        std::fs::write(hidden.join("h.mp4.partial"), b"not a recording").expect("hidden partial");

        let scan = startup_scan(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(scan.notices.len(), 2);
        assert!(scan.notices.iter().any(|n| n.text.contains("nested.mp4.partial")));
        assert!(scan.notices.iter().any(|n| n.text.contains("d4.mp4.partial")));
        assert_eq!(scan.rescues, vec![sub.join("recovery-2.mp4.tmp")]);
    }
}
