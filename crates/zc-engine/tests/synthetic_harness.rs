//! End-to-end specs for the engine, driven by synthetic sources (DESIGN §12).
//! These are the "done when" checks of Phases 3 and 4, runnable on any OS.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use zc_core::Hns;
use zc_core::control::{Command, EngineEvent};
use zc_core::ports::OsEvent;
use zc_core::settings::{CaptureMode, DvrStorage, Settings};
use zc_engine::synthetic::{FaultPlan, SyntheticInput, SyntheticVideoSource};
use zc_engine::{Engine, EngineConfig, EngineHandle, Ports};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The output folder [`engine_with`] uses, under the temp dir. It carries the process id so two
/// test runs at once (another `cargo test`, a second worktree) never delete each other's clips.
fn harness_dir() -> PathBuf {
    std::env::temp_dir().join(format!("zeroclip-harness-{}", std::process::id()))
}

/// The default settings in Screen mode: these specs drive a synthetic screen, not a game.
fn screen_settings() -> Settings {
    let mut settings = Settings::default();
    settings.video.capture_mode = CaptureMode::Screen;
    settings
}

fn engine_with(video: SyntheticVideoSource) -> EngineHandle {
    let config = EngineConfig { settings: screen_settings(), default_output_dir: harness_dir() };
    let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
    Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn")
}

fn wait_for(engine: &EngineHandle, mut pred: impl FnMut(&EngineEvent) -> bool) -> EngineEvent {
    loop {
        let event = engine.wait_event(TIMEOUT).expect("timed out waiting for an event");
        if pred(&event) {
            return event;
        }
    }
}

/// A folder under the temp dir for one test's files. It is removed when the test ends, even if
/// the test fails.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The pid the crash-rescue specs give cs2.
const GAME_PID: u32 = 4242;

/// A scratch output folder for one test, named after it.
fn test_dir(test: &str) -> ScratchDir {
    ScratchDir(std::env::temp_dir().join(format!("zeroclip-harness-{test}-{}", std::process::id())))
}

/// The game settings the crash-rescue specs use, writing to `dir`. cs2's session recording is
/// off: these specs are about the DVR buffer, and an unpaced source would flood a recording.
fn game_settings(dir: &Path) -> Settings {
    let mut settings = screen_settings();
    settings.recording.output_dir = Some(dir.to_path_buf());
    for game in &mut settings.profiles.games {
        game.session_recording = false;
    }
    settings
}

/// An engine on `video` with `input` as its input source, writing to `dir`.
fn engine_with_input(
    video: SyntheticVideoSource,
    input: &SyntheticInput,
    dir: &Path,
) -> EngineHandle {
    let config =
        EngineConfig { settings: game_settings(dir), default_output_dir: dir.to_path_buf() };
    let ports = Ports {
        video: Some(Box::new(video)),
        input: Some(Box::new(input.clone())),
        ..Ports::default()
    };
    Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn")
}

/// Focuses cs2 as [`GAME_PID`], and waits until its profile is in effect and the buffer holds
/// footage.
fn start_game(engine: &EngineHandle, input: &SyntheticInput) {
    // The first status comes after the sources started, the input among them; an event injected
    // earlier would be dropped.
    wait_for(engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));
    input.inject(OsEvent::Foreground {
        pid: GAME_PID,
        hwnd: 100,
        exe: r"C:\Games\cs2.exe".into(),
        fullscreen: false,
    });
    wait_for(engine, |e| matches!(e, EngineEvent::ProfileApplied { .. }));
    wait_for(engine, |e| matches!(e, EngineEvent::Status(s) if s.buffered > Hns::ZERO));
}

/// The crash-rescue files in `dir`.
fn rescue_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("recovery-"))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn saving_a_clip_writes_a_file() {
    let _scratch = ScratchDir(harness_dir());
    let engine = engine_with(SyntheticVideoSource::new().realtime(true));
    std::thread::sleep(Duration::from_secs(3));
    engine.send(Command::SaveClip).unwrap();
    let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
    let EngineEvent::ClipSaved { path, duration } = event else { unreachable!() };
    assert!(path.exists());
    assert!(duration.as_secs_f64() >= 2.0);
}

#[test]
fn disk_mode_saves_a_clip_from_the_ring_file() {
    let scratch =
        ScratchDir(std::env::temp_dir().join(format!("zeroclip-disk-dvr-{}", std::process::id())));
    let mut settings = screen_settings();
    settings.recording.output_dir = Some(scratch.0.clone());
    settings.dvr.storage = DvrStorage::Disk;
    settings.profiles.default.buffer_secs = 10;
    let config = EngineConfig { settings, default_output_dir: scratch.0.clone() };
    let video = SyntheticVideoSource::new().realtime(true);
    let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
    let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
    std::thread::sleep(Duration::from_secs(4));

    engine.send(Command::SaveClip).unwrap();
    let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
    let EngineEvent::ClipSaved { path, duration } = event else { unreachable!() };
    assert!(path.is_file());
    assert!(duration.as_secs_f64() >= 3.0, "{duration:?}");
    let ring = scratch.0.join("zeroclip-dvr.ring");
    assert!(ring.metadata().expect("the ring file exists").len() > 20_000_000);
    engine.shutdown();
    assert!(ring.is_file(), "the ring file is kept on a clean shutdown (D32)");
}

