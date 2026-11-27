//! Messages between the UI and the engine.
//!
//! The UI (and the hotkey matcher) send [`Command`]s; the engine answers with
//! [`EngineEvent`]s. Both are plain data so the engine stays testable without a UI.

use std::path::PathBuf;

use crate::hotkey::Key;
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
    /// Mark this moment of the running recording.
    Bookmark,
    /// Save a PNG of the current frame.
    Screenshot,
    /// The push-to-talk key went down (`true`) or up (`false`).
    PushToTalk(bool),
    /// Settings changed; apply what can be applied live.
    ApplySettings(Box<Settings>),
    /// The user answered the focus prompt. Clip desktop is not a choice here: the UI
    /// sends `ApplySettings` with capture mode Screen.
    FocusChoice(FocusChoice),
    /// Suspend global hotkey dispatch while the UI is capturing a new hotkey binding.
    SetHotkeysSuspended(bool),
    /// Stop all sources and exit the engine thread.
    Shutdown,
}

/// The user's answer to a focus prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusChoice {
    /// Keep clipping the tracked game.
    Keep,
    /// Track the candidate: its profile applies, as on a profiled focus.
    Switch,
}

/// Notifications from the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineEvent {
    /// A fresh status snapshot.
    Status(EngineStatus),
    /// The save hotkey was accepted and a clip is being written. Sent at once, so the overlay can
    /// show before the (slow) write finishes.
    ClipSaving,
    /// A DVR clip was written.
    ClipSaved {
        /// Where.
        path: PathBuf,
        /// How long.
        duration: Hns,
    },
    /// A bookmark was added to the running recording, `at` into it.
    BookmarkAdded {
        /// How far into the recording.
        at: Hns,
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
    /// The tracked game died unexpectedly. The DVR buffer was dumped to `path`; the user
    /// should be asked whether to keep or discard it.
    RecoveryReady {
        /// The rescued clip.
        path: PathBuf,
        /// Executable of the game that died.
        game: String,
        /// How much footage the rescue holds.
        duration: Hns,
    },
    /// A second game took focus while another is tracked. Ask whether to switch. The engine
    /// emits it once per pair per session; it names the tracked game and the candidate by file name.
    FocusPrompt {
        /// The game being clipped.
        current: String,
        /// The app that has focus and kept it for the debounce.
        candidate: String,
    },
    /// A raw key or mouse-button transition, sent only while hotkeys are suspended
    /// ([`Command::SetHotkeysSuspended`]) so a hotkey field can bind the true virtual-key code
    /// (Numpad1 vs 1) instead of what the UI toolkit makes of it. Never sent during gameplay.
    CaptureKey {
        /// The key as Raw Input reports it (may be a left/right modifier variant).
        key: Key,
        /// Pressed (`true`) or released.
        down: bool,
    },
    /// Something the user should know about, but not an error.
    Warning(String),
    /// Something failed.
    Error(String),
    /// A requested feature isn't built yet. Scaffolding only: this disappears as
    /// the features land.
    NotImplemented {
        /// What was requested.
        feature: &'static str,
        /// Phase that delivers it.
        phase: u8,
    },
}

/// Whether the microphone is capturing right now, for the status bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicState {
    /// Not capturing: the mic is off, or it failed to start.
    #[default]
    Off,
    /// Push-to-talk with the key up: the mic runs, but its gate writes silence.
    Muted,
    /// Capturing: always on, or push-to-talk with the key held.
    Live,
}

/// What the engine is doing right now, for the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineStatus {
    /// Frames are being captured and encoded.
    pub capturing: bool,
    /// A manual or session recording is being written.
    pub recording: bool,
    /// Whether the microphone is capturing right now.
    pub mic: MicState,
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
    /// Executable file names that had focus recently and have no profile, newest first,
    /// at most 8.
    pub recent_games: Vec<String>,
    /// The game being recorded: unlike `game`, it doesn't change when you alt-tab to the
    /// taskbar or another app. Clips are tagged with this one.
    pub tracked_game: Option<String>,
}
