use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zc_core::media::TrackId;
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;

/// Track 2: the microphone, gated by push-to-talk.
///
/// Phase 2 plan: `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eCapture,
/// eCommunications)` → `IAudioClient` (event-driven, shared mode) → loop: read
/// PCM; if the gate is closed, replace it with silence (same length, same
/// timestamps); optionally run `zc_core::dsp::Compressor`; AAC → `sink.push`.
#[derive(Debug)]
pub struct MicSource {
    /// Shared with the capture thread; flipped by the push-to-talk hotkey.
    gate: Arc<AtomicBool>,
    compressor: bool,
    running: bool,
}

impl MicSource {
    /// A stopped mic source. `always_on` opens the gate permanently (no PTT).
    #[must_use]
    pub fn new(always_on: bool, compressor: bool) -> Self {
        Self { gate: Arc::new(AtomicBool::new(always_on)), compressor, running: false }
    }

    /// Whether the dynamics compressor is enabled.
    #[must_use]
    pub fn compressor(&self) -> bool {
        self.compressor
    }

    /// Whether the gate is open right now.
    #[must_use]
    pub fn gate_open(&self) -> bool {
        self.gate.load(Ordering::Relaxed)
    }
}

impl AudioSource for MicSource {
    fn track(&self) -> TrackId {
        TrackId::Mic
    }

    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        health: Emit<HealthEvent>,
    ) -> Result<(), SourceError> {
        let _ = (config, sink, health);
        todo!("Phase 2: default capture endpoint, PTT gate, compressor, AAC (see the plan above)")
    }

    fn stop(&mut self) {
        if self.running {
            todo!("Phase 2: signal the capture thread, Stop() the client, join")
        }
    }

    fn set_gate(&mut self, open: bool) {
        self.gate.store(open, Ordering::Relaxed);
    }
}