#[test]
fn saves_one_after_another_each_write_a_file() {
    let scratch =
        ScratchDir(std::env::temp_dir().join(format!("zeroclip-clips-{}", std::process::id())));
    let mut settings = screen_settings();
    settings.recording.output_dir = Some(scratch.0.clone());
    let config = EngineConfig { settings, default_output_dir: scratch.0.clone() };
    let video = SyntheticVideoSource::new().realtime(true);
    let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
    let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
    std::thread::sleep(Duration::from_secs(2));

    // The second press comes after the first clip is written, so it is not refused as busy.
    let mut paths = Vec::new();
    for _ in 0..2 {
        engine.send(Command::SaveClip).unwrap();
        let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
        let EngineEvent::ClipSaved { path, .. } = event else { unreachable!() };
        assert!(path.is_file());
        paths.push(path);
    }
    assert_ne!(paths[0], paths[1], "a second clip never overwrites the first");
    engine.shutdown();
}

#[test]
fn quitting_right_after_a_save_still_finishes_the_clip() {
    let scratch =
        ScratchDir(std::env::temp_dir().join(format!("zeroclip-quit-save-{}", std::process::id())));
    let mut settings = screen_settings();
    settings.recording.output_dir = Some(scratch.0.clone());
    let config = EngineConfig { settings, default_output_dir: scratch.0.clone() };
    let video = SyntheticVideoSource::new().realtime(true);
    let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
    let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
    std::thread::sleep(Duration::from_secs(3));

    // Alt+F10, then quit at once: the save must not be cut short.
    engine.send(Command::SaveClip).unwrap();
    engine.shutdown();

    let clips: Vec<PathBuf> = std::fs::read_dir(&scratch.0)
        .expect("output folder")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
        .collect();
    assert_eq!(clips.len(), 1, "{clips:?}");
    let mut file = std::fs::File::open(&clips[0]).expect("clip");
    let len = file.metadata().expect("metadata").len();
    let end = zc_mux::repair::last_complete_fragment_end(&mut file).expect("readable");
    assert_eq!(end, Some(len), "the clip ends on a complete fragment");
}

#[test]
fn manual_recording_writes_a_file() {
    let scratch =
        ScratchDir(std::env::temp_dir().join(format!("zeroclip-recording-{}", std::process::id())));
    let mut settings = screen_settings();
    settings.recording.output_dir = Some(scratch.0.clone());
    let config = EngineConfig { settings, default_output_dir: scratch.0.clone() };
    let video = SyntheticVideoSource::new().realtime(true);
    let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
    let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");
    wait_for(&engine, |e| matches!(e, EngineEvent::Status(s) if s.capturing));

    std::thread::sleep(Duration::from_secs(1));
    engine.send(Command::ToggleRecording).unwrap();
    wait_for(&engine, |e| matches!(e, EngineEvent::RecordingStarted { .. }));
    std::thread::sleep(Duration::from_secs(2));
    engine.send(Command::ToggleRecording).unwrap();
    let event = wait_for(&engine, |e| matches!(e, EngineEvent::RecordingStopped { .. }));
    let EngineEvent::RecordingStopped { path, duration } = event else { unreachable!() };

    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp4"));
    assert!(path.is_file());
    assert!(duration.as_secs_f64() >= 1.0, "a 2 s recording, got {duration:?}");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[4..8], b"ftyp");
    let leftovers = std::fs::read_dir(&scratch.0)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "partial"))
        .count();
    assert_eq!(leftovers, 0, "the finished recording has no .partial file left");
    engine.shutdown();
}

#[test]
fn watchdog_recovers_from_an_injected_encoder_fault() {
    use zc_core::watchdog::WatchdogState;

    let scratch =
        ScratchDir(std::env::temp_dir().join(format!("zeroclip-watchdog-{}", std::process::id())));
    let mut settings = screen_settings();
    settings.recording.output_dir = Some(scratch.0.clone());
    let config = EngineConfig { settings, default_output_dir: scratch.0.clone() };
    let faults = FaultPlan { encoder_fault_at: Some(300), ..FaultPlan::default() };
    let video = SyntheticVideoSource::new().with_faults(faults);
    let ports = Ports { video: Some(Box::new(video)), ..Ports::default() };
    let engine = Engine::spawn(config, ports, Arc::new(|()| {})).expect("spawn");

    // No footage is lost: the frames from before the fault are flushed to their own file
    // during recovery, before the re-init. The buffer then restarts empty at RESUMING
    // (DESIGN §8), so a later clip holds only frames from the new encoder.
    let mut flushed = None;
    wait_for(&engine, |e| match e {
        EngineEvent::ClipSaved { path, .. } => {
            flushed.get_or_insert_with(|| path.clone());
            false
        }
        EngineEvent::Status(s) => s.watchdog == WatchdogState::Resuming,
        _ => false,
    });
    let flushed = flushed.expect("the pre-fault footage was flushed before the re-init");
    assert!(flushed.is_file());

    // Back in Running once the new encoder's IDR is in the buffer, and recording again.
    wait_for(
        &engine,
        |e| matches!(e, EngineEvent::Status(s) if s.watchdog == WatchdogState::Running && s.capturing),
    );
    engine.send(Command::SaveClip).unwrap();
    let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
    let EngineEvent::ClipSaved { path, .. } = event else { unreachable!() };
    assert!(path.is_file());
    assert_ne!(path, flushed, "the new clip is its own file");
    engine.shutdown();
}

