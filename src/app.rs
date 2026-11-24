//! The GPUI application root: owns the engine handle and the UI state.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use gpui::{Context, IntoElement, Render, Window};
use zc_core::control::{Command, EngineEvent, EngineStatus, FocusChoice};
use zc_core::settings::{CaptureMode, Settings, UiMode};
use zc_engine::{Engine, EngineConfig, EngineHandle};
use zc_mux::repair::{Repaired, repair_partial};

use crate::paths::AppPaths;
use crate::ui;
use crate::ui::components::screens::Screens;

/// The top-level tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Home,
    Clips,
    Profiles,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Home, Tab::Clips, Tab::Profiles, Tab::Settings];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Clips => "Clips",
            Tab::Profiles => "Profiles",
            Tab::Settings => "Settings",
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
    #[allow(dead_code)] // used by the Profiles screen
    pub open_apps: ui::components::open_apps::OpenAppsList,
    /// The focus prompt on screen, if a second game asked to take over (D35).
    pub focus_prompt: Option<ui::focus_prompt::FocusPrompt>,
    /// Footage waiting for Keep / Discard / Later (P4.12); the first one is on screen.
    pub rescues: Vec<ui::rescue::Rescue>,
    /// Recent log lines, dumped next to a rescue (P4.13).
    flight: Arc<crate::logging::FlightRecorder>,
    /// The startup scan's result, moved into `notices` and `rescues` when it arrives.
    repairs: mpsc::Receiver<StartupScan>,
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
        spawn_repair(output_dir, repairs_tx, wake_tx);

        cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_ok() {
                // Several wakes can pile up; one drain handles them all.
                while wake_rx.try_recv().is_ok() {}
                let alive = this.update(cx, |app, cx| {
                    app.drain_engine_events();
                    app.drain_repair_notices();
                    cx.notify();
                });
                if alive.is_err() {
                    return; // the app has closed
                }
            }
        })
        .detach();

        Ok(Self {
            screens: Screens::new(cx.weak_entity(), cx),
            engine,
            paths,
            draft: ui::settings::Draft::from_settings(&settings),
            settings,
            status: EngineStatus::default(),
            tab: Tab::Home,
            notices: VecDeque::new(),
            clips: ui::clips::ClipList::default(),
            open_apps: Default::default(),
            focus_prompt: None,
            rescues: Vec::new(),
            flight,
            repairs,
        })
    }

    /// Where recordings are written right now.
    #[allow(dead_code)] // used by the Clips screen
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
    #[allow(dead_code)] // used by the Settings screen
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

    fn drain_engine_events(&mut self) {
        let events: Vec<EngineEvent> = self.engine.poll_events().collect();
        for event in events {
            match event {
                EngineEvent::Status(status) => self.status = status,
                EngineEvent::FocusPrompt { current, candidate } => {
                    self.focus_prompt = Some(ui::focus_prompt::FocusPrompt { current, candidate });
                    // ponytail: egui flashed the taskbar here; GPUI has no equivalent yet.
                    // Never raise or activate the window (D35).
                }
                EngineEvent::ClipSaved { path, duration } => {
                    self.clips.invalidate();
                    self.notify(
                        NoticeLevel::Info,
                        format!("Saved a {duration} clip: {}", path.display()),
                    );
                }
                EngineEvent::ScreenshotSaved { path } => {
                    self.clips.invalidate();
                    self.notify(NoticeLevel::Info, format!("Screenshot saved: {}", path.display()));
                }
                EngineEvent::RecordingStarted { path } => {
                    self.notify(NoticeLevel::Info, format!("Recording to {}", path.display()));
                }
                EngineEvent::RecordingStopped { path, duration } => {
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
                EngineEvent::Warning(text) => self.notify(NoticeLevel::Warning, text),
                EngineEvent::Error(text) => self.notify(NoticeLevel::Error, text),
                EngineEvent::NotImplemented { feature, phase } => self.notify(
                    NoticeLevel::Info,
                    format!("{feature}: not built yet (arrives in Phase {phase})."),
                ),
            }
        }
    }
}

impl Render for ZeroClipApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ui::focus_prompt::clear_if_left(self);
        ui::shell(self, cx)
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

/// One pass over `dir`: repairs every `*.partial` file (D18, one notice per file) and collects
/// the `recovery-*.tmp` files for the rescue prompt. A folder that doesn't exist yet has nothing.
fn startup_scan(dir: &Path) -> StartupScan {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, dir = %dir.display(), "startup repair can't read the output folder");
            }
            return StartupScan::default();
        }
    };
    let mut scan = StartupScan::default();
    for path in entries.flatten().map(|entry| entry.path()).filter(|p| p.is_file()) {
        if path.extension().is_some_and(|ext| ext == "partial") {
            scan.notices.push(repair_notice(&path));
        } else if ui::rescue::kept_name(&path).is_some() {
            scan.rescues.push(path);
        }
    }
    scan
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
}
