//! End-to-end specs for the engine, driven by synthetic sources (DESIGN §12).
//! These are the "done when" checks of Phases 3 and 4, runnable on any OS.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use zc_core::control::{Command, EngineEvent};
use zc_core::settings::{CaptureMode, Settings};
use zc_engine::synthetic::{FaultPlan, SyntheticVideoSource};
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

fn wait_for(engine: &EngineHandle, pred: impl Fn(&EngineEvent) -> bool) -> EngineEvent {
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
#[ignore = "spec: Phase 4 done-when — the watchdog recovers without losing buffered footage"]
fn watchdog_recovers_from_an_injected_encoder_fault() {
    let faults = FaultPlan { encoder_fault_at: Some(300), ..FaultPlan::default() };
    let engine = engine_with(SyntheticVideoSource::new().with_faults(faults));
    // The recovery sequence ends back in Running, and a clip saved afterwards
    // still contains the frames from before the fault (they were flushed).
    wait_for(
        &engine,
        |e| matches!(e, EngineEvent::Status(s) if s.watchdog == zc_core::watchdog::WatchdogState::Resuming),
    );
    engine.send(Command::SaveClip).unwrap();
    wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
}