#[test]
fn crash_rescue_dumps_the_buffer_when_the_game_dies() {
    let dir = test_dir("crash-rescue");
    let input = SyntheticInput::default();
    let engine = engine_with_input(SyntheticVideoSource::new(), &input, &dir.0);
    start_game(&engine, &input);

    // Task Manager's End task reports exit code 1.
    input.inject(OsEvent::ProcessExited { pid: GAME_PID, exit_code: 1 });
    let event = wait_for(&engine, |e| matches!(e, EngineEvent::RecoveryReady { .. }));
    let EngineEvent::RecoveryReady { path, game, duration } = event else { unreachable!() };
    assert!(path.is_file(), "{}", path.display());
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    assert!(name.starts_with("recovery-") && name.ends_with(".mp4.tmp"), "{name}");
    assert_eq!(game, "cs2.exe");
    assert!(duration > Hns::ZERO);
    engine.shutdown();
}

#[test]
fn crash_rescue_waits_for_a_save_in_progress() {
    let dir = test_dir("crash-rescue-busy");
    let input = SyntheticInput::default();
    let engine = engine_with_input(SyntheticVideoSource::new(), &input, &dir.0);
    start_game(&engine, &input);

    // The game dies while the save is being written: one dump at a time (D29), so the rescue
    // waits for the save, and neither is lost.
    engine.send(Command::SaveClip).unwrap();
    input.inject(OsEvent::ProcessExited { pid: GAME_PID, exit_code: 0xC000_0005 });
    // The waiting rescue also lets go of the game, so the default profile applies again. That
    // status goes out when the rescue starts, before its file is written, so it is watched for here.
    let (mut saved, mut released) = (false, false);
    let event = wait_for(&engine, |e| {
        saved |= matches!(e, EngineEvent::ClipSaved { .. });
        released |= matches!(e, EngineEvent::Status(s)
            if s.game.is_none() && s.buffer_window == Hns::from_secs(60));
        matches!(e, EngineEvent::RecoveryReady { .. })
    });
    assert!(saved, "the save in progress finished before the rescue was written");
    assert!(released, "the game was let go, back to the default 60 s window");
    let EngineEvent::RecoveryReady { path, .. } = event else { unreachable!() };
    assert!(path.is_file());
    engine.shutdown();
}

#[test]
fn a_clean_game_exit_needs_no_rescue() {
    let dir = test_dir("clean-exit");
    let input = SyntheticInput::default();
    let engine = engine_with_input(SyntheticVideoSource::new(), &input, &dir.0);
    start_game(&engine, &input);

    input.inject(OsEvent::ProcessExited { pid: GAME_PID, exit_code: 0 });
    let mut rescued = false;
    wait_for(&engine, |e| {
        rescued |= matches!(e, EngineEvent::RecoveryReady { .. });
        matches!(e, EngineEvent::Status(s) if s.game.is_none())
    });
    assert!(!rescued);
    // Shutdown waits for any dump in flight, so a rescue would have its file by now.
    engine.shutdown();
    assert_eq!(rescue_files(&dir.0), Vec::<PathBuf>::new());
}

#[test]
fn only_the_tracked_game_is_rescued() {
    let dir = test_dir("untracked-exit");
    let input = SyntheticInput::default();
    let engine = engine_with_input(SyntheticVideoSource::new(), &input, &dir.0);
    start_game(&engine, &input);

    input.inject(OsEvent::ProcessExited { pid: 7, exit_code: 1 });
    // A no-op: cs2 gets a window no earlier status had, so the status showing it comes after the
    // exit was handled (one inbox, in order).
    let mut settings = game_settings(&dir.0);
    for game in &mut settings.profiles.games {
        if game.exe.eq_ignore_ascii_case("cs2.exe") {
            game.buffer_secs = 150;
        }
    }
    engine.send(Command::ApplySettings(Box::new(settings))).unwrap();
    let mut rescued = false;
    wait_for(&engine, |e| {
        rescued |= matches!(e, EngineEvent::RecoveryReady { .. });
        matches!(e, EngineEvent::Status(s) if s.buffer_window == Hns::from_secs(150))
    });
    assert!(!rescued);
    engine.shutdown();
    assert_eq!(rescue_files(&dir.0), Vec::<PathBuf>::new());
}
