use zc_core::media::TrackId;
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;

/// Track 1: the focused game's audio via WASAPI process loopback.
///
/// Phase 2 plan:
/// 1. `AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType:
///    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ProcessLoopbackParams: {
///    TargetProcessId: pid, ProcessLoopbackMode:
///    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE } }`, wrapped in a
///    `VT_BLOB` `PROPVARIANT`.
/// 2. `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
///    IAudioClient::IID, …)` with a completion handler (implement
///    `IActivateAudioInterfaceCompletionHandler`) → `IAudioClient`.
/// 3. `Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK |
///    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, …)` with an explicit format (process
///    loopback has no mix format: use 48 kHz stereo float), `SetEventHandle`,
///    `GetService::<IAudioCaptureClient>()`, `Start`.
/// 4. Loop: wait on the event → `GetBuffer(…, Some(&mut qpc_position))` (100 ns
///    units) → AAC → `sink.push`. Honour `AUDCLNT_BUFFERFLAGS_SILENT` and
///    `DATA_DISCONTINUITY` (report a gap).
/// 5. On `set_target_process`, restart against the new pid.
#[derive(Debug, Default)]
pub struct GameAudioSource {
    target_pid: Option<u32>,
    running: bool,
}

impl GameAudioSource {
    /// A stopped source with no target yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The process tree currently being captured.
    #[must_use]
    pub fn target_pid(&self) -> Option<u32> {
        self.target_pid
    }
}

impl AudioSource for GameAudioSource {
    fn track(&self) -> TrackId {
        TrackId::GameAudio
    }

    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<(), SourceError> {
        let _ = (config, sink, health);
        todo!("Phase 2: process loopback capture (see the plan above)")
    }

    fn stop(&mut self) {
        if self.running {
            todo!("Phase 2: signal the capture thread, Stop() the client, join")
        }
    }

    fn set_target_process(&mut self, pid: Option<u32>) {
        self.target_pid = pid;
        if self.running {
            todo!("Phase 2: restart capture against the new process tree")
        }
    }
}
