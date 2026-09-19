//! End-to-end specs for the engine, driven by synthetic sources (DESIGN §12).
//! These are the "done when" checks of Phases 3 and 4, runnable on any OS.

use std::sync::Arc;
use std::time::Duration;

use zc_core::control::{Command, EngineEvent};
use zc_core::settings::Settings;
use zc_engine::synthetic::{FaultPlan, SyntheticVideoSource};
use zc_engine::{Engine, EngineConfig, EngineHandle, Ports};

const TIMEOUT: Duration = Duration::from_secs(10);

fn engine_with(video: SyntheticVideoSource) -> EngineHandle {
    let config = EngineConfig {
        settings: Settings::default(),
        default_output_dir: std::env::temp_dir().join("zeroclip-harness"),
    };
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

#[test]
#[ignore = "spec: Phase 3 done-when — a DVR dump taken at any moment decodes from its first frame"]
fn saving_a_clip_writes_a_file() {
    let engine = engine_with(SyntheticVideoSource::new().realtime(true));
    std::thread::sleep(Duration::from_secs(3));
    engine.send(Command::SaveClip).unwrap();
    let event = wait_for(&engine, |e| matches!(e, EngineEvent::ClipSaved { .. }));
    let EngineEvent::ClipSaved { path, duration } = event else { unreachable!() };
    assert!(path.exists());
    assert!(duration.as_secs_f64() >= 2.0);
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
