use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;
use time::macros::format_description;

use zc_core::control::{Command, EngineEvent, EngineStatus, FocusChoice, MicState};
use zc_core::dvr::{DvrClip, DvrConfig, DvrError};
use zc_core::focus_prompt::{self, DEBOUNCE, FocusAction, Focused, is_shell, pair_key};
use zc_core::hotkey::{Binding, ChordMatcher, HotkeyEvent, Trigger};
use zc_core::media::{PacketRef, TrackId, TrackInfo, TrackKind};
use zc_core::ports::{
    AudioSource, AudioStart, CaptureTarget, Emit, InputSource, LockMemory, OsEvent, SeekPenalty,
    SourceError, TotalMemory, VideoSource, VideoStart,
};
use zc_core::profile::{
    GameProfile, ProfileSet, WindowClamp, clamp_window, effective_ram_ceiling, exe_name,
};
use zc_core::settings::{CaptureMode, DvrStorage, MicMode, Settings};
use zc_core::sink::{SharedSink, SinkId};
use zc_core::watchdog::{HealthEvent, Watchdog, WatchdogAction, WatchdogConfig, WatchdogState};
use zc_core::{Codec, Hns};
use zc_mux::pacing::{DriveKind, WritePacer};

use crate::disk_dvr::{self, DiskRing, Persister, STAGING_BYTES};
use crate::output::{self, OutputConfig};
use crate::recorder::{Copied, Recorder, RecorderStats};
use crate::spool;

/// The platform adapters the engine drives. Every one is optional: the app runs,
/// and the UI works, before the phase that delivers a given adapter lands.
#[derive(Default)]
pub struct Ports {
    /// GPU capture + encode (`zc-video`, Phase 1).
    pub video: Option<Box<dyn VideoSource>>,
    /// Game audio loopback (`zc-audio`, Phase 2).
    pub game_audio: Option<Box<dyn AudioSource>>,
    /// Microphone (`zc-audio`, Phase 2).
    pub mic: Option<Box<dyn AudioSource>>,
    /// Per-app audio tracks (`zc-audio`, D26), one per enabled `AppTrack` in settings order, each
    /// with the label its container track gets. They start after the mic and before Master.
    pub app_audio: Vec<(String, Box<dyn AudioSource>)>,
    /// The Master mix, track 1 (`zc-audio`, D30). Starts after the sources above, and only if
    /// one of them started.
    pub master_audio: Option<Box<dyn AudioSource>>,
    /// Raw Input hotkeys + foreground tracking (`zc-input`, Phase 2).
    pub input: Option<Box<dyn InputSource>>,
    /// Drive-type probe for the output folder (`zc-win`, P3.23). Without it the drive is
    /// unknown, and DVR dumps are not paced.
    pub seek_penalty: Option<SeekPenalty>,
    /// Pins the DVR rings in RAM (`zc-win`, P3.25). Without it the rings can be paged out.
    pub lock_memory: Option<LockMemory>,
    /// Reports the machine's physical RAM (`zc-win`, P4.20). Without it the settings' RAM ceiling
    /// stands unchanged (D32).
    pub total_memory: Option<TotalMemory>,
}

impl fmt::Debug for Ports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ports")
            .field("video", &self.video.is_some())
            .field("game_audio", &self.game_audio.is_some())
            .field("mic", &self.mic.is_some())
            .field("app_audio", &self.app_audio.len())
            .field("master_audio", &self.master_audio.is_some())
            .field("input", &self.input.is_some())
            .field("seek_penalty", &self.seek_penalty.is_some())
            .field("lock_memory", &self.lock_memory.is_some())
            .field("total_memory", &self.total_memory.is_some())
            .finish()
    }
}

/// Everything the engine needs to start.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// User settings.
    pub settings: Settings,
    /// Where files go when the settings don't name a folder (the app passes
    /// `<Videos>\ZeroClip`).
    pub default_output_dir: PathBuf,
}

/// Engine failures.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The engine thread has exited; commands can't be delivered.
    #[error("the engine has stopped")]
    Stopped,
    /// The OS refused to start the engine thread.
    #[error("couldn't start the engine thread: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Starts engines. See the [crate docs](crate).
#[derive(Debug)]
pub struct Engine;

impl Engine {
    /// Starts the engine on its own thread.
    ///
    /// `wake` is called after each event is queued, so a UI that sleeps until
    /// something happens (the UI does) knows to repaint.
    ///
    /// # Errors
    /// [`EngineError::Spawn`] if the thread can't be created.
    pub fn spawn(
        config: EngineConfig,
        ports: Ports,
        wake: Emit<()>,
    ) -> Result<EngineHandle, EngineError> {
        let (inbox, rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        // Read once: the machine's RAM doesn't change while the engine runs.
        let total_mib = ports.total_memory.and_then(|total| total());
        let (dvr, _) = dvr_config(&config.settings, total_mib);
        let storage = config.settings.dvr.storage;
        let actor = Actor {
            total_mib,
            storage,
            persister: None,
            output_dir: config
                .settings
                .recording
                .output_dir
                .clone()
                .unwrap_or(config.default_output_dir),
            hotkeys: ChordMatcher::new(bindings(&config.settings)),
            settings: config.settings,
            ports,
            inbox: inbox.clone(),
            events: events_tx,
            wake,
            status: EngineStatus::default(),
            recorder: Arc::new(Recorder::new(dvr)),
            stats_seen: RecorderStats::default(),
            audio_tracks: Vec::new(),
            recording: None,
            saving: None,
            drive: DriveKind::default(),
            mic_running: false,
            ptt_held: false,
            video_track: None,
            game_capture: None,
            tracked: None,
            audio_running: false,
            watchdog: None,
            video_config: None,
            flushing: false,
            pace_dumps: false,
            rescue_waiting: None,
            focus_pending: None,
            focus_prompt: None,
            prompted: HashSet::new(),
        };
        let thread =
            thread::Builder::new().name("zc-engine".into()).spawn(move || actor.run(&rx))?;
        Ok(EngineHandle { inbox, events, thread: Some(thread) })
    }
}

/// Messages to the engine thread.
enum Msg {
    Command(Command),
    Os(OsEvent),
    Health(HealthEvent),
    /// The recording's IO thread has finished: the saved file and its duration, or why it failed.
    RecordingEnded(io::Result<(PathBuf, Hns)>),
    /// The clip's IO thread has finished: which dump it wrote, and the saved file and its
    /// duration, or why there isn't one.
    ClipWritten(Dump, Result<(PathBuf, Hns), ClipError>),
}

/// A handle to a running engine. Dropping it shuts the engine down.
#[derive(Debug)]
pub struct EngineHandle {
    inbox: Sender<Msg>,
    events: Receiver<EngineEvent>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(c) => write!(f, "Command({c:?})"),
            // Never print which key: the input thread sees every keystroke on the
            // system, passwords included, and `run` traces every message.
            Self::Os(OsEvent::Key { .. }) => write!(f, "Os(Key)"),
            Self::Os(e) => write!(f, "Os({e:?})"),
            Self::Health(e) => write!(f, "Health({e:?})"),
            Self::RecordingEnded(result) => write!(f, "RecordingEnded({result:?})"),
            Self::ClipWritten(dump, result) => write!(f, "ClipWritten({dump:?}, {result:?})"),
        }
    }
}

impl EngineHandle {
    /// Sends a command.
    ///
    /// # Errors
    /// [`EngineError::Stopped`] if the engine thread has exited.
    pub fn send(&self, command: Command) -> Result<(), EngineError> {
        self.inbox.send(Msg::Command(command)).map_err(|_| EngineError::Stopped)
    }

    /// Events queued since the last call. Never blocks.
    pub fn poll_events(&self) -> impl Iterator<Item = EngineEvent> + '_ {
        self.events.try_iter()
    }

    /// Waits up to `timeout` for the next event. For tests and headless use.
    #[must_use]
    pub fn wait_event(&self, timeout: Duration) -> Option<EngineEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Stops every source, ends the engine thread and waits for it.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        if let Some(thread) = self.thread.take() {
            // If the send fails the thread is already gone; joining is still right.
            let _ = self.inbox.send(Msg::Command(Command::Shutdown));
            if thread.join().is_err() {
                tracing::error!("engine thread panicked");
            }
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// How often the engine looks at the recorder: the status bar's buffered span and the
/// DVR's error counter. A fixed schedule, so a stream of key and health messages during
/// gameplay can't postpone it.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// How long a save waits before it copies the buffer, when the Master mix runs. The mix trails
/// real time by 200 ms (D30). Waiting this long lets it reach the press, where the clip ends,
/// and the extra 100 ms covers a late wake-up of the mixer thread.
const MASTER_CATCH_UP: Duration = Duration::from_millis(300);

/// State owned by the engine thread.
struct Actor {
    settings: Settings,
    /// The machine's physical RAM in MiB, if the port could read it (D32).
    total_mib: Option<u64>,
    /// Where the DVR buffer really lives this run. Starts as the setting; `start_ring` drops it to
    /// RAM if the ring file can't be made, and a change in settings applies at the next start.
    storage: DvrStorage,
    /// The thread that writes finished GOPs to the ring file, in disk mode (P4.21).
    persister: Option<Persister>,
    output_dir: PathBuf,
    ports: Ports,
    /// Our own inbox, for the emitters handed to sources.
    inbox: Sender<Msg>,
    events: Sender<EngineEvent>,
    wake: Emit<()>,
    status: EngineStatus,
    /// Where every source's packets go: the DVR buffer, then any disk writers.
    recorder: Arc<Recorder>,
    /// The recorder's counters at the last tick, so a rise is reported once.
    stats_seen: RecorderStats,
    /// Audio tracks that started: Master first when it runs, then the sources. The containers
    /// take them (P3.19).
    audio_tracks: Vec<TrackInfo>,
    hotkeys: ChordMatcher<HotkeyAction>,
    /// The one recording in progress, if any (D20).
    recording: Option<ActiveRecording>,
    /// The IO thread of the clip being snapshotted and written. One at a time, so a dump's copy
    /// of the window never exists twice at once (D29). Joined at shutdown, so a clip the user
    /// asked for is never cut short by quitting.
    saving: Option<thread::JoinHandle<()>>,
    /// What kind of drive the output folder is on, which sets the write pacing for dumps.
    drive: DriveKind,
    /// Whether the mic source started, so it is running. Set by `start_audio`, cleared by
    /// `stop_sources`.
    mic_running: bool,
    /// Whether the push-to-talk key is down, as the last `Command::PushToTalk` said.
    ptt_held: bool,
    /// The video track of the encoder that last started. Kept while capture is paused, so a clip
    /// can still be cut from the buffer (D31).
    video_track: Option<TrackInfo>,
    /// The game whose buffer is held: its window and exe file name. Set when capture starts for a
    /// profiled game, and kept while paused. Only used in Game mode (D31).
    game_capture: Option<GameCapture>,
    /// The game whose profile is in effect (P4.6). Sticky: other apps don't change it, so the
    /// buffer keeps the game's window while the user is elsewhere.
    tracked: Option<Tracked>,
    /// Whether the audio sources are running. Set by `start_audio`, cleared by `stop_capture`.
    audio_running: bool,
    /// Decides the encoder recovery sequence (DESIGN §8). Created when the video source starts
    /// and dropped when capture stops, so it is `Some` exactly while video is meant to run, also
    /// mid-recovery when the encoder itself is down.
    watchdog: Option<Watchdog>,
    /// What the video source was last started with, so a watchdog re-init rebuilds the same
    /// capture, only with the codec the watchdog picks.
    video_config: Option<VideoStart>,
    /// The clip being written is the watchdog's flush, so its end moves recovery on (`Flushed`).
    flushing: bool,
    /// The watchdog saw a slow disk: dumps are paced at the HDD rate whatever the drive probe
    /// said (DESIGN §8: pace the writes, don't re-init).
    pace_dumps: bool,
    /// A crash rescue that found the dump slot busy (D29). It runs when that dump ends, before a
    /// flush waiting there too, because the flush ends in RESUMING, which clears the buffer.
    rescue_waiting: Option<WaitingRescue>,
    /// An app that is a candidate for a prompt and has focus, waiting out the debounce (D35). Its
    /// due time says when the prompt is emitted.
    focus_pending: Option<(Candidate, Instant)>,
    /// The candidate the user was asked about. Switch applies to it; it is dropped once focus
    /// moves to another app (D35).
    focus_prompt: Option<Candidate>,
    /// (tracked, candidate) pairs already prompted this session, by lowercase file name (D35).
    prompted: HashSet<(String, String)>,
}

/// An app that could take over the tracked game (D35): what a switch needs.
#[derive(Clone, Debug)]
struct Candidate {
    pid: u32,
    hwnd: isize,
    /// The executable's file name.
    exe: String,
}

/// A tracked game that died while another dump was being written: what its rescue needs once the
/// slot is free (D34).
struct WaitingRescue {
    pid: u32,
    game: String,
    exit_code: u32,
}

/// What a clip IO thread writes, which decides what its end reports.
#[derive(Debug)]
enum Dump {
    /// A clip: the user's save, or the watchdog's flush (`Actor::flushing` tells them apart).
    Clip,
    /// The tracked game died: its window, rescued to a `recovery-*.tmp` file (P4.11, D34).
    Rescue { game: String, exit_code: u32 },
}

/// The game window capture follows, and its exe file name.
struct GameCapture {
    hwnd: isize,
    exe: String,
}

/// The game with a profile in effect: its process, and its exe file name.
struct Tracked {
    pid: u32,
    exe: String,
    /// The tee slot of the session recording this game's profile started (P4.7). Kept so the
    /// game's exit stops that recording, and not a manual one the user started after stopping it.
    session: Option<SinkId>,
    /// The profile wants a session but the recording slot was still finishing the previous
    /// session's file; [`Engine::on_recording_ended`] starts it once the slot is free.
    session_pending: bool,
}

/// How many recently focused games without a profile the status keeps.
const RECENT_GAMES: usize = 8;

/// The window that took focus, as a foreground change reports it.
#[derive(Clone, Copy, Debug)]
struct Focus<'a> {
    pid: u32,
    hwnd: isize,
    exe: &'a str,
}

/// What a foreground change does to capture in Game mode (D31).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureDecision {
    /// Capture this window. `clear_buffer` when the buffer holds a different game.
    Start { hwnd: isize, clear_buffer: bool },
    /// Stop capture, and keep the buffer.
    Pause,
    /// Leave capture as it is.
    Keep,
}

/// Decides what a focus change does to capture (D31). `tracked` is the game with a profile in
/// effect, if any, and `kept` is the game whose buffer is held (window, exe file name), and
/// `capturing` says whether its window is being captured now.
///
/// In Screen mode nothing changes. In Game mode, a profiled game starts capture (or moves it to
/// its new window), and any other app pauses it. While a game is tracked, another profiled game
/// doesn't move capture: it is a candidate for a prompt (D35), so it pauses like any other app.
/// ZeroClip's own windows and the shell are ignored, so opening ZeroClip or the Start menu
/// mid-game doesn't pause capture.
fn capture_decision(
    mode: CaptureMode,
    profiles: &ProfileSet,
    tracked: Option<&str>,
    kept: Option<(isize, &str)>,
    capturing: bool,
    own_pid: u32,
    focus: Focus<'_>,
) -> CaptureDecision {
    if mode == CaptureMode::Screen || focus.pid == own_pid || is_shell(focus.exe) {
        return CaptureDecision::Keep;
    }
    if profiles.find(focus.exe).is_none() {
        return CaptureDecision::Pause;
    }
    if tracked.is_some_and(|game| !focus_prompt::same_game(game, focus.exe)) {
        return CaptureDecision::Pause;
    }
    let same_game =
        kept.is_some_and(|(_, exe)| exe_name(exe).eq_ignore_ascii_case(exe_name(focus.exe)));
    let same_window = kept.is_some_and(|(hwnd, _)| hwnd == focus.hwnd);
    if capturing && same_window && same_game {
        return CaptureDecision::Keep;
    }
    CaptureDecision::Start { hwnd: focus.hwnd, clear_buffer: kept.is_some() && !same_game }
}

/// A manual recording, from its start until its IO thread has finished the file.
struct ActiveRecording {
    /// The recorder's tee slot. `None` once the recording has been stopped: the spool is
    /// closed and the IO thread is finishing the file.
    id: Option<SinkId>,
    /// The file the recording ends up as.
    path: PathBuf,
    /// The IO thread that drains the spool into the file.
    thread: JoinHandle<()>,
}

