//! The ports: traits the platform adapters implement, and the engine drives.
//!
//! ```text
//!             zc-engine  ──drives──►  VideoSource / AudioSource / InputSource
//!                 ▲                               ▲
//!                 │ packets (PacketSink)          │ implemented by
//!                 └───────────────────────────── zc-video, zc-audio, zc-input (Windows)
//!                                                 SyntheticVideoSource (tests, any OS)
//! ```
//!
//! Adapters depend only on `zc-core`, never on the engine, and the engine never
//! depends on Windows. The app (the composition root) plugs real adapters in;
//! tests plug in synthetic ones. This is what keeps the whole recording engine
//! testable without a GPU.

use std::path::Path;
use std::sync::Arc;

use crate::hotkey::Key;
use crate::media::{Codec, TrackId};
use crate::sink::SharedSink;
use crate::time::Hns;
use crate::watchdog::HealthEvent;

/// A callback adapters use to report events without depending on the engine's
/// channel types. Must be cheap and non-blocking.
pub type Emit<T> = Arc<dyn Fn(T) + Send + Sync>;

/// What to capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The primary monitor.
    #[default]
    PrimaryMonitor,
    /// A monitor by index, in the OS's enumeration order.
    Monitor(u32),
    /// A window, by its `HWND` value.
    Window(isize),
}

/// Parameters for starting the GPU video pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoStart {
    /// What to capture.
    pub target: CaptureTarget,
    /// Codec to try first; the adapter walks down the ladder if it can't.
    pub codec: Codec,
    /// Target frame rate.
    pub fps: u32,
    /// VBR floor, bits per second.
    pub min_bitrate_bps: u32,
    /// VBR ceiling, bits per second. Also what the DVR buffer is sized against.
    pub max_bitrate_bps: u32,
    /// Fixed IDR interval (DESIGN §3: 1–2 s, never encoder defaults).
    pub keyframe_interval: Hns,
    /// Draw the mouse cursor into the capture (WGC `IsCursorCaptureEnabled`).
    pub capture_cursor: bool,
}

/// Parameters for starting an audio source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioStart {
    /// Sample rate, Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// AAC bitrate, bits per second.
    pub bitrate_bps: u32,
}

/// Why a source couldn't start or keep running.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The OS or hardware can't do this (e.g. no hardware encoder, Windows too old).
    #[error("not supported on this system: {0}")]
    Unsupported(String),
    /// Protected content (HDCP/DRM) is blocking capture; retry with backoff.
    #[error("protected content is blocking capture")]
    Protected,
    /// A platform API call failed.
    #[error("{context} failed with {code:#010x}: {message}")]
    Platform {
        /// What was being attempted.
        context: String,
        /// The raw error code (an `HRESULT` on Windows).
        code: i32,
        /// The system's description of the error.
        message: String,
    },
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// The GPU video pipeline: capture → hardware encode → packets (DESIGN §3).
pub trait VideoSource: Send {
    /// Starts capturing and encoding. Packets go to `sink`, health signals to
    /// `health`. Returns the codec actually in use after the capability ladder.
    ///
    /// # Errors
    /// See [`SourceError`].
    fn start(
        &mut self,
        config: &VideoStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<Codec, SourceError>;

    /// Stops and releases GPU resources. Idempotent.
    fn stop(&mut self);

    /// Asks the encoder for an IDR frame as soon as possible.
    fn request_keyframe(&mut self);

    /// Writes the current frame to `path` as PNG (DESIGN §5, "Screenshots").
    ///
    /// # Errors
    /// See [`SourceError`].
    fn screenshot(&mut self, path: &Path) -> Result<(), SourceError>;
}

/// An audio track: capture → AAC → packets (DESIGN §6).
pub trait AudioSource: Send {
    /// Which track this source produces.
    fn track(&self) -> TrackId;

    /// Starts capturing and encoding into `sink`.
    ///
    /// # Errors
    /// See [`SourceError`].
    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<(), SourceError>;

    /// Stops capture. Idempotent.
    fn stop(&mut self);

    /// Opens or closes the push-to-talk gate. A closed gate writes silence, so
    /// the track keeps its length and PTS alignment holds. Default: always open.
    fn set_gate(&mut self, open: bool) {
        let _ = open;
    }

    /// Follows a new game: loopback capture of that process tree (include mode,
    /// DESIGN §6). `None` means no game has focus. Default: ignored.
    fn set_target_process(&mut self, pid: Option<u32>) {
        let _ = pid;
    }
}

/// Events from the OS input thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsEvent {
    /// A key or mouse button changed state.
    Key {
        /// Which key.
        key: Key,
        /// `true` for down.
        down: bool,
    },
    /// Another window got focus.
    Foreground {
        /// Owning process id.
        pid: u32,
        /// Full path of its executable.
        exe: String,
    },
}

/// The OS input thread: Raw Input hotkeys plus foreground tracking (DESIGN §7, §8).
pub trait InputSource: Send {
    /// Starts the input thread, reporting through `events`.
    ///
    /// # Errors
    /// See [`SourceError`].
    fn start(&mut self, events: Emit<OsEvent>) -> Result<(), SourceError>;

    /// Stops the input thread. Idempotent.
    fn stop(&mut self);
}
