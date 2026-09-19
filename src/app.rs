//! The egui application: owns the engine handle and the UI state.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use zc_core::control::{Command, EngineEvent, EngineStatus};
use zc_core::settings::Settings;
use zc_engine::{Engine, EngineConfig, EngineHandle};

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

        Ok(Self {
            engine,
            paths,
            draft: ui::settings::Draft::from_settings(&settings),
            settings,
            status: EngineStatus::default(),
            tab: Tab::Home,
            notices: VecDeque::new(),
            clips: ui::clips::ClipList::default(),
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
        ui::show(self, ui);
    }
}