/// Why a DVR save produced no clip, as its IO thread reports it.
#[derive(Debug, thiserror::Error)]
enum ClipError {
    /// The DVR buffer holds no video yet.
    #[error("nothing is buffered yet")]
    NothingBuffered,
    /// The snapshot or the file write failed. The text says why.
    #[error("{0}")]
    Failed(String),
}

/// What a hotkey does. Engine-local: only the matcher's bindings use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeyAction {
    SaveClip,
    ToggleRecording,
    Screenshot,
    PushToTalk,
}

/// The hotkey bindings for these settings. Push-to-talk is bound only in
/// push-to-talk mode.
fn bindings(settings: &Settings) -> Vec<Binding<HotkeyAction>> {
    let h = &settings.hotkeys;
    let press = |chord: &zc_core::hotkey::Chord, action| Binding {
        chord: chord.clone(),
        action,
        trigger: Trigger::Press,
    };
    let mut bindings = vec![
        press(&h.save_clip, HotkeyAction::SaveClip),
        press(&h.toggle_recording, HotkeyAction::ToggleRecording),
        press(&h.screenshot, HotkeyAction::Screenshot),
    ];
    if settings.audio.mic == MicMode::PushToTalk
        && let Some(chord) = &h.push_to_talk
    {
        bindings.push(Binding {
            chord: chord.clone(),
            action: HotkeyAction::PushToTalk,
            trigger: Trigger::Hold,
        });
    }
    bindings
}

/// Sizes the DVR for these settings, and says how the RAM ceiling clamped the window the
/// profile asks for (DESIGN §7). With the DVR off, both rings are zero bytes, which
/// `DvrBuffer` treats as off. In disk mode the buffer is only the staging area, and the window
/// isn't limited by RAM (D32, P4.21).
fn dvr_config(settings: &Settings, total_mib: Option<u64>) -> (DvrConfig, WindowClamp) {
    let disk = settings.dvr.storage == DvrStorage::Disk;
    let ceiling = if disk {
        u32::MAX
    } else {
        effective_ram_ceiling(settings.dvr.ram_ceiling_mib, total_mib).mib
    };
    let clamp = clamp_for(settings, &settings.profiles.default, ceiling);
    let config = match (settings.dvr.enabled, disk) {
        (false, _) => DvrConfig { video_bytes: 0, audio_bytes: 0 },
        (true, true) => staging_config(settings),
        (true, false) => dvr_for(settings, clamp.window),
    };
    (config, clamp)
}

/// The RAM disk mode keeps: four GOPs at the worst-case bitrate, and at least `STAGING_BYTES`.
fn staging_config(settings: &Settings) -> DvrConfig {
    let window = Hns(settings.video.keyframe_interval().ticks().saturating_mul(4));
    let config = dvr_for(settings, window);
    DvrConfig {
        video_bytes: config.video_bytes.max(STAGING_BYTES),
        audio_bytes: config.audio_bytes.max(STAGING_BYTES / 16),
    }
}

/// The DVR rings for a `window` at the bitrates in these settings.
fn dvr_for(settings: &Settings, window: Hns) -> DvrConfig {
    let max_video_bps = u64::from(settings.video.max_bitrate_kbps) * 1_000;
    let audio_bps = u64::from(settings.audio.bitrate_kbps) * 1_000;
    DvrConfig::for_window(window, max_video_bps, audio_bps)
}

/// The DVR window a profile asks for, limited by the effective RAM ceiling (`ram_ceiling_mib`, the
/// output of [`effective_ram_ceiling`]) at the video bitrate.
fn clamp_for(settings: &Settings, profile: &GameProfile, ram_ceiling_mib: u32) -> WindowClamp {
    let max_video_bps = u64::from(settings.video.max_bitrate_kbps) * 1_000;
    let ram_ceiling = u64::from(ram_ceiling_mib) << 20;
    clamp_window(profile.buffer(), max_video_bps, ram_ceiling)
}

/// The container track for an audio source that has started. `codec_private` stays
/// empty: the muxer builds the AAC config from the rate and channel count (P3.19).
fn audio_track(track: TrackId, label: &str, config: &AudioStart) -> TrackInfo {
    TrackInfo {
        track,
        label: label.into(),
        kind: TrackKind::Audio { sample_rate: config.sample_rate, channels: config.channels },
        codec_private: Vec::new(),
    }
}

/// What Screen mode records: the chosen monitor, or the primary one.
fn screen_target(settings: &Settings) -> CaptureTarget {
    settings.video.monitor.map_or(CaptureTarget::PrimaryMonitor, CaptureTarget::Monitor)
}

/// What the video source is asked to do: `target` comes from the capture mode (see
/// `Actor::capture_target`), the rest from the user's settings.
fn video_start(settings: &Settings, target: CaptureTarget) -> VideoStart {
    let v = &settings.video;
    VideoStart {
        target,
        // "Auto" asks for the best codec; the adapter walks down the ladder.
        codec: v.codec.unwrap_or(Codec::LADDER[0]),
        fps: v.fps,
        min_bitrate_bps: v.min_bitrate_kbps.saturating_mul(1_000),
        max_bitrate_bps: v.max_bitrate_kbps.saturating_mul(1_000),
        keyframe_interval: v.keyframe_interval(),
        capture_cursor: v.capture_cursor,
    }
}

/// `2026-10-05 14-03-22.123`: sortable, no colons (Windows forbids them in file
/// names), and milliseconds so two screenshots in one second don't collide.
fn file_stamp(t: OffsetDateTime) -> String {
    let format =
        format_description!("[year]-[month]-[day] [hour]-[minute]-[second].[subsecond digits:3]");
    t.format(format).unwrap_or_else(|_| t.unix_timestamp().to_string())
}

/// How many times a save starts over when the ring file is overwritten under it (P4.21).
const RING_RETRIES: u32 = 2;

/// Copies the DVR window that ends at `end` and writes it to a new clip file. Runs on the clip's
/// IO thread, so the copy's short lock holds, the ring reads and the paced disk write all stay
/// off the actor (D29).
fn save_clip_file(
    recorder: &Recorder,
    window: Hns,
    end: Hns,
    cfg: &OutputConfig,
    tracks: &[TrackInfo],
    label: &str,
) -> Result<(PathBuf, Hns), ClipError> {
    let copied = recorder
        .plan_clip(window, Some(end))
        .and_then(|plan| recorder.copy_plan(plan))
        .map_err(|e| match e {
            DvrError::Empty => ClipError::NothingBuffered,
            other => ClipError::Failed(other.to_string()),
        })?;
    let create = || output::create_file(&cfg.dir, label, cfg.format.extension());
    write_copied(copied, create, tracks, cfg)
}

/// Writes a crash rescue that was already copied out of the DVR to `path`, a new file (P4.11).
/// Runs on the clip's IO thread. `create_new` means an unanswered rescue is never overwritten.
fn save_rescue_file(
    copied: Copied,
    path: PathBuf,
    cfg: &OutputConfig,
    tracks: &[TrackInfo],
) -> Result<(PathBuf, Hns), ClipError> {
    // The folder exists from startup, but the user may have removed it since.
    std::fs::create_dir_all(&cfg.dir).map_err(|e| ClipError::Failed(e.to_string()))?;
    let create = || File::create_new(&path).map(|file| (path.clone(), file));
    write_copied(copied, create, tracks, cfg)
}

/// Writes a copied clip to the new file `create` makes. A clip in RAM is written as it is. A
/// disk-mode clip is streamed from the ring file (see [`write_streamed`]).
fn write_copied(
    copied: Copied,
    mut create: impl FnMut() -> io::Result<(PathBuf, File)>,
    tracks: &[TrackInfo],
    cfg: &OutputConfig,
) -> Result<(PathBuf, Hns), ClipError> {
    match copied {
        Copied::Ram(clip) => {
            let (path, file) = create().map_err(|e| ClipError::Failed(e.to_string()))?;
            write_dump(&clip, path, file, tracks, cfg)
        }
        Copied::Disk(mut clip) => write_streamed(create, tracks, cfg, |attempt, write| {
            if attempt > 0 {
                clip.replan();
            }
            clip.stream(write)
        }),
    }
}

