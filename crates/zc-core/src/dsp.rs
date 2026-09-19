//! Mic dynamics compressor (DESIGN §6, "Track 2 — microphone").
//!
//! Mic track only: it stops shouting from clipping and lifts quiet callouts.
//! Game audio is never compressed — flattening its dynamic range is exactly what
//! makes clips sound worse.
//!
//! Latency gotcha: a lookahead limiter delays the signal by its lookahead, and that
//! delay must be compensated in the PTS mapping
//! ([`MasterClock::set_track_offset`](crate::time::MasterClock::set_track_offset)),
//! or the mic drifts ahead of the video by exactly that much. This compressor has
//! no lookahead, so [`Compressor::latency`] is zero — keep it that way or update it.

use crate::time::Hns;

/// Compressor settings. The defaults are the starting point from DESIGN §6.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorParams {
    /// Level above which gain reduction starts, in dBFS.
    pub threshold_db: f32,
    /// Input dB over the threshold per output dB over the threshold (3.0 = 3:1).
    pub ratio: f32,
    /// How fast gain reduction engages, in milliseconds.
    pub attack_ms: f32,
    /// How fast it lets go, in milliseconds.
    pub release_ms: f32,
    /// Gain added after compression, in dB.
    pub makeup_db: f32,
    /// Sample rate of the signal, in Hz.
    pub sample_rate: u32,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_db: 0.0,
            sample_rate: 48_000,
        }
    }
}

impl CompressorParams {
    /// The static curve: steady-state output level for a steady input level, in dBFS.
    /// Below the threshold the signal passes unchanged (plus makeup gain); above it,
    /// every `ratio` dB of input yields 1 dB of output.
    #[must_use]
    pub fn output_level_db(&self, input_db: f32) -> f32 {
        let _ = input_db;
        todo!("Phase 2: the static gain curve (see the `compressor_*` specs)")
    }
}

/// A feed-forward compressor with attack/release smoothing, processing
/// interleaved `f32` samples in place.
#[derive(Clone, Debug)]
pub struct Compressor {
    params: CompressorParams,
    // Phase 2: envelope follower state and attack/release coefficients go here.
}

impl Compressor {
    /// A compressor with `params`.
    #[must_use]
    pub fn new(params: CompressorParams) -> Self {
        Self { params }
    }

    /// The parameters in use.
    #[must_use]
    pub fn params(&self) -> CompressorParams {
        self.params
    }

    /// Compresses `samples` (interleaved, all channels linked) in place.
    /// Runs on the audio thread: must not allocate or lock.
    pub fn process(&mut self, samples: &mut [f32]) {
        let _ = samples;
        todo!("Phase 2: envelope follower + gain computer (see the `compressor_*` specs)")
    }

    /// Delay this processor adds to the signal. Zero: there is no lookahead.
    #[must_use]
    pub fn latency(&self) -> Hns {
        Hns::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_to_linear(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    #[test]
    #[ignore = "spec: CompressorParams::output_level_db (Phase 2)"]
    fn compressor_curve_passes_quiet_signals_unchanged() {
        let p = CompressorParams::default();
        assert!((p.output_level_db(-30.0) - -30.0).abs() < 1e-4);
        assert!((p.output_level_db(-18.0) - -18.0).abs() < 1e-4);
    }

    #[test]
    #[ignore = "spec: CompressorParams::output_level_db (Phase 2)"]
    fn compressor_curve_applies_the_ratio_above_threshold() {
        let p = CompressorParams::default();
        // -6 dBFS is 12 dB over the threshold → 4 dB over at 3:1 → -14 dBFS.
        assert!((p.output_level_db(-6.0) - -14.0).abs() < 1e-4);
        let with_makeup = CompressorParams { makeup_db: 6.0, ..p };
        assert!((with_makeup.output_level_db(-6.0) - -8.0).abs() < 1e-4);
    }

    #[test]
    #[ignore = "spec: Compressor::process (Phase 2)"]
    fn compressor_settles_on_the_static_curve() {
        let mut c = Compressor::new(CompressorParams::default());
        // One second of a steady -6 dBFS signal (DC keeps peak == RMS).
        let mut samples = vec![db_to_linear(-6.0); 48_000];
        c.process(&mut samples);
        let settled = samples[samples.len() - 1];
        let expected = db_to_linear(-14.0);
        assert!((settled - expected).abs() / expected < 0.05, "{settled} vs {expected}");
    }

    #[test]
    #[ignore = "spec: Compressor::process (Phase 2)"]
    fn compressor_leaves_silence_silent() {
        let mut c = Compressor::new(CompressorParams { makeup_db: 12.0, ..Default::default() });
        let mut samples = vec![0.0; 4_800];
        c.process(&mut samples);
        assert!(samples.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn compressor_adds_no_latency() {
        assert_eq!(Compressor::new(CompressorParams::default()).latency(), Hns::ZERO);
    }
}
