//! The egui application: owns the engine handle and the UI state.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use eframe::egui;
use zc_core::control::{Command, EngineEvent, EngineStatus};
use zc_core::settings::Settings;
use zc_engine::{Engine, EngineConfig, EngineHandle};
use zc_mux::repair::{Repaired, repair_partial};

use crate::paths::AppPaths;
use crate::ui;

/// The top-level tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Home,
    Clips,
    Profiles,
    Settings,
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
    pub open_apps: ui::components::open_apps::OpenAppsList,
    /// Results from the startup repair thread, moved into `notices` as they arrive.
    repairs: mpsc::Receiver<Notice>,
}

impl ZeroClipApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        paths: AppPaths,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        // The engine wakes the UI whenever it has news; otherwise egui sleeps.
        let ctx = cc.egui_ctx.clone();
        let wake = Arc::new(move |()| ctx.request_repaint());
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
        spawn_repair(output_dir, repairs_tx, cc.egui_ctx.clone());

        Ok(Self {
            engine,
            paths,
            draft: ui::settings::Draft::from_settings(&settings),
            settings,
            status: EngineStatus::default(),
            tab: Tab::Home,
            notices: VecDeque::new(),
            clips: ui::clips::ClipList::default(),
            open_apps: Default::default(),
            repairs,
        })
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

    /// Moves the startup repair's notices into the activity feed.
    fn drain_repair_notices(&mut self) {
        while let Ok(notice) = self.repairs.try_recv() {
            self.notify(notice.level, notice.text);
        }
    }

    fn drain_engine_events(&mut self) {
        let events: Vec<EngineEvent> = self.engine.poll_events().collect();
        for event in events {
            match event {
                EngineEvent::Status(status) => self.status = status,
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
                // P4.12 replaces this notice with the keep-or-discard prompt.
                EngineEvent::RecoveryReady { path, .. } => {
                    self.notify(
                        NoticeLevel::Warning,
                        format!("Rescued footage: {}", path.display()),
                    );
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

impl eframe::App for ZeroClipApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_engine_events();
        self.drain_repair_notices();
        ui::show(self, ui);
    }
}

/// Runs [`repair_partials`] on a background thread and sends its notices to the UI. It asks for
/// a repaint at the end, so the notices show even when the window is idle.
fn spawn_repair(dir: PathBuf, notices: mpsc::Sender<Notice>, ctx: egui::Context) {
    let spawned = std::thread::Builder::new().name("zc-repair".into()).spawn(move || {
        for notice in repair_partials(&dir) {
            if notices.send(notice).is_err() {
                return; // the app has closed
            }
        }
        ctx.request_repaint();
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "couldn't start the startup repair");
    }
}

/// Repairs every `*.partial` file in `dir` (D18) and returns one notice per file. A folder that
/// doesn't exist yet has nothing to repair.
fn repair_partials(dir: &Path) -> Vec<Notice> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, dir = %dir.display(), "startup repair can't read the output folder");
            }
            return Vec::new();
        }
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "partial") && path.is_file())
        .map(|path| repair_notice(&path))
        .collect()
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
        let notices = repair_partials(&dir);
        let missing = repair_partials(&dir.join("missing"));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].level, NoticeLevel::Warning);
        assert!(notices[0].text.contains("broken.mp4.partial"), "{}", notices[0].text);
        assert!(missing.is_empty());
    }
}
