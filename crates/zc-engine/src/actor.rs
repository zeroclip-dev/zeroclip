use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use time::OffsetDateTime;
use time::macros::format_description;

use zc_core::control::{Command, EngineEvent, EngineStatus};
use zc_core::hotkey::{Binding, ChordMatcher, HotkeyEvent, Trigger};
use zc_core::ports::{
    AudioSource, AudioStart, CaptureTarget, Emit, InputSource, OsEvent, SourceError, VideoSource,
    VideoStart,
};
use zc_core::settings::{MicMode, Settings};
use zc_core::sink::{PacketSink, SharedSink};
use zc_core::watchdog::HealthEvent;
use zc_core::{Codec, PacketRef};

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
    /// Raw Input hotkeys + foreground tracking (`zc-input`, Phase 2).
    pub input: Option<Box<dyn InputSource>>,
}

impl fmt::Debug for Ports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ports")
            .field("video", &self.video.is_some())
            .field("game_audio", &self.game_audio.is_some())
            .field("mic", &self.mic.is_some())
            .field("input", &self.input.is_some())
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
    /// something happens (egui does) knows to repaint.
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
        let actor = Actor {
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
            packets: Arc::default(),
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

/// State owned by the engine thread.
struct Actor {
    settings: Settings,
    output_dir: PathBuf,
    ports: Ports,
    /// Our own inbox, for the emitters handed to sources.
    inbox: Sender<Msg>,
    events: Sender<EngineEvent>,
    wake: Emit<()>,
    status: EngineStatus,
    packets: Arc<PacketCounter>,
    hotkeys: ChordMatcher<HotkeyAction>,
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

/// Counts what the video source produces. A Phase 1 stand-in for the `Recorder`,
/// which needs Phase 2's `MasterClock` and Phase 3's `DvrBuffer`; Phase 3 replaces
/// it. Atomics only, so `push` never allocates or blocks (CLAUDE.md rule 2).
#[derive(Debug, Default)]
struct PacketCounter {
    packets: AtomicU64,
    keyframes: AtomicU64,
    bytes: AtomicU64,
}

impl PacketSink for PacketCounter {
    fn push(&self, packet: PacketRef<'_>) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.keyframes.fetch_add(u64::from(packet.meta.keyframe), Ordering::Relaxed);
        self.bytes.fetch_add(packet.data.len() as u64, Ordering::Relaxed);
    }
}

/// What the video source is asked to do, from the user's settings.
fn video_start(settings: &Settings) -> VideoStart {
    let v = &settings.video;
    VideoStart {
        target: CaptureTarget::PrimaryMonitor,
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

impl Actor {
    fn run(mut self, rx: &Receiver<Msg>) {
        tracing::info!(output_dir = %self.output_dir.display(), "engine started");
        self.start_sources();
        self.publish_status();
        for msg in rx {
            tracing::trace!(?msg, "engine message");
            match msg {
                Msg::Command(Command::Shutdown) => break,
                Msg::Command(command) => self.on_command(command),
                Msg::Os(event) => self.on_os_event(event),
                Msg::Health(event) => self.on_health(event),
            }
        }
        self.stop_sources();
        tracing::info!("engine stopped");
    }

    fn emit(&self, event: EngineEvent) {
        if self.events.send(event).is_ok() {
            (self.wake)(());
        }
    }

    fn not_implemented(&self, feature: &'static str, phase: u8) {
        tracing::info!(feature, phase, "requested a feature that isn't built yet");
        self.emit(EngineEvent::NotImplemented { feature, phase });
    }

    fn publish_status(&mut self) {
        self.status.buffer_window = self.settings.profiles.default.buffer();
        self.emit(EngineEvent::Status(self.status.clone()));
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
        // Phase 3: hand every source the Recorder instead of the packet counter.
        self.start_video();
        self.start_audio();

        let inbox = self.inbox.clone();
        let os_events: Emit<OsEvent> = Arc::new(move |event| {
            // Fails only while the engine is shutting down; nothing to do then.
            let _ = inbox.send(Msg::Os(event));
        });
        if let Some(Err(e)) = self.ports.input.as_mut().map(|input| input.start(os_events)) {
            self.emit(EngineEvent::Error(format!("Hotkeys unavailable: {e}")));
        }
    }

    /// Both audio tracks: 48 kHz stereo AAC at the configured bitrate.
    fn start_audio(&mut self) {
        let config = AudioStart {
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: self.settings.audio.bitrate_kbps.saturating_mul(1_000),
        };
        let health = self.health_emitter();
        let sink: SharedSink = Arc::clone(&self.packets) as SharedSink;
        let gate_open = self.settings.audio.mic == MicMode::AlwaysOn;
        let mut errors = Vec::new();
        if let Some(Err(e)) = self
            .ports
            .game_audio
            .as_mut()
            .map(|a| a.start(&config, Arc::clone(&sink), Arc::clone(&health)))
        {
            errors.push(format!("Game audio unavailable: {e}"));
        }
        if let Some(mic) = self.ports.mic.as_mut() {
            match mic.start(&config, sink, health) {
                Ok(()) => mic.set_gate(gate_open),
                Err(e) => errors.push(format!("Microphone unavailable: {e}")),
            }
        }
        for error in errors {
            self.emit(EngineEvent::Error(error));
        }
    }

    fn start_video(&mut self) {
        let health = self.health_emitter();
        let config = video_start(&self.settings);
        let sink: SharedSink = Arc::clone(&self.packets) as SharedSink;
        match self.ports.video.as_mut().map(|video| video.start(&config, sink, health)) {
            Some(Ok(codec)) => {
                tracing::info!(codec = codec.name(), "capture started");
                self.status.capturing = true;
                self.status.codec = Some(codec);
            }
            Some(Err(e)) => self.emit(EngineEvent::Error(format!("Capture unavailable: {e}"))),
            None => self.emit(EngineEvent::Warning(
                "No capture source yet: the GPU video pipeline arrives in Phase 1.".into(),
            )),
        }
    }

    fn stop_sources(&mut self) {
        let Ports { video, game_audio, mic, input } = &mut self.ports;
        if let Some(input) = input {
            input.stop();
        }
        for audio in [game_audio, mic].into_iter().flatten() {
            audio.stop();
        }
        if let Some(video) = video {
            video.stop();
            let c = &self.packets;
            tracing::info!(
                packets = c.packets.load(Ordering::Relaxed),
                keyframes = c.keyframes.load(Ordering::Relaxed),
                bytes = c.bytes.load(Ordering::Relaxed),
                "video stream totals"
            );
        }
        self.status.capturing = false;
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::SaveClip => self.not_implemented("Saving a DVR clip", 3),
            Command::ToggleRecording => self.not_implemented("Manual recording", 3),
            Command::Screenshot => self.take_screenshot(),
            Command::PushToTalk(down) => {
                if let Some(mic) = self.ports.mic.as_mut() {
                    mic.set_gate(down);
                }
            }
            Command::ApplySettings(settings) => {
                // Phase 3/4: also re-size the DVR buffer.
                let video_changed = self.settings.video != settings.video;
                self.settings = *settings;
                self.hotkeys = ChordMatcher::new(bindings(&self.settings));
                // Switching modes must never leave the push-to-talk gate open.
                if let Some(mic) = self.ports.mic.as_mut() {
                    mic.set_gate(self.settings.audio.mic == MicMode::AlwaysOn);
                }
                if video_changed && let Some(video) = self.ports.video.as_mut() {
                    tracing::info!("video settings changed; restarting capture");
                    video.stop();
                    self.status.capturing = false;
                    self.start_video();
                }
                if let Some(dir) = &self.settings.recording.output_dir {
                    self.output_dir.clone_from(dir);
                }
                tracing::info!("settings applied");
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
            // Phase 2 policy: game audio follows every focus change. Phase 4 narrows
            // it to profiled games, resolves the profile, resizes the DVR buffer and
            // maybe starts a session.
            OsEvent::Foreground { pid, exe } => {
                tracing::debug!(pid, exe, "foreground changed");
                if let Some(game) = self.ports.game_audio.as_mut() {
                    game.set_target_process(Some(pid));
                }
                self.status.game = Some(exe);
                self.publish_status();
            }
        }
    }

    fn on_health(&mut self, event: HealthEvent) {
        // Phase 4: `Watchdog::on_event` decides; carry out the action it returns.
        tracing::trace!(?event, "health");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A video source that never touches a GPU. (`SyntheticVideoSource` arrives in Phase 3.)
    struct FakeVideo {
        fail: bool,
        starts: Arc<AtomicUsize>,
    }

    impl VideoSource for FakeVideo {
        fn start(
            &mut self,
            _: &VideoStart,
            _: SharedSink,
            _: Emit<HealthEvent>,
        ) -> Result<Codec, SourceError> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(SourceError::Unsupported("no GPU in this test".into()))
            } else {
                Ok(Codec::Hevc)
            }
        }
        fn stop(&mut self) {}
        fn request_keyframe(&mut self) {}
        fn screenshot(&mut self, path: &Path) -> Result<(), SourceError> {
            std::fs::write(path, b"png").map_err(|e| SourceError::Other(e.to_string()))
        }
    }

    fn with_video(fail: bool) -> Ports {
        Ports {
            video: Some(Box::new(FakeVideo { fail, starts: Arc::default() })),
            ..Ports::default()
        }
    }

    #[test]
    fn changed_video_settings_restart_capture_once() {
        let starts = Arc::new(AtomicUsize::new(0));
        let video = FakeVideo { fail: false, starts: Arc::clone(&starts) };
        let (engine, _) = spawn(Ports { video: Some(Box::new(video)), ..Ports::default() });
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));

        let mut settings = Settings::default();
        settings.profiles.default.buffer_secs = 90; // not a video setting
        engine.send(Command::ApplySettings(Box::new(settings.clone()))).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        assert_eq!(starts.load(Ordering::Relaxed), 1, "unchanged video settings keep capturing");

        settings.video.fps = 30;
        engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));
        assert_eq!(starts.load(Ordering::Relaxed), 2);
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn spawn(ports: Ports) -> (EngineHandle, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let config = EngineConfig {
            settings: Settings::default(),
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
    fn unbuilt_features_say_which_phase_delivers_them() {
        let (engine, _) = spawn(Ports::default());
        engine.send(Command::SaveClip).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::NotImplemented { .. }));
        assert_eq!(event, EngineEvent::NotImplemented { feature: "Saving a DVR clip", phase: 3 });
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
        let (engine, _) = spawn(with_video(false));
        wait_for(
            &engine,
            |e| matches!(e, EngineEvent::Status(s) if s.capturing && s.codec == Some(Codec::Hevc)),
        );
    }

    #[test]
    fn capture_failure_is_reported_not_fatal() {
        let (engine, _) = spawn(with_video(true));
        wait_for(&engine, |e| matches!(e, EngineEvent::Error(m) if m.starts_with("Capture")));
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let EngineEvent::Status(status) = event else { unreachable!() };
        assert!(!status.capturing);
    }

    #[test]
    fn video_start_follows_settings() {
        let start = video_start(&Settings::default());
        assert_eq!(start.codec, Codec::Av1, "Auto asks for the best codec");
        assert_eq!(start.min_bitrate_bps, 5_000_000);
        assert_eq!(start.max_bitrate_bps, 20_000_000);
        assert_eq!(start.keyframe_interval, zc_core::Hns::from_secs(1));
        assert!(!start.capture_cursor);
    }

    #[test]
    fn file_stamp_is_sortable_and_has_no_colons() {
        let t = time::macros::datetime!(2023-11-14 22:13:20.5 UTC);
        assert_eq!(file_stamp(t), "2023-11-14 22-13-20.500");
    }

    #[test]
    fn screenshot_is_saved_in_the_output_folder() {
        let (engine, _) = spawn(with_video(false));
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
        // Phase 3: expect ClipSaved instead.
        wait_for(&engine, |e| {
            matches!(e, EngineEvent::NotImplemented { feature: "Saving a DVR clip", .. })
        });
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
        wait_for(&engine, |e| matches!(e, EngineEvent::NotImplemented { .. }));
        assert!(mic.0.lock().unwrap().ends_with(&[true, false]));
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
    fn game_audio_starts_and_follows_focus() {
        let (input, game) = (FakeInput::default(), FakeAudio::default());
        let ports = Ports {
            input: Some(Box::new(input.clone())),
            game_audio: Some(Box::new(game.clone())),
            ..Ports::default()
        };
        let (engine, _) = spawn(ports);
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(_)));
        let emit = input.0.lock().unwrap().clone().expect("input started");
        emit(OsEvent::Foreground { pid: 42, exe: "game.exe".into() });
        wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.game.is_some()));
        let state = game.0.lock().unwrap();
        assert_eq!(state.0, 1, "started once");
        assert_eq!(state.1, [Some(42)]);
    }

    #[test]
    fn dropping_the_handle_stops_the_engine() {
        let (engine, _) = spawn(Ports::default());
        drop(engine); // must not hang
    }
}
