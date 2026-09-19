//! The encoder watchdog, as an explicit state machine (DESIGN §8).
//!
//! Rebuilding a D3D11 device and encoder mid-session while the ring buffer is
//! live will corrupt the buffer if done ad hoc. So recovery is a fixed sequence,
//! and this type decides every step:
//!
//! ```text
//! RUNNING
//!   │  10 consecutive dropped frames, or an encoder HRESULT fault
//!   ▼
//! DRAINING   stop accepting frames; flush the encoder's in-flight samples into
//!   │        the buffer; freeze the GOP index
//!   ▼
//! FLUSHING   write every complete GOP to disk (nothing is lost)
//!   ▼
//! REINIT     tear down and rebuild the D3D11 device + MF encoder;
//!   │        on repeated failure, step down the codec ladder (§3)
//!   ▼
//! RESUMING   new IDR; buffer restarts from empty; PTS continues from the
//!   │        master clock, not from zero
//!   ▼
//! RUNNING
//! ```
//!
//! A slow disk producing back-pressure looks like an encoder stall from the
//! inside, but needs a different response: pace the writes, don't re-init.
//!
//! The watchdog is pure logic: the engine feeds it [`HealthEvent`]s and carries
//! out the [`WatchdogAction`]s it returns. That keeps it deterministic and
//! testable without a GPU (see the synthetic-load harness in `zc-engine`).

use crate::media::Codec;
use crate::time::Hns;

/// Where the pipeline is in the recovery sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WatchdogState {
    /// Healthy: frames flow capture → encoder → sinks.
    #[default]
    Running,
    /// Stopped accepting frames; waiting for the encoder to drain.
    Draining,
    /// Writing every complete GOP to disk.
    Flushing,
    /// Rebuilding the device and encoder.
    Reinit,
    /// Rebuilt; waiting for the first IDR from the new encoder.
    Resuming,
    /// Every codec on the ladder failed to re-initialise. Capture is stopped
    /// and the user has been told.
    Failed,
}

/// Health signals reported by the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthEvent {
    /// A frame reached the encoder, `interval` after the previous one.
    FrameDelivered {
        /// Time since the previous frame.
        interval: Hns,
    },
    /// A captured frame was dropped before encoding.
    FrameDropped,
    /// The encoder returned a failing `HRESULT`.
    EncoderFault {
        /// The raw `HRESULT` value, for logging.
        hresult: i32,
    },
    /// A disk write completed, and took `latency`.
    DiskWrite {
        /// How long the write took.
        latency: Hns,
    },
    /// The encoder has emitted all in-flight samples (answer to [`WatchdogAction::Drain`]).
    Drained,
    /// Every complete GOP is on disk (answer to [`WatchdogAction::FlushToDisk`]).
    Flushed,
    /// The device and encoder were rebuilt (answer to [`WatchdogAction::Reinit`]).
    ReinitSucceeded,
    /// Rebuilding failed (answer to [`WatchdogAction::Reinit`]).
    ReinitFailed,
    /// The new encoder produced its first IDR frame (answer to [`WatchdogAction::Resume`]).
    KeyframeEmitted,
}

/// What the engine must do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Nothing.
    None,
    /// Stop feeding frames; drain the encoder into the buffer; freeze the GOP index.
    Drain,
    /// Write every complete GOP in the buffer to disk.
    FlushToDisk,
    /// Tear down and rebuild the D3D11 device and encoder, using `codec`.
    Reinit {
        /// Codec to rebuild with (may be lower on the ladder than before).
        codec: Codec,
    },
    /// Restart capture: request an IDR, clear the buffer, keep the master clock.
    Resume,
    /// The disk, not the encoder, is the bottleneck: slow writes down; don't re-init.
    PaceDiskWrites,
    /// Recovery is impossible: stop capture and alert the user.
    GiveUp,
}

/// Tuning knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// Consecutive dropped frames that trigger recovery (DESIGN §8: 10).
    pub max_consecutive_drops: u32,
    /// Re-init attempts with one codec before stepping down the ladder.
    pub reinit_attempts_per_codec: u32,
    /// A disk write slower than this counts as back-pressure.
    pub slow_disk_write: Hns,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            max_consecutive_drops: 10,
            reinit_attempts_per_codec: 2,
            slow_disk_write: Hns::from_millis(500),
        }
    }
}

/// The watchdog. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct Watchdog {
    config: WatchdogConfig,
    state: WatchdogState,
    codec: Codec,
    #[expect(dead_code, reason = "scaffold: read once on_event is implemented")]
    consecutive_drops: u32,
    #[expect(dead_code, reason = "scaffold: read once on_event is implemented")]
    failed_reinits: u32,
}