/// Writes the packets `stream` hands to its writer into a new file. `stream` gets the attempt
/// number, 0 first. If it fails with [`disk_dvr::Lapped`], some of the clip was overwritten after
/// packets were written: the partial file is removed and `stream` runs again into a fresh file,
/// at most [`RING_RETRIES`] more times. It returns the clip's length.
fn write_streamed(
    mut create: impl FnMut() -> io::Result<(PathBuf, File)>,
    tracks: &[TrackInfo],
    cfg: &OutputConfig,
    mut stream: impl FnMut(u32, &mut dyn FnMut(PacketRef<'_>) -> io::Result<()>) -> io::Result<Hns>,
) -> Result<(PathBuf, Hns), ClipError> {
    let failed = |e: io::Error| ClipError::Failed(e.to_string());
    for attempt in 0..=RING_RETRIES {
        let (path, file) = create().map_err(failed)?;
        match output::write_clip_with(file, tracks, cfg, |write| stream(attempt, write)) {
            Ok(duration) => return Ok(finish_dump(path, duration)),
            Err(e) if disk_dvr::is_lapped(&e) => {
                tracing::debug!(attempt, "DVR ring overwritten while saving");
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => return Err(failed(e)),
        }
    }
    Err(ClipError::Failed(
        "The DVR ring file was overwritten while saving, every time it was tried. The disk may          be too slow for the buffer; try again."
            .into(),
    ))
}

/// Writes `clip` into `file` at `path` and syncs it (DurableFile::dump, then `sync_all`), so the
/// file is complete on disk before its event goes out.
fn write_dump(
    clip: &DvrClip,
    path: PathBuf,
    file: File,
    tracks: &[TrackInfo],
    cfg: &OutputConfig,
) -> Result<(PathBuf, Hns), ClipError> {
    output::write_clip(clip, file, tracks, cfg).map_err(|e| ClipError::Failed(e.to_string()))?;
    Ok(finish_dump(path, clip.duration()))
}

/// The clip is written. It still plays without its length in the mehd, so a failure to set it is
/// only logged.
fn finish_dump(path: PathBuf, duration: Hns) -> (PathBuf, Hns) {
    if let Err(error) = zc_mux::repair::set_fragment_duration(&path) {
        tracing::warn!(error = %error, "could not set a clip's length");
    }
    (path, duration)
}

impl Actor {
    fn run(mut self, rx: &Receiver<Msg>) {
        tracing::info!(output_dir = %self.output_dir.display(), "engine started");
        // Before the profile: it sizes the ring file or the RAM buffer, whichever this run uses.
        self.start_ring();
        self.reapply_profile();
        self.detect_drive();
        // Before the sources start: pinning holds the recorder lock while Windows commits the
        // whole buffer, which would stall the encoder and audio threads if they were running.
        self.page_lock_dvr();
        self.start_sources();
        self.publish_status();
        let mut next_tick = Instant::now() + STATUS_INTERVAL;
        loop {
            let now = Instant::now();
            if now >= next_tick {
                self.on_tick();
                next_tick = now + STATUS_INTERVAL;
            }
            if self.focus_pending.as_ref().is_some_and(|(_, due)| now >= *due) {
                self.on_focus_settled();
            }
            // Wake for the next tick, or for a candidate's debounce if that comes first (D35).
            let wake =
                self.focus_pending.as_ref().map_or(next_tick, |(_, due)| next_tick.min(*due));
            let msg = match rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            tracing::trace!(?msg, "engine message");
            match msg {
                Msg::Command(Command::Shutdown) => break,
                Msg::Command(command) => self.on_command(command),
                Msg::Os(event) => self.on_os_event(event),
                Msg::Health(event) => self.on_health(event),
                Msg::RecordingEnded(result) => self.on_recording_ended(result),
                Msg::ClipWritten(dump, result) => self.on_clip_written(dump, result),
            }
        }
        self.stop_sources();
        self.finish_recording();
        self.finish_dumps();
        // After the dumps: a clip being written may still read the ring. The file stays on disk.
        drop(self.persister.take());
        tracing::info!("engine stopped");
    }

    /// At shutdown, waits for the dump being written, then writes a crash rescue that was waiting
    /// for it. Quitting right after Alt+F10 must not cut the clip short, and quitting right after a
    /// game crash must not lose the rescue: its file stays for the next start's scan (P4.12).
    fn finish_dumps(&mut self) {
        loop {
            if let Some(thread) = self.saving.take() {
                tracing::info!("finishing the clip being saved before exiting");
                if thread.join().is_err() {
                    tracing::error!("clip thread panicked");
                }
            }
            // The slot is free now, so the rescue starts at once and the loop joins it.
            let Some(waiting) = self.rescue_waiting.take() else { break };
            self.run_waiting_rescue(waiting);
        }
    }

    fn emit(&self, event: EngineEvent) {
        if self.events.send(event).is_ok() {
            (self.wake)(());
        }
    }

    /// Makes `profile` the one in effect: the DVR window is the profile's, limited by the RAM
    /// ceiling. A window that changed resizes the buffer (D21). `announce` says a game took focus,
    /// so the UI hears about it. A settings change re-applies quietly. The status keeps the active
    /// window, so there is only one copy of it to go stale.
    fn apply_profile(&mut self, profile: &GameProfile, exe: &str, announce: bool) {
        let ram = effective_ram_ceiling(self.settings.dvr.ram_ceiling_mib, self.total_mib);
        let disk = self.storage == DvrStorage::Disk;
        let clamp = clamp_for(&self.settings, profile, if disk { u32::MAX } else { ram.mib });
        if clamp.window != self.status.buffer_window && self.settings.dvr.enabled {
            let config = dvr_for(&self.settings, clamp.window);
            if disk {
                // Sized at start for the largest saved window, so this is a no-op unless a
                // settings change since then asked for more (and then it keeps the footage).
                match self.recorder.grow_ring(config.ring_file_bytes()) {
                    Ok(true) => tracing::info!(window = ?clamp.window, "DVR ring file grown"),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "DVR ring file not grown");
                        self.emit(EngineEvent::Warning(format!(
                            "Couldn't grow the DVR ring file ({error}). The buffer keeps its old size."
                        )));
                    }
                }
            } else {
                self.recorder.resize(config);
            }
        }
        self.status.buffer_window = clamp.window;
        if clamp.clamped {
            let source = if ram.from_system_ram { ", set from your system RAM," } else { "" };
            self.emit(EngineEvent::Warning(format!(
                "The {} MiB DVR memory limit{source} cuts the buffer to {:.0} s.",
                ram.mib,
                clamp.window.as_secs_f64(),
            )));
        }
        if announce {
            self.emit(EngineEvent::ProfileApplied {
                exe: exe.to_owned(),
                window: clamp.window,
                clamped: clamp.clamped,
            });
        }
    }

    /// Applies the tracked game's profile again after the settings changed, or the default when no
    /// game is tracked or its profile was removed.
    fn reapply_profile(&mut self) {
        let profile = self
            .tracked
            .as_ref()
            .and_then(|tracked| self.settings.profiles.find(&tracked.exe))
            .unwrap_or(&self.settings.profiles.default)
            .clone();
        self.apply_profile(&profile, "", false);
    }

    /// Handles a focus change for profiles (P4.6). A profiled game becomes the tracked game when
    /// nothing is tracked: its profile applies, and its process gets the input watch and the game
    /// audio. Any other app keeps the current profile (sticky, D33), so alt-tabbing to Discord or
    /// opening Task Manager can't shrink the buffer before a crash rescue. Such an app is listed
    /// as a recent game instead. A second game, or a fullscreen app without a profile, is a
    /// candidate: after the debounce the user is asked (D35), and nothing moves until they answer.
    ///
    /// Returns whether this focus started tracking a game, so the caller can start its session
    /// recording once capture is running.
    fn on_profile_focus(&mut self, pid: u32, hwnd: isize, exe: &str, fullscreen: bool) -> bool {
        let name = exe_name(exe);
        let ours = pid == std::process::id();
        let is_tracked = self.tracked.as_ref().is_some_and(|t| t.exe.eq_ignore_ascii_case(name));
        let focused =
            Focused { exe, profiled: self.settings.profiles.find(exe).is_some(), fullscreen, ours };
        let action = {
            let tracked = self.tracked.as_ref().map(|t| t.exe.as_str());
            let asked = tracked.is_some_and(|t| self.prompted.contains(&pair_key(t, name)));
            focus_prompt::on_focus(tracked, focused, asked)
        };
        if !focused.profiled && !is_tracked && !is_shell(exe) && !ours && !name.is_empty() {
            self.remember_recent(name);
        }
        match action {
            FocusAction::Ignore => false,
            FocusAction::Track => {
                if self.tracked.as_ref().is_some_and(|tracked| tracked.pid == pid) {
                    return false;
                }
                self.track_game(pid, exe);
                true
            }
            FocusAction::Ask => {
                let candidate = Candidate { pid, hwnd, exe: name.to_owned() };
                self.focus_pending = Some((candidate, Instant::now() + DEBOUNCE));
                false
            }
        }
    }

    /// Makes the app behind `pid` the tracked game (D33): its process gets the input watch and
    /// the game audio, and its profile (the default for an app without one) applies. Used for a
    /// profiled game when nothing is tracked, and for a switch the user picked (D35).
    fn track_game(&mut self, pid: u32, exe: &str) {
        let name = exe_name(exe);
        let profile = self.settings.profiles.resolve(exe).clone();
        // The previous game's session ends with its tracking: only the tracked game's exit is
        // acted on, so nothing else would ever stop it.
        self.stop_session();
        self.tracked =
            Some(Tracked { pid, exe: name.to_owned(), session: None, session_pending: false });
        if let Some(input) = self.ports.input.as_mut()
            && let Err(error) = input.watch_process(pid)
        {
            tracing::warn!(pid, %error, "couldn't watch the game's process");
        }
        if let Some(audio) = self.ports.game_audio.as_mut() {
            audio.set_target_process(Some(pid));
        }
        self.apply_profile(&profile, name, true);
    }

    /// A focus change to `exe` ends a candidate's wait and its prompt, unless `exe` is that
    /// candidate (D35). The tracked game is unchanged either way. The shell never ends them: a
    /// fullscreen player reaches ZeroClip through the taskbar or alt-tab (D35).
    fn settle_candidates(&mut self, exe: &str) {
        if is_shell(exe) {
            return;
        }
        let name = exe_name(exe);
        if self.focus_pending.as_ref().is_some_and(|(c, _)| !focus_prompt::same_game(&c.exe, name))
        {
            self.focus_pending = None;
        }
        if self.focus_prompt.as_ref().is_some_and(|c| !focus_prompt::same_game(&c.exe, name)) {
            self.focus_prompt = None;
        }
    }

    /// The candidate kept focus for the debounce (D35): ask the user, once per pair per session.
    fn on_focus_settled(&mut self) {
        let Some((candidate, _)) = self.focus_pending.take() else { return };
        let Some(current) = self.tracked.as_ref().map(|t| t.exe.clone()) else { return };
        self.prompted.insert(pair_key(&current, &candidate.exe));
        self.emit(EngineEvent::FocusPrompt { current, candidate: candidate.exe.clone() });
        self.focus_prompt = Some(candidate);
    }

    /// The user's answer to the focus prompt (D35). Switch tracks the candidate as a profiled
    /// focus would (D33), and starts its capture. Keep, or a prompt that already went away, does
    /// nothing to the tracked game.
    fn on_focus_choice(&mut self, choice: FocusChoice) {
        let Some(candidate) = self.focus_prompt.take() else { return };
        if choice == FocusChoice::Switch {
            self.track_game(candidate.pid, &candidate.exe);
            self.on_capture_focus(Focus {
                pid: candidate.pid,
                hwnd: candidate.hwnd,
                exe: &candidate.exe,
            });
            self.start_session();
        }
        self.publish_status();
    }

    /// Starts the tracked game's session recording when its profile asks for one (P4.7, formerly
    /// P3.26). It takes the one recording slot (D20), so it gets the same `.partial` + repair
    /// robustness as a manual recording, and the DVR keeps buffering alongside it. A recording
    /// already running is left alone: the user started it, so it isn't ours to replace.
    fn start_session(&mut self) {
        let Some(tracked) = self.tracked.as_ref() else { return };
        let wanted = self.settings.profiles.find(&tracked.exe).is_some_and(|p| p.session_recording);
        if !wanted {
            return;
        }
        if let Some(recording) = self.recording.as_ref() {
            // `id` is None once a stop was asked for: the file is only being finished, so the
            // session can start as soon as the slot frees up.
            if recording.id.is_none() {
                if let Some(tracked) = self.tracked.as_mut() {
                    tracked.session_pending = true;
                }
            } else {
                tracing::debug!("session recording skipped: a recording is already running");
            }
            return;
        }
        self.start_recording("session");
        let id = self.recording.as_ref().and_then(|recording| recording.id);
        if let Some(tracked) = self.tracked.as_mut() {
            tracked.session = id;
        }
    }

    /// Stops the tracked game's session recording, if it is still the one running. The IO thread
    /// finishes the file and reports `RecordingStopped`, as for a manual stop.
    fn stop_session(&mut self) {
        let Some(id) = self.tracked.as_mut().and_then(|tracked| tracked.session.take()) else {
            return;
        };
        if self.recording.as_ref().is_some_and(|recording| recording.id == Some(id)) {
            self.stop_recording();
        }
    }

    /// A watched process ended (P4.7). Only the tracked game's exit matters: an unexpected exit
    /// rescues the buffer first (P4.11), then its session recording stops, game audio lets go of
    /// its process, and the default profile applies again. Any other pid is a game tracked
    /// earlier, whose profile has already been replaced.
    fn on_process_exited(&mut self, pid: u32, exit_code: u32) {
        let Some(game) =
            self.tracked.as_ref().filter(|tracked| tracked.pid == pid).map(|t| t.exe.clone())
        else {
            tracing::debug!(pid, exit_code, "an untracked process exited");
            return;
        };
        tracing::info!(pid, exit_code, "the tracked game exited");
        // 0 is a clean quit. Task Manager's End task (Details tab) and `taskkill` report 1, and a
        // crash reports its NTSTATUS, such as 0xC0000005 for an access violation. So anything but
        // 0 is unexpected (D34).
        if exit_code != 0 {
            // One dump at a time (D29). The game stays tracked while the rescue waits, so its
            // profile keeps the buffer at the game's window; `on_clip_written` comes back for it.
            if self.saving.is_some() {
                tracing::info!(pid, "crash rescue waits for the dump being written");
                self.rescue_waiting = Some(WaitingRescue { pid, game, exit_code });
                return;
            }
            // Before anything below: the default profile can shrink the buffer and drop the
            // footage the rescue exists to save.
            self.rescue(&game, exit_code);
        }
        self.release_game();
    }

    /// Runs a rescue that waited for the dump slot, then lets go of its game, unless another
    /// profiled game took over while it waited. That game's focus already replaced the tracking,
    /// and its buffer is what the rescue gets. A different game's focus skips the Game-mode clear
    /// (D31) while a rescue waits, so the dead game's footage is still there.
    fn run_waiting_rescue(&mut self, waiting: WaitingRescue) {
        self.rescue(&waiting.game, waiting.exit_code);
        if self.tracked.as_ref().is_some_and(|tracked| tracked.pid == waiting.pid) {
            self.release_game();
        }
    }

    /// Crash rescue (P4.11, D34): copies the game's window out of the DVR and starts the clip IO
    /// thread writing it to `recovery-<unix_ms>.<ext>.tmp` in the output folder. The file stays
    /// there so Keep is a same-volume rename, and `.tmp` keeps it out of the Clips list until the
    /// user answers (P4.12). `RecoveryReady` goes out once the file is on disk.
    ///
    /// The copy runs here, not on the IO thread as a save's does (D29): the caller reverts to the
    /// default profile right after, and a resize under a copy in progress would fail it
    /// (`DvrError::Changed`). It is a few short lock holds (D29), once per crash. The caller
    /// checks the dump slot is free, so this is the only copy of the window.
    ///
    /// In disk mode only the staged part is copied here (at most the staging area). The older
    /// footage is streamed from the ring file on the IO thread, like a save's: reading a whole
    /// ring here would block the actor and fill the RAM. The profile switch doesn't touch the
    /// ring (it is sized for the largest window), so the file is still there when it is read.
    fn rescue(&mut self, game: &str, exit_code: u32) {
        if !self.settings.dvr.enabled {
            tracing::info!(game, exit_code, "game exited unexpectedly; the DVR buffer is off");
            return;
        }
        let snapshot = match self.video_track.clone() {
            Some(video) => self
                .recorder
                .plan_clip(self.status.buffer_window, None)
                .and_then(|plan| self.recorder.copy_plan(plan))
                .map(|copied| (video, copied)),
            None => Err(DvrError::Empty),
        };
        let (video, copied) = match snapshot {
            Ok(found) => found,
            Err(DvrError::Empty) => {
                self.emit(EngineEvent::Warning(format!(
                    "{game} closed unexpectedly, but nothing was buffered yet."
                )));
                return;
            }
            Err(error) => {
                tracing::error!(game, %error, "crash rescue: the buffer couldn't be copied");
                self.emit(EngineEvent::Error(format!("Couldn't rescue {game}'s footage: {error}")));
                return;
            }
        };
        let (tracks, cfg) = self.dump_output(video);
        // Milliseconds, so two rescues never share a name. `create_new` still refuses a name in
        // use rather than overwrite an unanswered rescue.
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let path = cfg.dir.join(format!("recovery-{unix_ms}.{}.tmp", cfg.format.extension()));
        let dump = Dump::Rescue { game: game.to_owned(), exit_code };
        self.spawn_dump(dump, move || save_rescue_file(copied, path, &cfg, &tracks));
    }

    /// Lets go of the tracked game after it exited: its session recording stops, game audio
    /// releases its process, and the default profile applies again.
    fn release_game(&mut self) {
        self.stop_session();
        if let Some(audio) = self.ports.game_audio.as_mut() {
            audio.set_target_process(None);
        }
        self.tracked = None;
        // A prompt about the game that just exited has nothing left to switch away from (D35).
        self.focus_pending = None;
        self.focus_prompt = None;
        self.status.game = None;
        let default = self.settings.profiles.default.clone();
        self.apply_profile(&default, "*", false);
        self.publish_status();
    }

    /// Moves `exe` to the front of the recent games, dropping an older entry for it, and keeps
    /// the newest [`RECENT_GAMES`].
    fn remember_recent(&mut self, exe: &str) {
        let recent = &mut self.status.recent_games;
        recent.retain(|game| !game.eq_ignore_ascii_case(exe));
        recent.insert(0, exe.to_owned());
        recent.truncate(RECENT_GAMES);
    }

    /// Asks the drive probe whether the output folder is on a spinning disk, which sets the
    /// pacing for DVR dumps (P3.24, DESIGN §4). Runs on the engine thread: it makes a few quick
    /// device calls, at startup and when the output folder changes. An unknown or failed answer
    /// leaves the writes unpaced.
    fn detect_drive(&mut self) {
        // Create the folder now, as the first recording would. The probe itself doesn't need it.
        let _ = std::fs::create_dir_all(&self.output_dir);
        self.drive = match self.ports.seek_penalty {
            Some(probe) => match probe(&self.output_dir) {
                Ok(answer) => DriveKind::from_seek_penalty(answer),
                Err(e) => {
                    tracing::warn!(error = %e, "drive probe failed");
                    DriveKind::Unknown
                }
            },
            None => DriveKind::Unknown,
        };
        tracing::info!(
            drive = ?self.drive,
            output_dir = %self.output_dir.display(),
            "output drive detected",
        );
        if self.drive == DriveKind::Hdd && self.settings.storage.pace_hdd_writes {
            self.emit(EngineEvent::Warning(format!(
                "Clips go to a hard disk, so they're written at {} MB/s to keep games smooth.",
                self.settings.storage.hdd_write_mb_per_sec,
            )));
        }
    }

    /// Disk mode (P4.21): creates the ring file and starts the thread that fills it. If the file
    /// can't be created or sized (disk full, no permission), says so and keeps the buffer in RAM
    /// for this run: `apply_profile` then sizes the RAM buffer as it always does.
    fn start_ring(&mut self) {
        if self.storage != DvrStorage::Disk || !self.settings.dvr.enabled {
            return;
        }
        let dir = self.settings.dvr.ring_dir.clone().unwrap_or_else(|| self.output_dir.clone());
        // The largest window of any profile, so a profile switch never needs a bigger file (RAM
        // isn't the limit here, and a switch must not wipe the footage a rescue is about to read).
        let profiles = &self.settings.profiles;
        let window = std::iter::once(&profiles.default)
            .chain(&profiles.games)
            .map(GameProfile::buffer)
            .max()
            .unwrap_or(Hns::ZERO);
        let bytes = dvr_for(&self.settings, window).ring_file_bytes();
        let started = Instant::now();
        let started_ring = DiskRing::create(&dir, bytes).and_then(|ring| {
            Persister::start(&self.recorder, &Arc::new(ring), self.ring_pacer(&dir))
        });
        match started_ring {
            Ok(persister) => {
                self.persister = Some(persister);
                tracing::info!(
                    dir = %dir.display(),
                    mib = bytes >> 20,
                    took = ?started.elapsed(),
                    "DVR ring file ready",
                );
            }
            Err(error) => {
                tracing::warn!(%error, dir = %dir.display(), "DVR ring file not created");
                self.storage = DvrStorage::Ram;
                self.emit(EngineEvent::Warning(format!(
                    "Couldn't create the DVR ring file in {} ({error}). The buffer is kept in RAM \
                     for this run.",
                    dir.display(),
                )));
            }
        }
    }

    /// Ring writes are paced like dumps on a spinning disk (P3.24). The ring may sit on another
    /// drive than the clips, so it asks the probe about its own folder.
    fn ring_pacer(&self, dir: &Path) -> WritePacer {
        let storage = &self.settings.storage;
        if !storage.pace_hdd_writes {
            return WritePacer::unlimited();
        }
        let answer = self.ports.seek_penalty.and_then(|probe| probe(dir).ok()).flatten();
        let rate = u64::from(storage.hdd_write_mb_per_sec) * 1_000_000;
        WritePacer::for_drive(DriveKind::from_seek_penalty(answer), rate)
    }

    /// Pins the DVR rings of the tracks that have a source: video, and each audio source this run
    /// was built with (P3.25, DESIGN §4). The other audio slots stay unpinned (pitfall 26). Runs
    /// once, before the sources start; a video restart keeps the same rings, so their pins stay.
    /// A ring that can't be pinned is reported once, and recording goes on unpinned.
    fn page_lock_dvr(&mut self) {
        let Some(lock) = self.ports.lock_memory else { return };
        // Disk mode keeps almost nothing in RAM, so there is nothing worth pinning (P4.21).
        if !self.settings.dvr.enabled || self.storage == DvrStorage::Disk {
            return;
        }
        let video = self.ports.video.is_some().then_some(TrackId::Video);
        let audio = [&self.ports.game_audio, &self.ports.mic, &self.ports.master_audio]
            .into_iter()
            .filter_map(|source| source.as_ref().map(|source| source.track()))
            .chain(self.ports.app_audio.iter().map(|(_, source)| source.track()));
        let tracks: Vec<TrackId> = video.into_iter().chain(audio).collect();
        let started = Instant::now();
        match self.recorder.lock_memory(lock, &tracks) {
            Ok(bytes) => tracing::info!(
                mib = bytes >> 20,
                took = ?started.elapsed(),
                "DVR buffer page-locked",
            ),
            Err(e) => {
                tracing::warn!(error = %e, "DVR buffer not page-locked");
                self.emit(EngineEvent::Warning(format!(
                    "Couldn't page-lock the DVR buffer ({e}). Clipping still works, but Windows \
                     may page it out under memory pressure."
                )));
            }
        }
    }

    fn publish_status(&self) {
        self.emit(EngineEvent::Status(self.status.clone()));
    }

    /// The mic's state for the status bar: `Live` while it captures, `Muted` in push-to-talk with
    /// the key up, and `Off` when the mic is off or didn't start.
    fn mic_state(&self) -> MicState {
        if !self.mic_running {
            return MicState::Off;
        }
        match self.settings.audio.mic {
            MicMode::Off => MicState::Off,
            MicMode::AlwaysOn => MicState::Live,
            MicMode::PushToTalk if self.ptt_held => MicState::Live,
            MicMode::PushToTalk => MicState::Muted,
        }
    }

    /// Publishes the status only if the mic's state moved. A held key auto-repeats its press, and
    /// those repeats must not flood the UI with identical statuses.
    fn refresh_mic_state(&mut self) {
        let state = self.mic_state();
        if state != self.status.mic {
            self.status.mic = state;
            self.publish_status();
        }
    }

    /// Posts health signals from source threads into the engine's inbox.
    fn health_emitter(&self) -> Emit<HealthEvent> {
        let inbox = self.inbox.clone();
        Arc::new(move |event| {
            // Fails only while the engine is shutting down; nothing to do then.
            let _ = inbox.send(Msg::Health(event));
        })
    }

    fn start_sources(&mut self) {
        // Game mode starts nothing here: capture begins when a profiled game takes focus (D31).
        if let Some(target) = self.capture_target() {
            self.start_capture(target);
        }

        let inbox = self.inbox.clone();
        let os_events: Emit<OsEvent> = Arc::new(move |event| {
            // Fails only while the engine is shutting down; nothing to do then.
            let _ = inbox.send(Msg::Os(event));
        });
        if let Some(Err(e)) = self.ports.input.as_mut().map(|input| input.start(os_events)) {
            self.emit(EngineEvent::Error(format!("Hotkeys unavailable: {e}")));
        }
    }

    /// The audio tracks: 48 kHz stereo AAC at the configured bitrate. The sources start first,
    /// then the Master mix, which runs only if one of them did (D30). Master leads
    /// `audio_tracks`, so it is the first audio track: browsers and Discord play only that one.
    fn start_audio(&mut self) {
        let config = AudioStart {
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: self.settings.audio.bitrate_kbps.saturating_mul(1_000),
        };
        let health = self.health_emitter();
        let sink: SharedSink = self.recorder.clone();
        let gate_open = self.settings.audio.mic == MicMode::AlwaysOn;
        let mut errors = Vec::new();
        let mut tracks = Vec::new();
        if let Some(game) = self.ports.game_audio.as_mut() {
            match game.start(&config, Arc::clone(&sink), Arc::clone(&health)) {
                Ok(()) => tracks.push(audio_track(game.track(), "Game", &config)),
                Err(e) => errors.push(format!("Game audio unavailable: {e}")),
            }
        }
        if let Some(mic) = self.ports.mic.as_mut() {
            match mic.start(&config, Arc::clone(&sink), Arc::clone(&health)) {
                Ok(()) => {
                    mic.set_gate(gate_open);
                    tracks.push(audio_track(mic.track(), "Microphone", &config));
                    self.mic_running = true;
                }
                Err(e) => errors.push(format!("Microphone unavailable: {e}")),
            }
        }
        // An app track that fails is reported and skipped; it never stops the recording.
        for (label, app) in &mut self.ports.app_audio {
            match app.start(&config, Arc::clone(&sink), Arc::clone(&health)) {
                Ok(()) => tracks.push(audio_track(app.track(), label, &config)),
                Err(e) => errors.push(format!("{label} audio unavailable: {e}")),
            }
        }
        // Only when a source runs, so Master is never a track of silence alone. It starts after
        // them, so its mix point is set before any source writes (D30).
        if !tracks.is_empty()
            && let Some(master) = self.ports.master_audio.as_mut()
        {
            match master.start(&config, sink, health) {
                Ok(()) => tracks.insert(0, audio_track(master.track(), "Master", &config)),
                Err(e) => errors.push(format!("Master mix unavailable: {e}")),
            }
        }
        self.audio_tracks = tracks;
        self.audio_running = true;
        // `run` publishes the first status once the sources have started.
        self.status.mic = self.mic_state();
        for error in errors {
            self.emit(EngineEvent::Error(error));
        }
    }

    /// What the video source should record now, or `None` when nothing should be: in Game mode
    /// that is only the game window while it is captured.
    fn capture_target(&self) -> Option<CaptureTarget> {
        match self.settings.video.capture_mode {
            CaptureMode::Screen => Some(screen_target(&self.settings)),
            CaptureMode::Game if self.status.capturing => {
                self.game_capture.as_ref().map(|game| CaptureTarget::Window(game.hwnd))
            }
            CaptureMode::Game => None,
        }
    }

    /// Starts the video source on `target`, and the audio sources unless they already run.
    fn start_capture(&mut self, target: CaptureTarget) {
        self.start_video(target);
        if !self.audio_running {
            self.start_audio();
        }
    }

    fn start_video(&mut self, target: CaptureTarget) {
        let health = self.health_emitter();
        let config = video_start(&self.settings, target);
        let sink: SharedSink = self.recorder.clone();
        match self.ports.video.as_mut().map(|video| video.start(&config, sink, health)) {
            Some(Ok(codec)) => {
                tracing::info!(codec = codec.name(), ?target, "capture started");
                // A fresh start is a healthy pipeline, so any recovery in progress is over.
                self.watchdog = Some(Watchdog::new(WatchdogConfig::default(), codec));
                self.video_config = Some(config);
                self.status.watchdog = WatchdogState::Running;
                self.status.capturing = true;
                self.status.codec = Some(codec);
                self.video_track = self.ports.video.as_ref().and_then(|video| video.format());
            }
            Some(Err(e)) => self.emit(EngineEvent::Error(format!("Capture couldn't start: {e}"))),
            None => self.emit(EngineEvent::Warning(
                "No capture source is attached, so nothing is captured or buffered.".into(),
            )),
        }
    }

    fn stop_sources(&mut self) {
        if let Some(input) = self.ports.input.as_mut() {
            input.stop();
        }
        self.stop_capture();
    }

    /// Restarts capture after a video setting or the capture mode changed. The DVR buffer
    /// restarts from empty, since GOPs from the old encoder can't share a clip with the new one.
    fn restart_capture(&mut self) {
        tracing::info!("video settings changed; restarting capture");
        let target = self.capture_target();
        let was_capturing = self.status.capturing;
        if let Some(video) = self.ports.video.as_mut() {
            video.stop();
        }
        if self.recording.as_ref().is_some_and(|r| r.id.is_some()) {
            self.stop_recording();
            self.emit(EngineEvent::Warning(
                "Video settings changed, so the recording was stopped.".into(),
            ));
        }
        // `clear` keeps the rings' memory, so the pins taken at startup still hold.
        self.recorder.clear();
        self.status.capturing = false;
        match target {
            // Audio keeps running when only the video restarts.
            Some(target) if was_capturing => self.start_video(target),
            Some(target) => self.start_capture(target),
            None => self.stop_capture(),
        }
    }

    /// Starts, pauses or keeps capture for the window that took focus (D31).
    fn on_capture_focus(&mut self, focus: Focus<'_>) {
        let kept = self.game_capture.as_ref().map(|game| (game.hwnd, game.exe.as_str()));
        // `status.capturing` is false mid-recovery, but the capture session is still alive.
        let active = self.status.capturing || self.watchdog.is_some();
        let tracked = self.tracked.as_ref().map(|t| t.exe.as_str());
        let decision = capture_decision(
            self.settings.video.capture_mode,
            &self.settings.profiles,
            tracked,
            kept,
            active,
            std::process::id(),
            focus,
        );
        match decision {
            CaptureDecision::Keep => {}
            CaptureDecision::Pause => {
                // Not `status.capturing`: that is false mid-recovery, and a recovery left running
                // would restart the encoder while the user is in another app.
                if self.watchdog.is_some() {
                    tracing::info!("focus left the game; capture paused");
                    self.stop_capture();
                }
            }
            CaptureDecision::Start { hwnd, clear_buffer } => {
                if active {
                    self.stop_capture();
                }
                // A rescue waiting for the dump slot still needs the dead game's footage, so it
                // isn't cleared. Trade-off: the new game's first clips can hold the old game too.
                if clear_buffer && self.rescue_waiting.is_none() {
                    self.recorder.clear();
                }
                let exe = exe_name(focus.exe).to_owned();
                tracing::info!(exe, hwnd, clear_buffer, "capturing the game window");
                self.game_capture = Some(GameCapture { hwnd, exe });
                self.start_capture(CaptureTarget::Window(hwnd));
            }
        }
    }

    /// Stops the video and audio sources and keeps the DVR buffer. The input thread keeps running,
    /// so the hotkeys work and the next game focus can start capture again (D31).
    fn stop_capture(&mut self) {
        let Ports { video, game_audio, mic, app_audio, master_audio, .. } = &mut self.ports;
        // Master last: it still mixes what the sources wrote before they stopped.
        let sources = [game_audio, mic]
            .into_iter()
            .flatten()
            .chain(app_audio.iter_mut().map(|(_, source)| source))
            .chain(master_audio.as_mut());
        for audio in sources {
            audio.stop();
        }
        if let Some(video) = video {
            video.stop();
        }
        // A recovery cut short here must not restart the encoder later (a pending flush's
        // `Flushed` then finds no watchdog and is dropped).
        self.watchdog = None;
        // A flush still writing ends through `on_clip_written`; its `Flushed` must not reach a
        // watchdog created later.
        self.flushing = false;
        self.status.watchdog = WatchdogState::Running;
        tracing::info!(stats = ?self.recorder.stats(), "recorder totals");
        self.status.capturing = false;
        self.audio_running = false;
        self.mic_running = false;
        self.status.mic = self.mic_state();
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::SaveClip => self.save_clip(),
            Command::FocusChoice(choice) => self.on_focus_choice(choice),
            Command::ToggleRecording => self.toggle_recording(),
            Command::Screenshot => self.take_screenshot(),
            Command::PushToTalk(down) => {
                self.ptt_held = down;
                if let Some(mic) = self.ports.mic.as_mut() {
                    mic.set_gate(down);
                }
                self.refresh_mic_state();
            }
            Command::ApplySettings(settings) => {
                // Phase 4: also re-size the DVR buffer. Until then it keeps its startup size.
                let video_changed = self.settings.video != settings.video;
                self.settings = *settings;
                self.reapply_profile();
                self.hotkeys = ChordMatcher::new(bindings(&self.settings));
                // Switching modes must never leave the push-to-talk gate open.
                if let Some(mic) = self.ports.mic.as_mut() {
                    mic.set_gate(self.settings.audio.mic == MicMode::AlwaysOn);
                }
                // The gate was just set for the new mode, and the matcher was rebuilt above, so
                // it has forgotten any push-to-talk key that was down.
                self.ptt_held = false;
                // In Game mode with nothing captured, the next game focus starts capture with the
                // new settings, so there is nothing to restart now.
                let restart = self.settings.video.capture_mode == CaptureMode::Screen
                    || self.status.capturing;
                if video_changed && self.ports.video.is_some() && restart {
                    self.restart_capture();
                }
                if let Some(dir) = &self.settings.recording.output_dir
                    && *dir != self.output_dir
                {
                    self.output_dir.clone_from(dir);
                    self.detect_drive();
                }
                tracing::info!("settings applied");
                self.status.mic = self.mic_state();
                self.publish_status();
            }
            Command::Shutdown => unreachable!("handled by the run loop"),
        }
    }

    /// Runs on the engine thread, off the hot path, so a ~100 ms PNG encode is fine.
    fn take_screenshot(&mut self) {
        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        let path = self.output_dir.join(format!("ZeroClip {}.png", file_stamp(now)));
        let result = match self.ports.video.as_mut() {
            Some(video) if self.status.capturing => std::fs::create_dir_all(&self.output_dir)
                .map_err(|e| SourceError::Other(e.to_string()))
                .and_then(|()| video.screenshot(&path)),
            _ => Err(SourceError::Other("capture isn't running".into())),
        };
        match result {
            Ok(()) => self.emit(EngineEvent::ScreenshotSaved { path }),
            Err(e) => self.emit(EngineEvent::Error(format!("Screenshot failed: {e}"))),
        }
    }

    /// Starts a manual recording, or stops it. One recording at a time (D20).
    fn toggle_recording(&mut self) {
        match &self.recording {
            None => self.start_recording("recording"),
            Some(ActiveRecording { id: Some(_), .. }) => self.stop_recording(),
            // The last recording is still being finished, so its file isn't free yet.
            Some(ActiveRecording { id: None, .. }) => {
                self.emit(EngineEvent::Warning("Still finishing the last recording.".into()));
            }
        }
    }

    /// Starts a recording into a new file in the output folder. Packets reach it through the
    /// spool, which an IO thread drains into the file (P3.21). The DVR keeps buffering.
    fn start_recording(&mut self, label: &str) {
        let Some(video) = self.ports.video.as_ref().and_then(|video| video.format()) else {
            self.emit(EngineEvent::Warning("Nothing to record yet.".into()));
            return;
        };
        let tracks: Vec<TrackInfo> =
            std::iter::once(video).chain(self.audio_tracks.iter().cloned()).collect();
        // Recordings stream at the encode bitrate. Pacing is for DVR dumps, which come in bursts.
        let cfg = OutputConfig {
            pacer: WritePacer::unlimited(),
            ..OutputConfig::from_settings(&self.settings, self.output_dir.clone(), self.drive)
        };
        let (partial, container) = match output::create_recording(&cfg, &tracks, label) {
            Ok(created) => created,
            Err(e) => {
                self.emit(EngineEvent::Error(format!("Couldn't start the recording: {e}")));
                return;
            }
        };

        let (tee_writer, reader) = spool::spool();
        let path = spool::final_path(&partial);
        let inbox = self.inbox.clone();
        let io_partial = partial.clone();
        let spawned = thread::Builder::new().name("zc-io-recording".into()).spawn(move || {
            // A panic must still end the recording, or the actor would wait for it forever.
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                spool::drain(reader, container, &io_partial)
            }))
            .unwrap_or_else(|_| Err(io::Error::other("the recording thread panicked")));
            // Fails only while the engine is shutting down, when nobody needs the result.
            let _ = inbox.send(Msg::RecordingEnded(result));
        });
        let io_thread = match spawned {
            Ok(io_thread) => io_thread,
            Err(e) => {
                output::discard_recording(&partial);
                self.emit(EngineEvent::Error(format!("Couldn't start the recording: {e}")));
                return;
            }
        };
        // Attached only once the IO thread exists, so a failed spawn never leaves a writer
        // feeding a spool that nothing reads.
        let id = self.recorder.attach(Box::new(tee_writer));
        self.recording =
            Some(ActiveRecording { id: Some(id), path: path.clone(), thread: io_thread });
        self.status.recording = true;
        self.publish_status();
        tracing::info!(path = %path.display(), "recording started");
        self.emit(EngineEvent::RecordingStarted { path });
    }

    /// Stops the recording. Closing the spool lets the IO thread drain what is left, finish the
    /// file, and report back with `Msg::RecordingEnded`. The actor doesn't wait for that.
    fn stop_recording(&mut self) {
        let Some(recording) = self.recording.as_mut() else { return };
        let Some(id) = recording.id.take() else { return };
        drop(self.recorder.detach(id));
        tracing::info!(path = %recording.path.display(), "recording stopped");
    }

    /// The IO thread has finished the file, or has given up on it. The thread sends this just
    /// before it exits, so the join returns at once.
    fn on_recording_ended(&mut self, result: io::Result<(PathBuf, Hns)>) {
        let Some(recording) = self.recording.take() else {
            tracing::warn!("a recording ended that the engine wasn't tracking");
            return;
        };
        if recording.thread.join().is_err() {
            tracing::error!("recording thread panicked");
        }
        // Still attached only if the IO thread gave up on a disk error. Detach it, so the
        // recorder stops feeding a spool that nothing reads.
        if let Some(id) = recording.id {
            drop(self.recorder.detach(id));
        }
        self.status.recording = false;
        self.publish_status();
        if self.tracked.as_mut().is_some_and(|t| std::mem::take(&mut t.session_pending)) {
            self.start_session();
        }
        match result {
            Ok((path, duration)) => {
                tracing::info!(path = %path.display(), "recording saved");
                self.emit(EngineEvent::RecordingStopped { path, duration });
            }
            Err(e) => {
                tracing::error!(error = %e, "recording failed");
                self.emit(EngineEvent::Error(format!("Recording failed: {e}")));
            }
        }
    }

    /// At shutdown, stops the recording and waits for its IO thread, so the file is finished
    /// before the engine exits. The engine is exiting anyway, so this is the one wait on a drain.
    fn finish_recording(&mut self) {
        let Some(mut recording) = self.recording.take() else { return };
        if let Some(id) = recording.id.take() {
            drop(self.recorder.detach(id));
        }
        if recording.thread.join().is_err() {
            tracing::error!("recording thread panicked");
        }
        tracing::info!(path = %recording.path.display(), "recording closed at shutdown");
    }

    /// Saves the DVR's last `buffer_window` to a new clip file. The actor only checks the state
    /// and starts the IO thread. The snapshot and the disk write happen there, and the result
    /// comes back as `Msg::ClipWritten` (D19, D29).
    fn save_clip(&mut self) {
        if !self.settings.dvr.enabled {
            self.emit(EngineEvent::Warning("The DVR buffer is off (Settings, DVR buffer).".into()));
            return;
        }
        // A press during a save is refused, not queued: two copies of the window at once would
        // double the RAM a dump takes.
        if self.saving.is_some() {
            self.emit(EngineEvent::Warning("Still saving the previous clip.".into()));
            return;
        }
        // The buffer outlives a pause (D31), so the track comes from the last encoder, not the
        // running one.
        let Some(video) = self.video_track.clone() else {
            self.emit(EngineEvent::Warning("Nothing to save yet: capture hasn't started.".into()));
            return;
        };
        self.spawn_clip("clip", self.status.buffer_window, video);
    }

    /// Starts the clip IO thread, which snapshots the last `window` of the DVR and writes it to a
    /// new `label` file, then reports `Msg::ClipWritten`. Returns whether the thread started; if
    /// not, the error has been reported.
    fn spawn_clip(&mut self, label: &'static str, window: Hns, video: TrackInfo) -> bool {
        let (tracks, cfg) = self.dump_output(video);
        // The clip ends at the press. The Master mix trails real time (D30), so when it runs the
        // copy waits until the mix has reached that point.
        let end = self.recorder.video_end();
        let catch_up = if self.audio_tracks.iter().any(|t| t.track == TrackId::Master) {
            MASTER_CATCH_UP
        } else {
            Duration::ZERO
        };
        let recorder = Arc::clone(&self.recorder);
        self.spawn_dump(Dump::Clip, move || {
            if !catch_up.is_zero() {
                thread::sleep(catch_up);
            }
            save_clip_file(&recorder, window, end, &cfg, &tracks, label)
        })
    }

    /// The tracks and output settings for a dump whose video track is `video`. Dumps come in
    /// bursts, so they are paced on a spinning disk, and always once the watchdog saw a slow one.
    fn dump_output(&self, video: TrackInfo) -> (Vec<TrackInfo>, OutputConfig) {
        let tracks: Vec<TrackInfo> =
            std::iter::once(video).chain(self.audio_tracks.iter().cloned()).collect();
        let mut cfg =
            OutputConfig::from_settings(&self.settings, self.output_dir.clone(), self.drive);
        if self.pace_dumps {
            let hdd_rate = u64::from(self.settings.storage.hdd_write_mb_per_sec) * 1_000_000;
            cfg.pacer = WritePacer::limited(hdd_rate);
        }
        (tracks, cfg)
    }

    /// Starts the clip IO thread on `job`, which writes one dump, and reports what it returns as
    /// `Msg::ClipWritten` with `dump`. Takes the one dump slot (`saving`, D29), so callers check
    /// it is free. Returns whether the thread started; if not, the error has been reported.
    fn spawn_dump(
        &mut self,
        dump: Dump,
        job: impl FnOnce() -> Result<(PathBuf, Hns), ClipError> + Send + 'static,
    ) -> bool {
        let inbox = self.inbox.clone();
        let spawned = thread::Builder::new().name("zc-io-clip".into()).spawn(move || {
            // A panic must still report back, or `saving` stays set and refuses every later save.
            let result = panic::catch_unwind(AssertUnwindSafe(job))
                .unwrap_or_else(|_| Err(ClipError::Failed("the clip thread panicked".into())));
            // Fails only while the engine is shutting down, when nobody needs the result.
            let _ = inbox.send(Msg::ClipWritten(dump, result));
        });
        match spawned {
            // The result comes back through the inbox; the handle is joined then, or at shutdown.
            Ok(thread) => {
                self.saving = Some(thread);
                true
            }
            Err(e) => {
                self.emit(EngineEvent::Error(format!("Couldn't save the clip: {e}")));
                false
            }
        }
    }

    /// The watchdog's FLUSHING step (DESIGN §8): writes everything the DVR holds to its own
    /// `recovered` file, because the buffer restarts empty when capture resumes. It is a normal
    /// dump on the clip IO thread, so `Flushed` comes from [`on_clip_written`](Self::on_clip_written)
    /// and the actor never waits on the disk. Returns `Flushed` at once when there is nothing to
    /// write, and `None` while the write (or a save it waits behind) runs.
    fn start_flush(&mut self) -> Option<HealthEvent> {
        // One dump at a time (D29): a save already running finishes first, and its end starts this.
        if self.saving.is_some() {
            return None;
        }
        match self.video_track.clone() {
            Some(video) if self.settings.dvr.enabled => {
                // The whole buffer, not the profile's window: the ring may hold more, and all of
                // it is about to be cleared.
                self.flushing = self.spawn_clip("recovered", Hns(i64::MAX), video);
                (!self.flushing).then_some(HealthEvent::Flushed)
            }
            _ => Some(HealthEvent::Flushed),
        }
    }

    /// The clip's IO thread has finished, or given up. Clears `saving`, so the next press is taken,
    /// and starts a rescue or a flush that was waiting for it.
    fn on_clip_written(&mut self, dump: Dump, result: Result<(PathBuf, Hns), ClipError>) {
        // The thread sends this just before it exits, so the join returns at once.
        if let Some(thread) = self.saving.take()
            && thread.join().is_err()
        {
            tracing::error!("clip thread panicked");
        }
        let flushed = std::mem::take(&mut self.flushing);
        if let Dump::Rescue { game, exit_code } = dump {
            self.on_rescue_written(game, exit_code, result);
        } else {
            self.on_dump_written(flushed, result);
        }
        // The rescue first: a flush moving on below ends in RESUMING, which clears the buffer.
        // The flush then waits behind the rescue's write, and starts at its end.
        if let Some(waiting) = self.rescue_waiting.take() {
            self.run_waiting_rescue(waiting);
        }
        if flushed {
            // Even a failed flush moves on: staying stuck in FLUSHING would end capture for good.
            self.on_health(HealthEvent::Flushed);
        } else if self.watchdog.as_ref().is_some_and(|w| w.state() == WatchdogState::Flushing)
            && let Some(event) = self.start_flush()
        {
            // The flush was waiting behind this save. `start_flush` found nothing to write.
            self.on_health(event);
        }
    }

    /// Reports a written rescue: the prompt's `RecoveryReady`, or why there is no file.
    fn on_rescue_written(
        &mut self,
        game: String,
        exit_code: u32,
        result: Result<(PathBuf, Hns), ClipError>,
    ) {
        match result {
            Ok((path, duration)) => {
                tracing::warn!(
                    game,
                    exit_code,
                    path = %path.display(),
                    "game exited unexpectedly; buffer rescued"
                );
                self.emit(EngineEvent::RecoveryReady { path, game, duration });
            }
            Err(error) => {
                tracing::error!(game, exit_code, %error, "crash rescue failed");
                self.emit(EngineEvent::Error(format!("Couldn't rescue {game}'s footage: {error}")));
            }
        }
    }

    /// Reports a written clip or flush.
    fn on_dump_written(&mut self, flushed: bool, result: Result<(PathBuf, Hns), ClipError>) {
        match result {
            Ok((path, duration)) => {
                tracing::info!(path = %path.display(), "clip saved");
                if flushed {
                    self.emit(EngineEvent::Warning(format!(
                        "The encoder failed and is restarting; the footage so far was saved to {}.",
                        path.display()
                    )));
                }
                self.emit(EngineEvent::ClipSaved { path, duration });
            }
            // An empty buffer has nothing to rescue, so the flush says nothing about it.
            Err(ClipError::NothingBuffered) if flushed => {}
            Err(ClipError::NothingBuffered) => {
                self.emit(EngineEvent::Warning("Nothing to save yet.".into()));
            }
            Err(ClipError::Failed(reason)) => {
                tracing::error!(error = %reason, "clip save failed");
                self.emit(EngineEvent::Error(format!("Couldn't save the clip: {reason}")));
            }
        }
    }

    fn on_os_event(&mut self, event: OsEvent) {
        match event {
            OsEvent::Key { key, down } => {
                let Some(event) = self.hotkeys.on_key(key, down) else {
                    return;
                };
                tracing::debug!(?event, "hotkey");
                let command = match event {
                    HotkeyEvent::Pressed(HotkeyAction::SaveClip) => Command::SaveClip,
                    HotkeyEvent::Pressed(HotkeyAction::ToggleRecording) => Command::ToggleRecording,
                    HotkeyEvent::Pressed(HotkeyAction::Screenshot) => Command::Screenshot,
                    HotkeyEvent::Pressed(HotkeyAction::PushToTalk) => Command::PushToTalk(true),
                    HotkeyEvent::Released(HotkeyAction::PushToTalk) => Command::PushToTalk(false),
                    HotkeyEvent::Released(_) => return,
                };
                self.on_command(command);
            }
            OsEvent::Foreground { pid, hwnd, exe, fullscreen } => {
                tracing::debug!(pid, exe, "foreground changed");
                self.settle_candidates(&exe);
                let newly_tracked = self.on_profile_focus(pid, hwnd, &exe, fullscreen);
                self.on_capture_focus(Focus { pid, hwnd, exe: &exe });
                // After capture focus: in Game mode the encoder starts there, and a recording
                // needs its video track.
                if newly_tracked {
                    self.start_session();
                }
                // The status holds the file name, not the full path (P4.6).
                self.status.game = Some(exe_name(&exe).to_owned());
                self.publish_status();
            }
            OsEvent::ProcessExited { pid, exit_code } => self.on_process_exited(pid, exit_code),
        }
    }

    /// Feeds a health signal to the watchdog and carries out what it decides (DESIGN §8). Most
    /// steps finish at once and answer with the next event, which goes straight back in. The
    /// flush and the first IDR come back later through the inbox. Without a watchdog (video not
    /// started, or capture paused) the signal is dropped.
    fn on_health(&mut self, mut event: HealthEvent) {
        tracing::trace!(?event, "health");
        loop {
            let Some(watchdog) = self.watchdog.as_mut() else { return };
            let before = watchdog.state();
            let action = watchdog.on_event(event);
            let after = watchdog.state();
            if after != before {
                tracing::info!(?before, ?after, ?action, "watchdog");
                self.status.watchdog = after;
                // RESUMING has a running encoder again; only the first IDR is still to come.
                self.status.capturing =
                    matches!(after, WatchdogState::Running | WatchdogState::Resuming);
                self.publish_status();
            }
            match self.perform(action) {
                Some(next) => event = next,
                None => return,
            }
        }
    }

    /// Carries out one watchdog step. Returns the health event that answers it when the step is
    /// done already, or `None` when nothing follows or the answer comes later.
    fn perform(&mut self, action: WatchdogAction) -> Option<HealthEvent> {
        match action {
            WatchdogAction::None => None,
            WatchdogAction::Drain => {
                // `VideoSource::stop` drains the encoder's in-flight samples into the sink and
                // releases the device, so once it returns the buffer holds everything encoded.
                if let Some(video) = self.ports.video.as_mut() {
                    video.stop();
                }
                Some(HealthEvent::Drained)
            }
            WatchdogAction::FlushToDisk => self.start_flush(),
            WatchdogAction::Reinit { codec } => Some(self.reinit(codec)),
            WatchdogAction::Resume => {
                // The old GOPs are in the flushed file. `clear` keeps the master clock anchored,
                // so PTS carries on from before the fault rather than from zero (DESIGN §8). The
                // DVR skips frames until the IDR asked for here, which the source reports.
                self.recorder.clear();
                if let Some(video) = self.ports.video.as_mut() {
                    video.request_keyframe();
                }
                None
            }
            WatchdogAction::PaceDiskWrites => {
                if !self.pace_dumps {
                    self.pace_dumps = true;
                    self.emit(EngineEvent::Warning(
                        "The disk is slow; clips will be written more gently.".into(),
                    ));
                }
                None
            }
            WatchdogAction::GiveUp => {
                tracing::error!("watchdog gave up: no codec could be re-initialised");
                self.status.capturing = false;
                self.emit(EngineEvent::Error(
                    "Capture stopped: no hardware encoder could be restarted. Update the GPU \
                     driver or restart ZeroClip."
                        .into(),
                ));
                None
            }
        }
    }

    /// The watchdog's REINIT step: starts the video source again as it was last started, with
    /// `codec`, which is lower on the ladder after repeated failures (DESIGN §3, §8).
    fn reinit(&mut self, codec: Codec) -> HealthEvent {
        let Some(config) = self.video_config else { return HealthEvent::ReinitFailed };
        let config = VideoStart { codec, ..config };
        let sink: SharedSink = self.recorder.clone();
        let health = self.health_emitter();
        let Some(video) = self.ports.video.as_mut() else { return HealthEvent::ReinitFailed };
        match video.start(&config, sink, health) {
            Ok(actual) => {
                tracing::info!(codec = actual.name(), "encoder re-initialised");
                let format = video.format();
                self.status.codec = Some(actual);
                // The tee would keep writing the new encoder's packets under the old track
                // header, as `restart_capture` avoids.
                if self.recording.as_ref().is_some_and(|r| r.id.is_some()) {
                    self.stop_recording();
                    self.emit(EngineEvent::Warning(
                        "The encoder restarted, so the recording was stopped.".into(),
                    ));
                }
                // A step down the ladder changes the track; the buffer is cleared at RESUMING,
                // so later clips hold only the new format.
                self.video_track = format;
                HealthEvent::ReinitSucceeded
            }
            Err(error) => {
                tracing::warn!(codec = codec.name(), %error, "encoder re-init failed");
                HealthEvent::ReinitFailed
            }
        }
    }

    /// Runs once a second while no message arrives. Refreshes the buffered span on the
    /// status bar when it moved, and warns when the DVR refused video since the last tick.
    fn on_tick(&mut self) {
        // The ring is sized for the maximum bitrate, so at a lower one it holds more than the
        // window. A save only takes the window, so that's the most worth showing.
        let buffered = self.recorder.buffered().min(self.status.buffer_window);
        if buffered != self.status.buffered {
            self.status.buffered = buffered;
            self.publish_status();
        }
        let stats = self.recorder.stats();
        if stats.dvr_errors > self.stats_seen.dvr_errors {
            self.emit(EngineEvent::Warning(
                "A keyframe interval's worth of video didn't fit in the DVR buffer; \
                 raise the memory limit or lower the bitrate."
                    .into(),
            ));
        }
        if stats.ring_errors > self.stats_seen.ring_errors {
            self.emit(EngineEvent::Warning(
                "The DVR ring file couldn't be written; the buffer is not keeping older footage."
                    .into(),
            ));
        }
        // Only a recording still attached can overflow its spool. After a stop, or after its
        // IO thread gave up, a rise in the count is from a spool nobody reads, so it isn't news.
        let attached = self.recording.as_ref().is_some_and(|recording| recording.id.is_some());
        if stats.writer_errors > self.stats_seen.writer_errors && attached {
            self.emit(EngineEvent::Warning(
                "The disk couldn't keep up; the recording skipped ahead to the next keyframe."
                    .into(),
            ));
        }
        self.stats_seen = stats;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use zc_core::hotkey::Key;

    use crate::synthetic::{SyntheticInput, SyntheticVideoSource};

    use super::*;

    /// A video source that never touches a GPU and can be told to fail. It records the target of
    /// every start.
    struct FakeVideo {
        fail: bool,
        targets: Arc<Mutex<Vec<CaptureTarget>>>,
    }

    impl VideoSource for FakeVideo {
        fn start(
            &mut self,
            config: &VideoStart,
            _: SharedSink,
            _: Emit<HealthEvent>,
        ) -> Result<Codec, SourceError> {
            self.targets.lock().unwrap().push(config.target);
            if self.fail {
                Err(SourceError::Unsupported("no GPU in this test".into()))
            } else {
                Ok(Codec::Hevc)
            }
        }
        fn format(&self) -> Option<TrackInfo> {
            (!self.fail).then(|| TrackInfo {
                track: TrackId::Video,
                label: "Video".into(),
                kind: TrackKind::Video { codec: Codec::Hevc, width: 1920, height: 1080, fps: 60 },
                codec_private: Vec::new(),
            })
        }
        fn stop(&mut self) {}
        fn request_keyframe(&mut self) {}
        fn screenshot(&mut self, path: &Path) -> Result<(), SourceError> {
            std::fs::write(path, b"png").map_err(|e| SourceError::Other(e.to_string()))
        }
    }

    fn with_video(fail: bool) -> Ports {
        Ports {
            video: Some(Box::new(FakeVideo { fail, targets: Arc::default() })),
            ..Ports::default()
        }
    }

    /// The default settings, in Screen mode, for tests about the whole screen.
    fn screen_settings() -> Settings {
        let mut settings = Settings::default();
        settings.video.capture_mode = CaptureMode::Screen;
        settings
    }

    #[test]
    fn changed_video_settings_restart_capture_once() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let video = FakeVideo { fail: false, targets: Arc::clone(&targets) };
        let (engine, _) =
            spawn_in(screen_settings(), Ports { video: Some(Box::new(video)), ..Ports::default() });
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));

        let mut settings = screen_settings();
        settings.profiles.default.buffer_secs = 90; // not a video setting
        engine.send(Command::ApplySettings(Box::new(settings.clone()))).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        assert_eq!(targets.lock().unwrap().len(), 1, "unchanged video settings keep capturing");

        settings.video.fps = 30;
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));
        assert_eq!(targets.lock().unwrap().len(), 2);
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn spawn(ports: Ports) -> (EngineHandle, Arc<AtomicUsize>) {
        spawn_in(Settings::default(), ports)
    }

    fn spawn_in(settings: Settings, ports: Ports) -> (EngineHandle, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let config = EngineConfig {
            settings,
            default_output_dir: std::env::temp_dir().join("zeroclip-test"),
        };
        let engine = Engine::spawn(
            config,
            ports,
            Arc::new(move |()| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .expect("spawn");
        (engine, wakes)
    }

    /// Waits for the first event matching `pred`.
    fn wait_for(engine: &EngineHandle, pred: impl Fn(&EngineEvent) -> bool) -> EngineEvent {
        loop {
            let event = engine.wait_event(TIMEOUT).expect("timed out waiting for an event");
            if pred(&event) {
                return event;
            }
        }
    }

    #[test]
    fn starts_reports_status_and_shuts_down() {
        let (engine, wakes) = spawn(Ports::default());
        let status = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = status else { unreachable!() };
        assert!(!status.capturing);
        assert!(wakes.load(Ordering::Relaxed) > 0, "UI must be woken for every event");
        engine.shutdown();
    }

    #[test]
    fn applying_settings_publishes_a_new_status() {
        let (engine, _) = spawn(Ports::default());
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let mut settings = Settings::default();
        settings.profiles.default.buffer_secs = 90;
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = event else { unreachable!() };
        assert_eq!(status.buffer_window, zc_core::Hns::from_secs(90));
    }

    #[test]
    fn reports_the_codec_once_capture_starts() {
        let (engine, _) = spawn_in(screen_settings(), with_video(false));
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.capturing && s.codec == Some(Codec::Hevc)),
        );
    }

    #[test]
    fn capture_failure_is_reported_not_fatal() {
        let (engine, _) = spawn_in(screen_settings(), with_video(true));
        wait_for(&engine, |e| matches!(e, EngineEvent::Error(m) if m.starts_with("Capture")));
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = event else { unreachable!() };
        assert!(!status.capturing);
    }

    #[test]
    fn video_start_follows_settings() {
        let start = video_start(&Settings::default(), CaptureTarget::PrimaryMonitor);
        assert_eq!(start.codec, Codec::Av1, "Auto asks for the best codec");
        assert_eq!(start.min_bitrate_bps, 5_000_000);
        assert_eq!(start.max_bitrate_bps, 20_000_000);
        assert_eq!(start.keyframe_interval, zc_core::Hns::from_secs(1));
        assert!(!start.capture_cursor);
        assert_eq!(start.target, CaptureTarget::PrimaryMonitor);
    }

    #[test]
    fn screen_mode_targets_the_chosen_monitor() {
        let mut settings = Settings::default();
        settings.video.monitor = Some(1);
        assert_eq!(screen_target(&settings), CaptureTarget::Monitor(1));
        assert_eq!(
            video_start(&settings, screen_target(&settings)).target,
            CaptureTarget::Monitor(1)
        );
    }

    fn focus(pid: u32, hwnd: isize, exe: &str) -> Focus<'_> {
        Focus { pid, hwnd, exe }
    }

    const CS2: &str = r"C:\Games\cs2.exe";
    const DISCORD: &str = r"C:\Apps\Discord.exe";

    #[test]
    fn screen_mode_never_changes_capture() {
        let decision = capture_decision(
            CaptureMode::Screen,
            &ProfileSet::default(),
            None,
            None,
            true,
            1,
            focus(9, 5, DISCORD),
        );
        assert_eq!(decision, CaptureDecision::Keep);
    }

    #[test]
    fn game_mode_starts_on_a_profiled_game() {
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            Some("cs2.exe"),
            None,
            false,
            1,
            focus(42, 100, CS2),
        );
        assert_eq!(decision, CaptureDecision::Start { hwnd: 100, clear_buffer: false });
    }

    #[test]
    fn game_mode_pauses_on_any_other_app() {
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            Some("cs2.exe"),
            Some((100, "cs2.exe")),
            true,
            1,
            focus(8, 5, DISCORD),
        );
        assert_eq!(decision, CaptureDecision::Pause);
    }

    #[test]
    fn same_game_after_a_pause_keeps_the_buffer() {
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            Some("cs2.exe"),
            Some((100, "cs2.exe")),
            false,
            1,
            focus(42, 100, CS2),
        );
        assert_eq!(decision, CaptureDecision::Start { hwnd: 100, clear_buffer: false });
    }

    #[test]
    fn a_candidate_game_does_not_move_capture_while_one_is_tracked() {
        // D35: the prompt decides, so capture stays off the second game and keeps its buffer.
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            Some("cs2.exe"),
            Some((100, "cs2.exe")),
            true,
            1,
            focus(77, 300, r"D:\Games\RainbowSix.exe"),
        );
        assert_eq!(decision, CaptureDecision::Pause);
    }

    #[test]
    fn a_different_game_clears_the_buffer_when_none_is_tracked() {
        // After the tracked game exited, the next game is tracked at once and takes the buffer.
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            None,
            Some((100, "cs2.exe")),
            false,
            1,
            focus(77, 300, r"D:\Games\RainbowSix.exe"),
        );
        assert_eq!(decision, CaptureDecision::Start { hwnd: 300, clear_buffer: true });
    }

    #[test]
    fn another_window_of_the_same_game_moves_capture_and_keeps_the_buffer() {
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            Some("cs2.exe"),
            Some((100, "cs2.exe")),
            true,
            1,
            focus(42, 200, CS2),
        );
        assert_eq!(decision, CaptureDecision::Start { hwnd: 200, clear_buffer: false });
    }

    #[test]
    fn the_game_being_captured_is_left_alone() {
        let decision = capture_decision(
            CaptureMode::Game,
            &ProfileSet::default(),
            Some("cs2.exe"),
            Some((100, "cs2.exe")),
            true,
            1,
            focus(42, 100, CS2),
        );
        assert_eq!(decision, CaptureDecision::Keep);
    }

    #[test]
    fn zeroclip_and_the_shell_never_pause_capture() {
        let (profiles, kept) = (ProfileSet::default(), Some((100, "cs2.exe")));
        let own = capture_decision(
            CaptureMode::Game,
            &profiles,
            Some("cs2.exe"),
            kept,
            true,
            1,
            focus(1, 5, DISCORD),
        );
        assert_eq!(own, CaptureDecision::Keep, "our own window is ignored");
        let shell = capture_decision(
            CaptureMode::Game,
            &profiles,
            Some("cs2.exe"),
            kept,
            true,
            1,
            focus(3, 6, r"C:\Windows\explorer.exe"),
        );
        assert_eq!(shell, CaptureDecision::Keep, "the taskbar is ignored");
    }

    #[test]
    fn a_game_without_a_profile_is_not_captured() {
        let mut settings = Settings::default();
        settings.profiles.games.clear();
        let decision = capture_decision(
            CaptureMode::Game,
            &settings.profiles,
            None,
            None,
            false,
            1,
            focus(42, 100, CS2),
        );
        assert_eq!(decision, CaptureDecision::Pause, "without a profile, the game is not captured");
    }
    #[test]
    fn file_stamp_is_sortable_and_has_no_colons() {
        let t = time::macros::datetime!(2023-11-14 22:13:20.5 UTC);
        assert_eq!(file_stamp(t), "2023-11-14 22-13-20.500");
    }

    #[test]
    fn screenshot_is_saved_in_the_output_folder() {
        let (engine, _) = spawn_in(screen_settings(), with_video(false));
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));
        engine.send(Command::Screenshot).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::ScreenshotSaved { .. }));
        let EngineEvent::ScreenshotSaved { path } = event else { unreachable!() };
        assert_eq!(path.parent(), Some(std::env::temp_dir().join("zeroclip-test").as_path()));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert!(path.is_file());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn saving_with_nothing_buffered_writes_no_file() {
        let dir = std::env::temp_dir().join(format!("zeroclip-clip-empty-{}", std::process::id()));
        let cfg = OutputConfig::from_settings(&Settings::default(), dir.clone(), DriveKind::Ssd);
        let recorder = Recorder::new(DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 });
        let result =
            save_clip_file(&recorder, Hns::from_secs(60), Hns::from_secs(60), &cfg, &[], "clip");
        assert!(matches!(result, Err(ClipError::NothingBuffered)));
        assert!(!dir.exists(), "the file is only created once there is a clip to write");
    }

    #[test]
    fn a_save_starts_over_when_the_ring_is_lapped_and_gives_up_after_the_retries() {
        let dir = std::env::temp_dir().join(format!("zeroclip-clip-lapped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = OutputConfig::from_settings(&Settings::default(), dir.clone(), DriveKind::Ssd);
        let video = TrackInfo {
            track: TrackId::Video,
            label: "Video".into(),
            kind: TrackKind::Video { codec: Codec::H264, width: 1920, height: 1080, fps: 60 },
            codec_private: Vec::new(),
        };
        let packets = zc_core::testing::SyntheticStream::new().take_secs(2);
        let create = || output::create_file(&dir, "clip", "mp4");
        let lapped = || io::Error::other(disk_dvr::Lapped);
        let files = || std::fs::read_dir(&dir).map_or(0, Iterator::count);

        // Lapped twice, after packets were written: each partial file is removed, the third
        // attempt is kept.
        let mut attempts = Vec::new();
        let (path, length) =
            write_streamed(create, std::slice::from_ref(&video), &cfg, |attempt, write| {
                attempts.push(attempt);
                for packet in packets.iter().filter(|p| p.meta.track == TrackId::Video) {
                    write(packet.view())?;
                }
                if attempt < 2 { Err(lapped()) } else { Ok(Hns::from_secs(2)) }
            })
            .unwrap();
        assert_eq!(attempts, [0, 1, 2]);
        assert_eq!(length, Hns::from_secs(2));
        assert_eq!(files(), 1, "only the last file is left");
        assert!(path.is_file());
        std::fs::remove_file(path).unwrap();

        // Lapped every time: a clear error, and no partial file.
        let mut attempts = 0;
        let result = write_streamed(create, std::slice::from_ref(&video), &cfg, |_, write| {
            attempts += 1;
            write(packets[0].view())?;
            Err(lapped())
        });
        assert!(matches!(result, Err(ClipError::Failed(m)) if m.contains("overwritten")));
        assert_eq!(attempts, RING_RETRIES + 1);
        assert_eq!(files(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn screenshot_without_capture_is_an_error() {
        let (engine, _) = spawn(Ports::default());
        engine.send(Command::Screenshot).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Error(m) if m.starts_with("Screenshot")));
    }

    /// An input source whose emitter the test can fire.
    #[derive(Clone, Default)]
    struct FakeInput(Arc<Mutex<Option<Emit<OsEvent>>>>);

    impl InputSource for FakeInput {
        fn start(&mut self, events: Emit<OsEvent>) -> Result<(), SourceError> {
            *self.0.lock().unwrap() = Some(events);
            Ok(())
        }
        fn stop(&mut self) {}
    }

    impl FakeInput {
        fn key(&self, key: zc_core::hotkey::Key, down: bool) {
            let emit = self.0.lock().unwrap().clone().expect("input started");
            emit(OsEvent::Key { key, down });
        }
    }

    /// A mic that records every gate change.
    #[derive(Clone, Default)]
    struct FakeMic(Arc<Mutex<Vec<bool>>>);

    impl AudioSource for FakeMic {
        fn track(&self) -> zc_core::TrackId {
            zc_core::TrackId::Mic
        }
        fn start(
            &mut self,
            _: &zc_core::ports::AudioStart,
            _: SharedSink,
            _: Emit<HealthEvent>,
        ) -> Result<(), SourceError> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn set_gate(&mut self, open: bool) {
            self.0.lock().unwrap().push(open);
        }
    }

    #[test]
    fn alt_f10_saves_a_clip() {
        use zc_core::hotkey::Key;
        let input = FakeInput::default();
        let (engine, _) = spawn(Ports { input: Some(Box::new(input.clone())), ..Ports::default() });
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        input.key(Key::ALT, true);
        input.key(Key::f(10), true);
        // No capture runs here, so the save is refused. That still shows the hotkey reached it.
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Warning(m) if m.starts_with("Nothing to save yet")),
        );
    }

    #[test]
    fn push_to_talk_opens_and_closes_the_mic_gate() {
        use zc_core::hotkey::{Chord, Key};
        let mut settings = Settings::default();
        settings.audio.mic = MicMode::PushToTalk;
        settings.hotkeys.push_to_talk = Some(Chord::new([Key::MOUSE5]).unwrap());
        let (input, mic) = (FakeInput::default(), FakeMic::default());
        let ports = Ports {
            input: Some(Box::new(input.clone())),
            mic: Some(Box::new(mic.clone())),
            ..Ports::default()
        };
        let engine = Engine::spawn(
            EngineConfig {
                settings,
                default_output_dir: std::env::temp_dir().join("zeroclip-test"),
            },
            ports,
            Arc::new(|()| {}),
        )
        .expect("spawn");
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        input.key(Key::MOUSE5, true);
        input.key(Key::MOUSE5, false);
        // A barrier: once this is answered, the key events before it were handled.
        engine.send(Command::SaveClip).unwrap();
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Warning(m) if m.starts_with("Nothing to save")),
        );
        assert!(mic.0.lock().unwrap().ends_with(&[true, false]));
    }

    #[test]
    fn mic_indicator_follows_the_push_to_talk_key() {
        use zc_core::hotkey::{Chord, Key};
        let mut settings = screen_settings();
        settings.audio.mic = MicMode::PushToTalk;
        settings.hotkeys.push_to_talk = Some(Chord::new([Key::MOUSE5]).unwrap());
        let input = FakeInput::default();
        let ports = Ports {
            input: Some(Box::new(input.clone())),
            mic: Some(Box::new(FakeMic::default())),
            ..Ports::default()
        };
        let engine = Engine::spawn(
            EngineConfig {
                settings,
                default_output_dir: std::env::temp_dir().join("zeroclip-test"),
            },
            ports,
            Arc::new(|()| {}),
        )
        .expect("spawn");
        // The mic runs with the key up, so it is muted.
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.mic == MicState::Muted));
        input.key(Key::MOUSE5, true);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.mic == MicState::Live));
        // A repeated press changes nothing, so it publishes no status. The barrier shows that.
        engine.send(Command::PushToTalk(true)).unwrap();
        engine.send(Command::SaveClip).unwrap();
        let mut statuses = 0;
        loop {
            match engine.wait_event(TIMEOUT).expect("timed out waiting for the barrier") {
                EngineEvent::Status(_) => statuses += 1,
                EngineEvent::Warning(m) if m.starts_with("Nothing to save") => break,
                _ => {}
            }
        }
        assert_eq!(statuses, 0, "a repeated press publishes nothing");
        input.key(Key::MOUSE5, false);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.mic == MicState::Muted));
        engine.shutdown();
    }

    #[test]
    fn mic_indicator_stays_live_while_the_mic_is_always_on() {
        let mut settings = screen_settings();
        settings.audio.mic = MicMode::AlwaysOn;
        let ports = Ports { mic: Some(Box::new(FakeMic::default())), ..Ports::default() };
        let engine = Engine::spawn(
            EngineConfig {
                settings: settings.clone(),
                default_output_dir: std::env::temp_dir().join("zeroclip-test"),
            },
            ports,
            Arc::new(|()| {}),
        )
        .expect("spawn");
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.mic == MicState::Live));
        // Applying settings publishes a status again, and the mic is still live.
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = event else { unreachable!() };
        assert_eq!(status.mic, MicState::Live);
        engine.shutdown();
    }

    #[test]
    fn mic_indicator_is_off_while_the_mic_is_off() {
        // The mode is Off, so the mic stays off even with a source attached.
        let ports = Ports { mic: Some(Box::new(FakeMic::default())), ..Ports::default() };
        let (engine, _) = spawn(ports);
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = event else { unreachable!() };
        assert_eq!(status.mic, MicState::Off);
        engine.shutdown();
    }

    #[test]
    fn keystrokes_never_reach_the_log() {
        let msg = Msg::Os(OsEvent::Key { key: zc_core::hotkey::Key(0x41), down: true });
        assert_eq!(format!("{msg:?}"), "Os(Key)");
    }

    /// An audio source that records what the engine asked of it.
    #[derive(Clone, Default)]
    struct FakeAudio(Arc<Mutex<(usize, Vec<Option<u32>>)>>);

    impl AudioSource for FakeAudio {
        fn track(&self) -> zc_core::TrackId {
            zc_core::TrackId::GameAudio
        }
        fn start(
            &mut self,
            _: &AudioStart,
            _: SharedSink,
            _: Emit<HealthEvent>,
        ) -> Result<(), SourceError> {
            self.0.lock().unwrap().0 += 1;
            Ok(())
        }
        fn stop(&mut self) {}
        fn set_target_process(&mut self, pid: Option<u32>) {
            self.0.lock().unwrap().1.push(pid);
        }
    }

    #[test]
    fn game_audio_follows_the_tracked_game_only() {
        let (input, game) = (FakeInput::default(), FakeAudio::default());
        let ports = Ports {
            input: Some(Box::new(input.clone())),
            game_audio: Some(Box::new(game.clone())),
            ..Ports::default()
        };
        // Screen mode, so the audio starts with the engine. Game mode would wait for a profiled game.
        let (engine, _) = spawn_in(screen_settings(), ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let emit = input.0.lock().unwrap().clone().expect("input started");
        emit(OsEvent::Foreground { pid: 42, hwnd: 100, exe: CS2.into(), fullscreen: false });
        // Clicking the taskbar and alt-tabbing to an app without a profile must not take the audio.
        emit(OsEvent::Foreground {
            pid: 7,
            hwnd: 200,
            exe: r"C:\Windows\Explorer.EXE".into(),
            fullscreen: false,
        });
        emit(OsEvent::Foreground { pid: 9, hwnd: 300, exe: DISCORD.into(), fullscreen: false });
        wait_for_focus(&engine, "Discord.exe", false);
        let state = game.0.lock().unwrap();
        assert_eq!(state.0, 1, "started once");
        assert_eq!(state.1, [Some(42)], "set once, for the game");
    }

    #[test]
    fn synthetic_input_reaches_the_engine() {
        let input = SyntheticInput::default();
        let ports = Ports { input: Some(Box::new(input.clone())), ..Ports::default() };
        let (engine, _) = spawn(ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let injected = input.inject(OsEvent::Foreground {
            pid: 1,
            hwnd: 1,
            exe: r"C:\x\cs2.exe".into(),
            fullscreen: false,
        });
        assert!(injected, "the engine started the input source");
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.game.is_some()));
    }

    /// Starts an engine in Screen mode with a synthetic input, and returns the input too.
    fn spawn_with_input(settings: Settings) -> (EngineHandle, SyntheticInput) {
        let input = SyntheticInput::default();
        let ports = Ports { input: Some(Box::new(input.clone())), ..Ports::default() };
        let (engine, _) = spawn_in(settings, ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        (engine, input)
    }

    #[test]
    fn foreground_game_applies_its_profile() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        let EngineEvent::ProfileApplied { exe, window, clamped } =
            wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }))
        else {
            unreachable!()
        };
        assert_eq!((exe.as_str(), window, clamped), ("cs2.exe", Hns::from_secs(120), false));
        assert_eq!(input.watched(), [42], "the game's process is watched");
        engine.shutdown();
    }

    #[test]
    fn unknown_apps_keep_the_current_profile() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }));
        input.inject(OsEvent::Foreground {
            pid: 9,
            hwnd: 300,
            exe: r"C:\Windows\notepad.exe".into(),
            fullscreen: false,
        });
        // Collect what the engine says up to the status for the new focus.
        let mut events = Vec::new();
        loop {
            let event = engine.wait_event(TIMEOUT).expect("timed out waiting for an event");
            let done = matches!(&event, EngineEvent::Status(s) if s.game.as_deref() == Some("notepad.exe"));
            events.push(event);
            if done {
                break;
            }
        }
        assert!(
            !events.iter().any(|e| matches!(e, EngineEvent::ProfileApplied { .. })),
            "an unknown app must not change the profile"
        );
        let EngineEvent::Status(status) = events.pop().expect("the status") else { unreachable!() };
        assert_eq!(status.buffer_window, Hns::from_secs(120), "the game's window stays");
        assert_eq!(status.recent_games, ["notepad.exe"]);
        engine.shutdown();
    }

    #[test]
    fn profile_window_is_clamped_to_the_ram_ceiling() {
        let mut settings = screen_settings();
        settings.dvr.ram_ceiling_mib = 64;
        let max_video_bps = u64::from(settings.video.max_bitrate_kbps) * 1_000;
        let (engine, input) = spawn_with_input(settings);
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: r"C:\Games\RainbowSix.exe".into(),
            fullscreen: false,
        });
        let EngineEvent::ProfileApplied { window, clamped, .. } =
            wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }))
        else {
            unreachable!()
        };
        assert!(clamped, "5 min at this bitrate doesn't fit in 64 MiB");
        assert_eq!(window, clamp_window(Hns::from_secs(300), max_video_bps, 64 << 20).window);
        engine.shutdown();
    }

    #[test]
    fn game_exit_reverts_to_the_default_window() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }));
        input.inject(OsEvent::ProcessExited { pid: 42, exit_code: 0 });
        let EngineEvent::Status(status) =
            wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.game.is_none()))
        else {
            unreachable!()
        };
        assert_eq!(status.buffer_window, Hns::from_secs(60), "back to the default profile");
        engine.shutdown();
    }

    #[test]
    fn exit_of_an_untracked_process_is_ignored() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.game.is_some()));
        input.inject(OsEvent::ProcessExited { pid: 7, exit_code: 1 });
        // Give cs2 a window no status published so far has, so only a status made after the exit
        // was handled (one inbox, in order) can show it. It needs cs2 to still be tracked.
        let mut settings = screen_settings();
        for game in &mut settings.profiles.games {
            if game.exe.eq_ignore_ascii_case("cs2.exe") {
                game.buffer_secs = 150;
            }
        }
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        let EngineEvent::Status(status) = wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffer_window == Hns::from_secs(150)),
        ) else {
            unreachable!()
        };
        assert_eq!(status.game.as_deref(), Some("cs2.exe"), "cs2 is still tracked");
        engine.shutdown();
    }

    #[test]
    fn session_profile_records_for_the_life_of_the_game() {
        let dir = std::env::temp_dir().join(format!("zeroclip-session-{}", std::process::id()));
        let mut settings = screen_settings();
        settings.recording.output_dir = Some(dir.clone());
        let input = SyntheticInput::default();
        let ports = Ports {
            video: Some(Box::new(SyntheticVideoSource::new().realtime(true))),
            input: Some(Box::new(input.clone())),
            ..Ports::default()
        };
        let (engine, _) = spawn_in(settings, ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));

        // cs2's default profile has session recording on.
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        let EngineEvent::RecordingStarted { path } =
            wait_for(&engine, |e| matches!(e, EngineEvent::RecordingStarted { .. }))
        else {
            unreachable!()
        };
        assert!(path.to_string_lossy().contains("session"), "{}", path.display());

        input.inject(OsEvent::ProcessExited { pid: 42, exit_code: 0 });
        wait_for(&engine, |e| matches!(e, EngineEvent::RecordingStopped { .. }));
        engine.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Waits until the status names `exe` (by file name) as the focused app, with capture
    /// `capturing`. That status is published after the focus change is handled.
    fn wait_for_focus(engine: &EngineHandle, exe: &str, capturing: bool) {
        wait_for(engine, |e| {
            matches!(e, EngineEvent::Status(s)
                if s.game.as_deref() == Some(exe_name(exe)) && s.capturing == capturing)
        });
    }

    #[test]
    fn game_mode_captures_a_profiled_game_window_only() {
        let (targets, input) = (Arc::new(Mutex::new(Vec::new())), FakeInput::default());
        let video = FakeVideo { fail: false, targets: Arc::clone(&targets) };
        let ports = Ports {
            input: Some(Box::new(input.clone())),
            video: Some(Box::new(video)),
            ..Ports::default()
        };
        // No session recording: the fake video pushes nothing, so it would leave an empty file.
        let mut settings = Settings::default();
        settings.profiles.games.iter_mut().for_each(|game| game.session_recording = false);
        let (engine, _) = spawn_in(settings, ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        assert!(targets.lock().unwrap().is_empty(), "nothing is captured before a game");

        let emit = input.0.lock().unwrap().clone().expect("input started");
        emit(OsEvent::Foreground { pid: 7, hwnd: 900, exe: DISCORD.into(), fullscreen: false });
        wait_for_focus(&engine, DISCORD, false);
        emit(OsEvent::Foreground { pid: 42, hwnd: 100, exe: CS2.into(), fullscreen: false });
        wait_for_focus(&engine, CS2, true);
        assert_eq!(*targets.lock().unwrap(), [CaptureTarget::Window(100)]);

        emit(OsEvent::Foreground { pid: 7, hwnd: 900, exe: DISCORD.into(), fullscreen: false });
        wait_for_focus(&engine, DISCORD, false);
        // Alt+F10 still saves while capture is paused: the buffer is kept, so the save is only
        // "nothing buffered yet", not "capture hasn't started".
        engine.send(Command::SaveClip).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Warning(m) if m == "Nothing to save yet."));

        emit(OsEvent::Foreground { pid: 42, hwnd: 100, exe: CS2.into(), fullscreen: false });
        wait_for_focus(&engine, CS2, true);
        assert_eq!(targets.lock().unwrap().len(), 2, "the same game resumes on its window");
        engine.shutdown();
    }

    /// Focus `exe` as `pid` on the synthetic input, and wait until the engine has handled it.
    fn focus_app(
        engine: &EngineHandle,
        input: &SyntheticInput,
        pid: u32,
        hwnd: isize,
        exe: &str,
        fullscreen: bool,
    ) {
        input.inject(OsEvent::Foreground { pid, hwnd, exe: exe.into(), fullscreen });
        let name = exe_name(exe).to_owned();
        wait_for(
            engine,
            move |e| matches!(e, EngineEvent::Status(s) if s.game.as_deref() == Some(name.as_str())),
        );
    }

    /// Collects events for `period` and reports whether any of them is a focus prompt.
    fn a_focus_prompt_within(engine: &EngineHandle, period: Duration) -> bool {
        let deadline = Instant::now() + period;
        while let Some(event) =
            engine.wait_event(deadline.saturating_duration_since(Instant::now()))
        {
            if matches!(event, EngineEvent::FocusPrompt { .. }) {
                return true;
            }
        }
        false
    }

    const R6: &str = r"D:\Games\RainbowSix.exe";
    const WINDOWED: &str = r"E:\Games\Windowed.exe";

    #[test]
    fn a_second_game_waits_for_the_user_before_it_takes_over() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }));

        input.inject(OsEvent::Foreground { pid: 77, hwnd: 300, exe: R6.into(), fullscreen: false });
        let EngineEvent::FocusPrompt { current, candidate } =
            wait_for(&engine, |e| matches!(e, EngineEvent::FocusPrompt { .. }))
        else {
            unreachable!()
        };
        assert_eq!((current.as_str(), candidate.as_str()), ("cs2.exe", "RainbowSix.exe"));
        assert!(!input.watched().contains(&77), "the candidate is not tracked before the answer");

        engine.send(Command::FocusChoice(FocusChoice::Switch)).unwrap();
        let EngineEvent::ProfileApplied { exe, .. } =
            wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }))
        else {
            unreachable!()
        };
        assert_eq!(exe, "RainbowSix.exe", "Switch applies the candidate's profile");
        assert!(input.watched().contains(&77), "and watches its process");
        engine.shutdown();
    }

    #[test]
    fn the_shell_keeps_the_prompt_and_switch_still_tracks_the_candidate() {
        const EXPLORER: &str = r"C:\Windows\explorer.exe";
        let (engine, input) = spawn_with_input(screen_settings());
        focus_app(&engine, &input, 42, 100, CS2, false);
        input.inject(OsEvent::Foreground { pid: 77, hwnd: 300, exe: R6.into(), fullscreen: false });
        wait_for(&engine, |e| matches!(e, EngineEvent::FocusPrompt { .. }));

        // The taskbar takes focus while the banner is up: the prompt must survive it.
        input.inject(OsEvent::Foreground {
            pid: 5,
            hwnd: 50,
            exe: EXPLORER.into(),
            fullscreen: false,
        });
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.game.as_deref() == Some("explorer.exe")),
        );

        engine.send(Command::FocusChoice(FocusChoice::Switch)).unwrap();
        let EngineEvent::ProfileApplied { exe, .. } =
            wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }))
        else {
            unreachable!()
        };
        assert_eq!(exe, "RainbowSix.exe", "Switch still applies the candidate's profile");
        assert!(input.watched().contains(&77), "and watches its process");
        engine.shutdown();
    }

    #[test]
    fn a_quick_alt_tab_and_an_ordinary_app_never_prompt() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }));

        // Flick through a second game, then leave it before the debounce ends.
        input.inject(OsEvent::Foreground { pid: 77, hwnd: 300, exe: R6.into(), fullscreen: false });
        input.inject(OsEvent::Foreground {
            pid: 9,
            hwnd: 900,
            exe: DISCORD.into(),
            fullscreen: false,
        });
        // A windowed app without a profile is never a candidate.
        assert!(!a_focus_prompt_within(&engine, Duration::from_millis(3_500)));
        engine.shutdown();
    }

    #[test]
    fn a_fullscreen_app_without_a_profile_is_a_candidate() {
        let (engine, input) = spawn_with_input(screen_settings());
        input.inject(OsEvent::Foreground {
            pid: 42,
            hwnd: 100,
            exe: CS2.into(),
            fullscreen: false,
        });
        wait_for(&engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }));

        input.inject(OsEvent::Foreground {
            pid: 55,
            hwnd: 500,
            exe: WINDOWED.into(),
            fullscreen: true,
        });
        let EngineEvent::FocusPrompt { candidate, .. } =
            wait_for(&engine, |e| matches!(e, EngineEvent::FocusPrompt { .. }))
        else {
            unreachable!()
        };
        assert_eq!(candidate, "Windowed.exe");
        engine.shutdown();
    }

    #[test]
    fn a_pair_prompts_once_per_session() {
        let (engine, input) = spawn_with_input(screen_settings());
        focus_app(&engine, &input, 42, 100, CS2, false);
        input.inject(OsEvent::Foreground { pid: 77, hwnd: 300, exe: R6.into(), fullscreen: false });
        wait_for(&engine, |e| matches!(e, EngineEvent::FocusPrompt { .. }));
        engine.send(Command::FocusChoice(FocusChoice::Keep)).unwrap();

        // Back to the tracked game, then to the candidate again: the pair has been asked.
        focus_app(&engine, &input, 42, 100, CS2, false);
        input.inject(OsEvent::Foreground { pid: 77, hwnd: 300, exe: R6.into(), fullscreen: false });
        assert!(!a_focus_prompt_within(&engine, Duration::from_millis(3_500)));
        engine.shutdown();
    }
    #[test]
    fn screen_mode_records_the_primary_monitor_from_the_start() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let video = FakeVideo { fail: false, targets: Arc::clone(&targets) };
        let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
        let (engine, _) = spawn_in(screen_settings(), ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));
        assert_eq!(*targets.lock().unwrap(), [CaptureTarget::PrimaryMonitor]);
        engine.shutdown();
    }

    /// An audio source that pushes a 20 ms synthetic packet of its track about every 20 ms, from
    /// its own thread, until stopped. Real time, so the packets line up with the synthetic video.
    struct FeedAudio {
        track: zc_core::TrackId,
        halt: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FeedAudio {
        fn new(track: zc_core::TrackId) -> Self {
            Self { track, halt: Arc::new(AtomicBool::new(false)), thread: None }
        }
    }

    impl AudioSource for FeedAudio {
        fn track(&self) -> zc_core::TrackId {
            self.track
        }
        fn start(
            &mut self,
            _: &AudioStart,
            sink: SharedSink,
            _: Emit<HealthEvent>,
        ) -> Result<(), SourceError> {
            let halt = Arc::new(AtomicBool::new(false));
            self.halt = Arc::clone(&halt);
            let track = self.track;
            self.thread = Some(thread::spawn(move || {
                let mut stream = zc_core::testing::SyntheticStream::new();
                while !halt.load(Ordering::Relaxed) {
                    sink.push(stream.next_audio(track).view());
                    thread::sleep(Duration::from_millis(20));
                }
            }));
            Ok(())
        }
        fn stop(&mut self) {
            self.halt.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn master_leads_the_audio_tracks_of_a_saved_clip() {
        let dir = std::env::temp_dir().join(format!("zeroclip-master-{}", std::process::id()));
        let mut settings = screen_settings();
        settings.recording.output_dir = Some(dir.clone());
        let config = EngineConfig { settings, default_output_dir: dir.clone() };
        let ports = Ports {
            video: Some(Box::new(SyntheticVideoSource::new().realtime(true))),
            game_audio: Some(Box::new(FeedAudio::new(zc_core::TrackId::GameAudio))),
            master_audio: Some(Box::new(FeedAudio::new(zc_core::TrackId::Master))),
            ..Ports::default()
        };
        let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffered >= zc_core::Hns::from_secs(1)),
        );
        engine.send(Command::SaveClip).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
        let EngineEvent::ClipSaved { path, .. } = event else { unreachable!() };
        engine.shutdown();

        // A track's name is in its handler box, in declaration order, so the first mention of
        // each name says where that track is declared.
        let file = std::fs::read(&path).expect("the clip is readable");
        let first = |name: &[u8]| {
            file.windows(name.len())
                .position(|w| w == name)
                .unwrap_or_else(|| panic!("{} is not in the clip", String::from_utf8_lossy(name)))
        };
        assert!(first(b"Video") < first(b"Master"), "video is track 0");
        assert!(first(b"Master") < first(b"Game"), "Master is the first audio track");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_app_track_is_started_and_named_by_its_label_in_a_saved_clip() {
        let dir = std::env::temp_dir().join(format!("zeroclip-app-{}", std::process::id()));
        let mut settings = screen_settings();
        settings.recording.output_dir = Some(dir.clone());
        let config = EngineConfig { settings, default_output_dir: dir.clone() };
        let app = FeedAudio::new(zc_core::TrackId::audio(3).expect("slot 3 is an audio track"));
        let ports = Ports {
            video: Some(Box::new(SyntheticVideoSource::new().realtime(true))),
            game_audio: Some(Box::new(FeedAudio::new(zc_core::TrackId::GameAudio))),
            app_audio: vec![("Discord".to_owned(), Box::new(app) as Box<dyn AudioSource>)],
            ..Ports::default()
        };
        let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffered >= zc_core::Hns::from_secs(1)),
        );
        engine.send(Command::SaveClip).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
        let EngineEvent::ClipSaved { path, .. } = event else { unreachable!() };
        engine.shutdown();

        // The app track is declared after Game, and carries its settings label.
        let file = std::fs::read(&path).expect("the clip is readable");
        let first = |name: &[u8]| {
            file.windows(name.len())
                .position(|w| w == name)
                .unwrap_or_else(|| panic!("{} is not in the clip", String::from_utf8_lossy(name)))
        };
        assert!(first(b"Game") < first(b"Discord"), "the app track follows the game track");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dvr_is_sized_for_the_clamped_window_and_off_when_disabled() {
        let mut settings = Settings::default();
        settings.profiles.default.buffer_secs = 600;
        settings.dvr.ram_ceiling_mib = 64;
        let (config, clamp) = dvr_config(&settings, None);
        assert!(clamp.clamped);
        assert_eq!(config, DvrConfig::for_window(clamp.window, 20_000_000, 192_000));

        settings.dvr.enabled = false;
        let (config, _) = dvr_config(&settings, None);
        assert_eq!(config, DvrConfig { video_bytes: 0, audio_bytes: 0 });
    }

    #[test]
    fn disk_mode_keeps_only_staging_in_ram_and_ignores_the_ram_ceiling() {
        let mut settings = Settings::default();
        settings.profiles.default.buffer_secs = 600;
        settings.dvr.ram_ceiling_mib = 64;
        settings.dvr.storage = DvrStorage::Disk;
        let (config, clamp) = dvr_config(&settings, Some(4_096));
        assert!(!clamp.clamped);
        assert_eq!(clamp.window, Hns::from_secs(600));
        assert_eq!(config, staging_config(&settings));
        assert!(config.video_bytes < 64 << 20, "{config:?}");
    }

    #[test]
    fn the_ring_file_is_sized_once_for_the_largest_profile() {
        let dir = std::env::temp_dir().join(format!("zeroclip-ring-size-{}", std::process::id()));
        let mut settings = Settings::default();
        settings.dvr.storage = DvrStorage::Disk;
        settings.dvr.ring_dir = Some(dir.clone());
        settings.profiles.default.buffer_secs = 30;
        settings.profiles.games[0].buffer_secs = 900;
        let biggest = dvr_for(&settings, Hns::from_secs(900)).ring_file_bytes();
        let default = dvr_for(&settings, Hns::from_secs(30)).ring_file_bytes();
        assert!(biggest > default);

        let (engine, _) = spawn_in(settings, Ports::default());
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let ring = dir.join(disk_dvr::RING_FILE_NAME);
        assert_eq!(std::fs::metadata(&ring).unwrap().len(), biggest);
        engine.shutdown();
        assert!(ring.is_file(), "the ring file stays after a clean shutdown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unusable_ring_folder_falls_back_to_ram_with_a_warning() {
        // A file where the folder should be: the ring file can't be created under it.
        let blocker =
            std::env::temp_dir().join(format!("zeroclip-ring-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"x").unwrap();
        let mut settings = Settings::default();
        settings.dvr.storage = DvrStorage::Disk;
        settings.dvr.ring_dir = Some(blocker.clone());
        let (engine, _) = spawn_in(settings, Ports::default());
        wait_for(&engine, |e| matches!(e, EngineEvent::Warning(m) if m.contains("ring file")));
        engine.shutdown();
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn the_system_ram_share_caps_the_window_when_it_is_smaller() {
        // 4 GiB of RAM: the 5% share (204 MiB) is under the 1024 MiB setting, so a 10 min window
        // at 20 Mbps (1500 MB) is cut to what 204 MiB holds.
        let mut settings = Settings::default();
        settings.profiles.default.buffer_secs = 600;
        let (_, clamp) = dvr_config(&settings, Some(4_096));
        assert!(clamp.clamped);
        assert_eq!(clamp.window, clamp_window(Hns::from_secs(600), 20_000_000, 204 << 20).window);
    }

    #[test]
    fn a_ram_ceiling_that_cuts_the_window_is_announced() {
        let mut settings = Settings::default();
        settings.profiles.default.buffer_secs = 600;
        settings.dvr.ram_ceiling_mib = 64;
        let config = EngineConfig {
            settings,
            default_output_dir: std::env::temp_dir().join("zeroclip-test"),
        };
        let engine = Engine::spawn(config, Ports::default(), Arc::new(|()| {})).expect("spawn");
        wait_for(&engine, |e| matches!(e, EngineEvent::Warning(m) if m.contains("MiB")));
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = event else { unreachable!() };
        assert!(status.buffer_window < zc_core::Hns::from_secs(600));
    }

    #[test]
    fn status_reports_the_buffered_footage_each_second() {
        // Frames arrive at 60 fps in real time, so a tick a second in sees about a second.
        let video = SyntheticVideoSource::new().realtime(true);
        let (engine, _) =
            spawn_in(screen_settings(), Ports { video: Some(Box::new(video)), ..Ports::default() });
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffered >= zc_core::Hns::from_secs(1)),
        );
        engine.shutdown();
    }

    #[test]
    fn status_keeps_refreshing_while_keys_flood_in() {
        // During gameplay the inbox is never quiet for a second: every key press is a
        // message. The status tick must still run.
        let input = FakeInput::default();
        let video = SyntheticVideoSource::new().realtime(true);
        let ports = Ports {
            video: Some(Box::new(video)),
            input: Some(Box::new(input.clone())),
            ..Ports::default()
        };
        let (engine, _) = spawn_in(screen_settings(), ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let emit = input.0.lock().unwrap().clone().expect("input started");
        let stop = Arc::new(AtomicBool::new(false));
        let flood = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let key = Key::letter('W').expect("a letter");
                while !stop.load(Ordering::Relaxed) {
                    emit(OsEvent::Key { key, down: true });
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffered >= zc_core::Hns::from_secs(1)),
        );
        stop.store(true, Ordering::Relaxed);
        flood.join().expect("flood thread");
        engine.shutdown();
    }

    #[test]
    fn toggling_a_recording_before_capture_says_so() {
        let (engine, _) = spawn(Ports::default());
        engine.send(Command::ToggleRecording).unwrap();
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Warning(m) if m == "Nothing to record yet."),
        );
        engine.shutdown();
    }

    #[test]
    fn a_recording_that_cannot_create_its_file_is_an_error() {
        // The output folder is a plain file, so no recording file can be created in it.
        let blocker = std::env::temp_dir().join(format!("zeroclip-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"").unwrap();
        let mut settings = screen_settings();
        settings.recording.output_dir = Some(blocker.clone());
        let config = EngineConfig { settings, default_output_dir: blocker.clone() };
        let video = SyntheticVideoSource::new().realtime(true);
        let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
        let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));

        engine.send(Command::ToggleRecording).unwrap();
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Error(m) if m.starts_with("Couldn't start")),
        );
        engine.shutdown();
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn changing_video_settings_ends_the_recording_and_empties_the_dvr() {
        let dir = std::env::temp_dir().join(format!("zeroclip-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut settings = screen_settings();
        settings.recording.output_dir = Some(dir.clone());
        let config = EngineConfig { settings: settings.clone(), default_output_dir: dir.clone() };
        let video = SyntheticVideoSource::new().realtime(true);
        let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
        let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffered >= zc_core::Hns::from_secs(1)),
        );
        engine.send(Command::ToggleRecording).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::RecordingStarted { .. }));

        // A new frame rate means a new encoder: the old file and the old GOPs end here.
        settings.video.fps = 30;
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Warning(m) if m.contains("recording was stopped")),
        );
        wait_for(&engine, |e| matches!(e, EngineEvent::RecordingStopped { .. }));
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.buffered < zc_core::Hns::from_secs(1)),
        );
        engine.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A drive probe that answers the way a spinning disk does.
    fn spinning_disk(_: &Path) -> Result<Option<bool>, SourceError> {
        Ok(Some(true))
    }

    #[test]
    fn a_hard_disk_output_folder_is_announced_at_start_and_on_change() {
        let ports = Ports { seek_penalty: Some(spinning_disk), ..Ports::default() };
        let (engine, _) = spawn(ports);
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Warning(m) if m.starts_with("Clips go to a hard disk")),
        );
        // A new output folder is probed again, so the notice comes again.
        let other = std::env::temp_dir().join(format!("zeroclip-drive-{}", std::process::id()));
        let mut settings = Settings::default();
        settings.recording.output_dir = Some(other.clone());
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Warning(m) if m.starts_with("Clips go to a hard disk")),
        );
        engine.shutdown();
        let _ = std::fs::remove_dir(other);
    }

    /// A page lock that refuses, as Windows does when the working-set quota can't grow.
    fn refuse_lock(_: &[u8]) -> Result<Box<dyn Send>, SourceError> {
        Err(SourceError::Other("no working-set quota".into()))
    }

    /// Set by [`record_pin`]. Only `the_dvr_is_pinned_before_any_source_starts` pins with it.
    static PINNED: AtomicBool = AtomicBool::new(false);

    fn record_pin(_: &[u8]) -> Result<Box<dyn Send>, SourceError> {
        PINNED.store(true, Ordering::SeqCst);
        Ok(Box::new(()))
    }

    /// A video source that notes whether the DVR was already pinned when it was started.
    struct PinOrderVideo(Arc<AtomicBool>);

    impl VideoSource for PinOrderVideo {
        fn start(
            &mut self,
            _: &VideoStart,
            _: SharedSink,
            _: Emit<HealthEvent>,
        ) -> Result<Codec, SourceError> {
            self.0.store(PINNED.load(Ordering::SeqCst), Ordering::SeqCst);
            Ok(Codec::Hevc)
        }
        fn format(&self) -> Option<TrackInfo> {
            None
        }
        fn stop(&mut self) {}
        fn request_keyframe(&mut self) {}
        fn screenshot(&mut self, _: &Path) -> Result<(), SourceError> {
            Err(SourceError::Unsupported("no pixels".into()))
        }
    }

    #[test]
    fn the_dvr_is_pinned_before_any_source_starts() {
        // Pinning holds the recorder lock while Windows commits the whole buffer, so a source
        // that was already pushing would stall (audio drops after 100 ms).
        let pinned_first = Arc::new(AtomicBool::new(false));
        let ports = Ports {
            video: Some(Box::new(PinOrderVideo(Arc::clone(&pinned_first)))),
            lock_memory: Some(record_pin),
            ..Ports::default()
        };
        let (engine, _) = spawn_in(screen_settings(), ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));
        assert!(pinned_first.load(Ordering::SeqCst), "the pin ran after a source had started");
        engine.shutdown();
    }

    #[test]
    fn a_refused_page_lock_is_one_warning_and_capture_goes_on() {
        let video = SyntheticVideoSource::new().realtime(true);
        let ports = Ports {
            video: Some(Box::new(video)),
            lock_memory: Some(refuse_lock),
            ..Ports::default()
        };
        let (engine, _) = spawn_in(screen_settings(), ports);
        let mut warnings = 0;
        loop {
            match engine.wait_event(TIMEOUT).expect("status") {
                EngineEvent::Status(status) if status.capturing => break,
                EngineEvent::Warning(m) if m.starts_with("Couldn't page-lock") => warnings += 1,
                _ => {}
            }
        }
        assert_eq!(warnings, 1);
        engine.shutdown();
    }

    #[test]
    fn a_failed_drive_probe_is_silent_and_leaves_writes_unpaced() {
        fn probe_fails(_: &Path) -> Result<Option<bool>, SourceError> {
            Err(SourceError::Other("no volume".into()))
        }
        let ports = Ports { seek_penalty: Some(probe_fails), ..Ports::default() };
        let (engine, _) = spawn(ports);
        // The drive notice is sent before the first status, so reading up to it sees every one.
        loop {
            match engine.wait_event(TIMEOUT).expect("status") {
                EngineEvent::Status(_) => break,
                EngineEvent::Warning(m) => assert!(!m.contains("hard disk"), "unexpected: {m}"),
                _ => {}
            }
        }
        engine.shutdown();
    }

    #[test]
    fn dropping_the_handle_stops_the_engine() {
        let (engine, _) = spawn(Ports::default());
        drop(engine); // must not hang
    }
}
