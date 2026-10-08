//! Messages between the UI and the engine.
//!
//! The UI (and the hotkey matcher) send [`Command`]s; the engine answers with
//! [`EngineEvent`]s. Both are plain data so the engine stays testable without a UI.

use std::path::PathBuf;

use crate::media::Codec;
use crate::settings::Settings;
use crate::time::Hns;
use crate::watchdog::WatchdogState;

/// Requests to the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Write the last N seconds of the DVR buffer (N from the active profile) to a clip file.
    SaveClip,
    /// Start or stop a manual recording.
    ToggleRecording,
    /// Save a PNG of the current frame.
    Screenshot,
    /// The push-to-talk key went down (`true`) or up (`false`).
    PushToTalk(bool),
    /// Settings changed; apply what can be applied live.
    ApplySettings(Box<Settings>),
    /// Stop all sources and exit the engine thread.
    Shutdown,
}

/// Notifications from the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineEvent {
    /// A fresh status snapshot.
    Status(EngineStatus),
    /// A DVR clip was written.
    ClipSaved {
        /// Where.
        path: PathBuf,
        /// How long.
        duration: Hns,
    },
    /// A screenshot was written.
    ScreenshotSaved {
        /// Where.
        path: PathBuf,
    },
    /// A manual or session recording started.
    RecordingStarted {
        /// The file being written.
        path: PathBuf,
    },
    /// A manual or session recording finished.
    RecordingStopped {
        /// The finished file.
        path: PathBuf,
        /// Its length.
        duration: Hns,
    },
    /// A game profile was applied after a focus change.
    ProfileApplied {
        /// Executable that got focus.
        exe: String,
        /// DVR window now in effect.
        window: Hns,
        /// Whether the window was reduced to respect the RAM ceiling.
        clamped: bool,
    },
    /// Something the user should know about, but not an error.
    Warning(String),
    /// Something failed.
    Error(String),
    /// A requested feature isn't built yet. Scaffolding only: this disappears as
    /// the roadmap phases land.
    NotImplemented {
        /// What was requested.
        feature: &'static str,
        /// Roadmap phase that delivers it.
        phase: u8,
    },
}

/// What the engine is doing right now, for the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineStatus {
    /// Frames are being captured and encoded.
    pub capturing: bool,
    /// A manual or session recording is being written.
    pub recording: bool,
    /// Codec in use, once the capability ladder has run.
    pub codec: Option<Codec>,
    /// Footage currently in the DVR buffer.
    pub buffered: Hns,
    /// The DVR window the active profile asks for.
    pub buffer_window: Hns,
    /// Watchdog state.
    pub watchdog: WatchdogState,
    /// Executable of the focused game, if any.
    pub game: Option<String>,
}