impl Watchdog {
    /// A watchdog for a pipeline currently running `codec`.
    #[must_use]
    pub fn new(config: WatchdogConfig, codec: Codec) -> Self {
        Self {
            config,
            state: WatchdogState::Running,
            codec,
            consecutive_drops: 0,
            failed_reinits: 0,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> WatchdogState {
        self.state
    }

    /// Codec the pipeline is (or will be) running.
    #[must_use]
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> WatchdogConfig {
        self.config
    }

    /// Feeds one health event and returns what to do about it.
    ///
    /// Events that make no sense in the current state (e.g. `Flushed` while
    /// `Running`) are ignored and return [`WatchdogAction::None`].
    pub fn on_event(&mut self, event: HealthEvent) -> WatchdogAction {
        let _ = event;
        todo!("Phase 4: the recovery state machine (see the `watchdog_*` specs)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watchdog(codec: Codec) -> Watchdog {
        Watchdog::new(WatchdogConfig::default(), codec)
    }

    fn drops(w: &mut Watchdog, n: u32) -> WatchdogAction {
        let mut last = WatchdogAction::None;
        for _ in 0..n {
            last = w.on_event(HealthEvent::FrameDropped);
        }
        last
    }

    /// Drives a running watchdog to the point where it asks for a re-init.
    fn fault_to_reinit(w: &mut Watchdog) -> WatchdogAction {
        assert_eq!(w.on_event(HealthEvent::EncoderFault { hresult: -1 }), WatchdogAction::Drain);
        assert_eq!(w.on_event(HealthEvent::Drained), WatchdogAction::FlushToDisk);
        w.on_event(HealthEvent::Flushed)
    }

    #[test]
    fn watchdog_starts_running() {
        let w = watchdog(Codec::Hevc);
        assert_eq!(w.state(), WatchdogState::Running);
        assert_eq!(w.codec(), Codec::Hevc);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_drains_after_ten_consecutive_drops() {
        let mut w = watchdog(Codec::Hevc);
        assert_eq!(drops(&mut w, 9), WatchdogAction::None);
        assert_eq!(w.state(), WatchdogState::Running);
        assert_eq!(drops(&mut w, 1), WatchdogAction::Drain);
        assert_eq!(w.state(), WatchdogState::Draining);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_resets_the_drop_count_when_a_frame_gets_through() {
        let mut w = watchdog(Codec::Hevc);
        drops(&mut w, 9);
        let ok = HealthEvent::FrameDelivered { interval: Hns::from_millis(16) };
        assert_eq!(w.on_event(ok), WatchdogAction::None);
        assert_eq!(drops(&mut w, 9), WatchdogAction::None);
        assert_eq!(w.state(), WatchdogState::Running);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_drains_immediately_on_encoder_fault() {
        let mut w = watchdog(Codec::Hevc);
        let fault = HealthEvent::EncoderFault { hresult: 0x8000_4005_u32.cast_signed() };
        assert_eq!(w.on_event(fault), WatchdogAction::Drain);
        assert_eq!(w.state(), WatchdogState::Draining);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_paces_a_slow_disk_instead_of_reinitialising() {
        let mut w = watchdog(Codec::Hevc);
        let slow = HealthEvent::DiskWrite { latency: Hns::from_secs(2) };
        assert_eq!(w.on_event(slow), WatchdogAction::PaceDiskWrites);
        assert_eq!(w.state(), WatchdogState::Running);
        let fast = HealthEvent::DiskWrite { latency: Hns::from_millis(5) };
        assert_eq!(w.on_event(fast), WatchdogAction::None);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_runs_the_full_recovery_sequence() {
        let mut w = watchdog(Codec::Hevc);
        assert_eq!(fault_to_reinit(&mut w), WatchdogAction::Reinit { codec: Codec::Hevc });
        assert_eq!(w.state(), WatchdogState::Reinit);
        assert_eq!(w.on_event(HealthEvent::ReinitSucceeded), WatchdogAction::Resume);
        assert_eq!(w.state(), WatchdogState::Resuming);
        assert_eq!(w.on_event(HealthEvent::KeyframeEmitted), WatchdogAction::None);
        assert_eq!(w.state(), WatchdogState::Running);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_ignores_frame_events_while_recovering() {
        let mut w = watchdog(Codec::Hevc);
        drops(&mut w, 10);
        assert_eq!(w.state(), WatchdogState::Draining);
        assert_eq!(drops(&mut w, 25), WatchdogAction::None, "no second Drain");
        let fault = HealthEvent::EncoderFault { hresult: -1 };
        assert_eq!(w.on_event(fault), WatchdogAction::None);
        assert_eq!(w.state(), WatchdogState::Draining);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_ignores_out_of_order_events() {
        let mut w = watchdog(Codec::Hevc);
        for e in [HealthEvent::Drained, HealthEvent::Flushed, HealthEvent::ReinitSucceeded] {
            assert_eq!(w.on_event(e), WatchdogAction::None);
            assert_eq!(w.state(), WatchdogState::Running);
        }
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_steps_down_the_codec_ladder_after_repeated_reinit_failures() {
        let mut w = watchdog(Codec::Av1);
        assert_eq!(fault_to_reinit(&mut w), WatchdogAction::Reinit { codec: Codec::Av1 });
        // Two attempts per codec, then the next codec down.
        let retry = w.on_event(HealthEvent::ReinitFailed);
        assert_eq!(retry, WatchdogAction::Reinit { codec: Codec::Av1 });
        let step_down = w.on_event(HealthEvent::ReinitFailed);
        assert_eq!(step_down, WatchdogAction::Reinit { codec: Codec::Hevc });
        assert_eq!(w.codec(), Codec::Hevc);
        assert_eq!(w.on_event(HealthEvent::ReinitSucceeded), WatchdogAction::Resume);
    }

    #[test]
    #[ignore = "spec: Watchdog::on_event (Phase 4)"]
    fn watchdog_gives_up_when_the_ladder_is_exhausted() {
        let mut w = watchdog(Codec::H264);
        assert_eq!(fault_to_reinit(&mut w), WatchdogAction::Reinit { codec: Codec::H264 });
        assert_eq!(
            w.on_event(HealthEvent::ReinitFailed),
            WatchdogAction::Reinit { codec: Codec::H264 }
        );
        assert_eq!(w.on_event(HealthEvent::ReinitFailed), WatchdogAction::GiveUp);
        assert_eq!(w.state(), WatchdogState::Failed);
        assert_eq!(w.on_event(HealthEvent::FrameDropped), WatchdogAction::None, "stays failed");
    }
}
