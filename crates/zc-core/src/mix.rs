//! The Master mix core: one absolute 48 kHz sample timeline with a ring per source.
//!
//! Each audio source writes the PCM it captured at the sample its own track gets, and the
//! mixer reads back the sum of all sources, [`MIX_LAG`] behind real time. A source whose block
//! arrives a little late still lands in place. A sample no source wrote is silence.
//!
//! Pure logic, and nothing allocates after [`MixBus::new`]. The sources and the mixer thread
//! share the bus behind a `Mutex`, and each holds the lock only for a copy.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::time::Hns;

/// Sample rate of the mix timeline, Hz. Every source runs at this rate.
pub const MIX_RATE: u32 = 48_000;

/// How far behind real time the mix runs. A block that arrives within this still lands in place.
pub const MIX_LAG: Hns = Hns::from_millis(200);

/// Audio each input's ring holds. A block further ahead of the mix point than this is clipped.
const RING_SECONDS: i64 = 2;

/// Where the soft clip starts, as a fraction of full scale. Below it the sum passes unchanged.
const KNEE: f32 = 0.8;

/// The sample index of a point on the QPC timeline: `pts × 48 000 / 10⁷`, rounded half up.
#[must_use]
pub fn sample_index(t: Hns) -> i64 {
    let rate = i128::from(MIX_RATE);
    let per_second = i128::from(Hns::PER_SECOND);
    let index = (2 * i128::from(t.ticks()) * rate + per_second).div_euclid(2 * per_second);
    i64::try_from(index).unwrap_or(i64::MAX)
}

/// The QPC time of a sample index: the inverse of [`sample_index`], to the nearest tick.
#[must_use]
pub fn sample_time(sample: i64) -> Hns {
    let rate = i128::from(MIX_RATE);
    let per_second = i128::from(Hns::PER_SECOND);
    let ticks = (2 * i128::from(sample) * per_second + rate).div_euclid(2 * rate);
    Hns(i64::try_from(ticks).unwrap_or(i64::MAX))
}

/// Linear gain for a level in dB: 0 dB is unity, −6 dB is about half.
#[must_use]
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Locks a bus. A poisoned lock is still usable (the worst case is one half-copied block), so
/// the audio threads recover instead of panicking.
pub fn lock_bus(bus: &Mutex<MixBus>) -> MutexGuard<'_, MixBus> {
    bus.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Soft clip on a normalised sample: the identity below [`KNEE`], then a tanh roll-off that
/// approaches ±1 and never reaches it, so a loud sum bends instead of wrapping.
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()).copysign(x)
    }
}

/// A normalised sample (±1 is full scale) as PCM, soft-clipped first.
fn to_pcm(sum: f32) -> i16 {
    // The clip keeps the value inside ±1, so it fits an i16. NaN would become 0.
    (soft_clip(sum) * 32_767.0).round() as i16
}

/// Index of the first sample of `sample`'s slot in an interleaved ring of `capacity` frames.
fn offset(sample: i64, capacity: i64, channels: usize) -> usize {
    usize::try_from(sample.rem_euclid(capacity)).unwrap_or(0) * channels
}

/// Writes the mixer had to drop or clip. Read them to see whether a source runs late.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MixStats {
    /// Writes with samples the mix had already passed. Those samples were dropped.
    pub late: u64,
    /// Writes that reached past the end of the ring. Those samples were clipped.
    pub ahead: u64,
}

/// The Master mix: one ring of PCM per input, on one shared sample timeline.
///
/// Inputs are indexed `0..inputs`, in whatever order the caller chose. The mix point starts at
/// the sample given to [`anchor`](Self::anchor). Mixing moves it forward. Writes behind it are
/// dropped, and writes beyond the ring are clipped.
pub struct MixBus {
    channels: usize,
    /// Frames each ring holds.
    capacity: i64,
    /// One interleaved ring per input, `capacity × channels` samples each.
    rings: Vec<Vec<i16>>,
    /// Linear gain per input, applied when mixing.
    gains: Vec<f32>,
    /// The first sample not yet mixed. `None` until anchored.
    mixed_until: Option<i64>,
    stats: MixStats,
}

impl fmt::Debug for MixBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixBus")
            .field("inputs", &self.rings.len())
            .field("mixed_until", &self.mixed_until)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl MixBus {
    /// A bus with `inputs` inputs of `channels` interleaved channels. Allocates every ring now,
    /// at unity gain. Call it off the hot path.
    #[must_use]
    pub fn new(inputs: usize, channels: u16) -> Self {
        let channels = usize::from(channels.max(1));
        let capacity = RING_SECONDS * i64::from(MIX_RATE);
        let samples = usize::try_from(capacity).unwrap_or(0) * channels;
        Self {
            channels,
            capacity,
            rings: (0..inputs).map(|_| vec![0; samples]).collect(),
            gains: vec![1.0; inputs],
            mixed_until: None,
            stats: MixStats::default(),
        }
    }

    /// Sets an input's gain in linear terms (see [`db_to_gain`]). An index out of range is ignored.
    pub fn set_gain(&mut self, input: usize, gain: f32) {
        if let Some(g) = self.gains.get_mut(input) {
            *g = gain;
        }
    }

    /// Sets the sample the mix starts from. Only the first call has any effect. Until it is
    /// called, writes are dropped and [`mix_until`](Self::mix_until) mixes nothing.
    pub fn anchor(&mut self, sample: i64) {
        self.mixed_until.get_or_insert(sample);
    }

    /// The first sample not yet mixed, `None` before [`anchor`](Self::anchor).
    #[must_use]
    pub fn mixed_until(&self) -> Option<i64> {
        self.mixed_until
    }

    /// The drop and clip counters.
    #[must_use]
    pub fn stats(&self) -> MixStats {
        self.stats
    }

    /// Copies a block of interleaved PCM into `input`, whose first frame is sample `start`.
    ///
    /// Frames before the mix point are dropped, and frames beyond the ring are clipped. Both are
    /// counted in [`stats`](Self::stats). Allocates nothing.
    pub fn write(&mut self, input: usize, start: i64, pcm: &[i16]) {
        let channels = self.channels;
        let frames = i64::try_from(pcm.len() / channels).unwrap_or(i64::MAX);
        let Some(mixed) = self.mixed_until else {
            self.stats.late += 1;
            return;
        };
        let end = start + frames;
        if end <= mixed {
            self.stats.late += 1;
            return;
        }
        if start < mixed {
            self.stats.late += 1;
        }
        let window_end = mixed + self.capacity;
        if end > window_end {
            self.stats.ahead += 1;
        }
        let (from, until) = (start.max(mixed), end.min(window_end));
        let Some(ring) = self.rings.get_mut(input) else {
            return;
        };
        for sample in from..until {
            let block = usize::try_from(sample - start).unwrap_or(0) * channels;
            let at = offset(sample, self.capacity, channels);
            ring[at..at + channels].copy_from_slice(&pcm[block..block + channels]);
        }
    }

    /// Mixes `[mixed_until, end)` into `out` and moves the mix point up to what it wrote.
    /// Returns the frames written.
    ///
    /// `out` holds `channels` samples per frame. If it is shorter than the span, it takes what
    /// fits, and the rest waits for the next call. Each input's samples are summed with its gain,
    /// soft-clipped to ±1 of full scale, and then cleared, so a later write for the same ring
    /// slot can't replay them. Samples nothing wrote come out as silence. Allocates nothing.
    pub fn mix_until(&mut self, end: i64, out: &mut [i16]) -> usize {
        let Some(start) = self.mixed_until else {
            return 0;
        };
        let channels = self.channels;
        let room = i64::try_from(out.len() / channels).unwrap_or(i64::MAX);
        let frames = (end - start).clamp(0, room);
        let taken = out.chunks_exact_mut(channels).take(usize::try_from(frames).unwrap_or(0));
        for (sample, frame) in (start..).zip(taken) {
            let at = offset(sample, self.capacity, channels);
            for (c, value) in frame.iter_mut().enumerate() {
                let sum: f32 = self
                    .rings
                    .iter()
                    .zip(&self.gains)
                    .map(|(ring, gain)| gain * f32::from(ring[at + c]))
                    .sum();
                *value = to_pcm(sum / 32_768.0);
            }
            for ring in &mut self.rings {
                ring[at..at + channels].fill(0);
            }
        }
        self.mixed_until = Some(start + frames);
        usize::try_from(frames).unwrap_or(0)
    }
}

/// One source's handle on a shared bus: the source writes its PCM through it.
#[derive(Clone)]
pub struct MixInput {
    bus: Arc<Mutex<MixBus>>,
    input: usize,
}

impl fmt::Debug for MixInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixInput").field("input", &self.input).finish_non_exhaustive()
    }
}

impl MixInput {
    /// A handle on input `input` of `bus`.
    #[must_use]
    pub fn new(bus: Arc<Mutex<MixBus>>, input: usize) -> Self {
        Self { bus, input }
    }

    /// Copies a block in, at sample `start`. Takes the lock for the copy only.
    pub fn write(&self, start: i64, pcm: &[i16]) {
        lock_bus(&self.bus).write(self.input, start, pcm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: i64 = MIX_RATE as i64;

    /// A bus with `inputs` inputs, anchored at sample 0.
    fn bus(inputs: usize) -> MixBus {
        let mut bus = MixBus::new(inputs, 2);
        bus.anchor(0);
        bus
    }

    /// `frames` stereo frames of `value` on both channels.
    fn block(frames: usize, value: i16) -> Vec<i16> {
        vec![value; frames * 2]
    }

    /// Mixes `[mixed_until, end)` and returns the left channel of what was produced.
    fn mix_left(bus: &mut MixBus, end: i64) -> Vec<i16> {
        let mut out =
            vec![0i16; usize::try_from(end - bus.mixed_until().unwrap_or(0)).unwrap() * 2];
        let frames = bus.mix_until(end, &mut out);
        out.iter().step_by(2).take(frames).copied().collect()
    }

    #[test]
    fn mix_sample_index_rounds_pts_to_the_timeline() {
        assert_eq!(sample_index(Hns::from_millis(10)), 480);
        assert_eq!(sample_index(Hns::from_millis(7)), 336);
        assert_eq!(sample_index(Hns::ZERO), 0);
        assert_eq!(sample_index(Hns(1)), 0, "a tick is 0.0048 samples");
        assert_eq!(sample_index(Hns(104)), 0, "just under half a sample rounds down");
        assert_eq!(sample_index(Hns(105)), 1, "a little over half a sample rounds up");
        for sample in (0..200_000).step_by(997) {
            assert_eq!(sample_index(sample_time(sample)), sample, "round trip at {sample}");
        }
        assert_eq!(sample_time(480), Hns::from_millis(10));
    }

    #[test]
    fn mix_sums_two_inputs_offset_by_7_ms() {
        let mut bus = bus(2);
        bus.write(0, 0, &block(480, 1_000));
        bus.write(1, sample_index(Hns::from_millis(7)), &block(480, 500));
        let left = mix_left(&mut bus, 1_000);
        assert_eq!(left.len(), 1_000);
        assert_eq!(left[0], 1_000, "input 0 alone");
        assert_eq!(left[335], 1_000, "input 0 alone, up to 7 ms");
        assert_eq!(left[336], 1_500, "both inputs from 7 ms");
        assert_eq!(left[479], 1_500, "both inputs to input 0's end");
        assert_eq!(left[480], 500, "input 1 alone after input 0 ends");
        assert_eq!(left[815], 500, "input 1's last frame");
        assert_eq!(left[816], 0, "nothing after both blocks");
    }

    #[test]
    fn mix_missing_input_is_silence() {
        let mut bus = bus(2);
        bus.write(0, 0, &block(480, 800));
        let left = mix_left(&mut bus, 480);
        assert!(left.iter().all(|&s| s == 800), "input 1 adds nothing");
        let mut empty = self::bus(2);
        assert!(mix_left(&mut empty, 480).iter().all(|&s| s == 0));
    }

    #[test]
    fn mix_block_older_than_the_mix_point_is_dropped_and_counted() {
        let mut bus = bus(1);
        mix_left(&mut bus, 200);
        // Entirely before the mix point: nothing lands.
        bus.write(0, 150, &block(10, 900));
        assert_eq!(bus.stats(), MixStats { late: 1, ahead: 0 });
        // Straddling it: only the part at and after the point lands.
        bus.write(0, 195, &block(10, 900));
        assert_eq!(bus.stats(), MixStats { late: 2, ahead: 0 });
        // The block covers 195..205. Its frames 195..200 were dropped, and 200..205 land, so they
        // are the first five frames the mix produces from the mix point on.
        let left = mix_left(&mut bus, 300);
        assert!(left[..5].iter().all(|&s| s == 900), "frames 200..205 land");
        assert!(left[5..].iter().all(|&s| s == 0), "the frames before 200 were dropped");
    }

    #[test]
    fn mix_block_ahead_of_the_mix_point_lands_even_if_late() {
        let mut bus = bus(1);
        mix_left(&mut bus, 200);
        // Behind real time, but still ahead of what has been mixed: it lands in place.
        bus.write(0, 250, &block(10, 400));
        assert_eq!(bus.stats(), MixStats::default());
        let left = mix_left(&mut bus, 300);
        assert_eq!(left[50], 400);
        assert_eq!(left[59], 400);
        assert_eq!(left[60], 0);
    }

    #[test]
    fn mix_gain_applies_per_input() {
        let mut bus = bus(2);
        bus.set_gain(0, db_to_gain(-6.0206));
        bus.write(0, 0, &block(4, 1_000));
        bus.write(1, 0, &block(4, 1_000));
        let left = mix_left(&mut bus, 4);
        assert_eq!(left, [1_500, 1_500, 1_500, 1_500], "half of 1000 plus 1000");
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_gain(12.0) - 3.981).abs() < 1e-3);
    }

    #[test]
    fn mix_clipping_never_wraps() {
        let mut bus = bus(2);
        bus.write(0, 0, &block(2, 30_000));
        bus.write(1, 0, &block(2, 30_000));
        let loud = mix_left(&mut bus, 2);
        assert!(loud.iter().all(|&s| s > 30_000), "bends toward full scale: {loud:?}");

        let mut bus = bus_negative();
        assert!(mix_left(&mut bus, 2).iter().all(|s| (-32_767..-30_000).contains(s)));

        let mut quiet = self::bus(1);
        quiet.write(0, 0, &block(2, 20_000));
        let left = mix_left(&mut quiet, 2);
        assert!(left.iter().all(|&s| (s - 20_000).abs() <= 1), "below the knee nothing changes");
    }

    /// A bus whose two inputs both sit at -30 000 from sample 0.
    fn bus_negative() -> MixBus {
        let mut bus = self::bus(2);
        bus.write(0, 0, &block(2, -30_000));
        bus.write(1, 0, &block(2, -30_000));
        bus
    }

    #[test]
    fn mix_consumed_samples_are_cleared_before_the_ring_reuses_them() {
        let mut bus = bus(1);
        bus.write(0, 0, &block(10, 1_000));
        assert!(mix_left(&mut bus, 10).iter().all(|&s| s == 1_000));
        // Sample 96 000 shares a slot with sample 0. Nothing wrote 96 000 yet, so it must be silent.
        bus.write(0, 96_000, &block(10, 300));
        let mut out = vec![0i16; 2 * 95_990];
        assert_eq!(bus.mix_until(96_000, &mut out), 95_990);
        assert!(out.iter().all(|&s| s == 0), "the old 1000s were replayed");
        let left = mix_left(&mut bus, 96_010);
        assert!(left.iter().all(|&s| s == 300), "the new block lands at its own sample");
    }

    #[test]
    fn mix_write_beyond_the_ring_is_clipped_and_counted() {
        let mut bus = bus(1);
        let two_seconds = usize::try_from(2 * RATE).unwrap() + 10;
        bus.write(0, 0, &block(two_seconds, 700));
        assert_eq!(bus.stats().ahead, 1);
        let left = mix_left(&mut bus, 2 * RATE);
        assert!(left.iter().all(|&s| s == 700), "the first two seconds land");
    }

    #[test]
    fn mix_output_is_limited_to_the_buffer_and_resumes() {
        let mut bus = bus(1);
        bus.write(0, 0, &block(100, 250));
        let mut out = vec![0i16; 2 * 60];
        assert_eq!(bus.mix_until(100, &mut out), 60, "the buffer holds 60 frames");
        assert_eq!(bus.mixed_until(), Some(60));
        assert_eq!(bus.mix_until(100, &mut out), 40, "the rest comes next");
        assert_eq!(bus.mixed_until(), Some(100));
        assert_eq!(bus.mix_until(100, &mut out), 0, "nothing past the end");
    }

    #[test]
    fn mix_unanchored_bus_drops_writes_and_mixes_nothing() {
        let mut bus = MixBus::new(1, 2);
        bus.write(0, 0, &block(10, 500));
        assert_eq!(bus.stats().late, 1, "written before the mix started");
        let mut out = vec![0i16; 20];
        assert_eq!(bus.mix_until(10, &mut out), 0);
        bus.anchor(5);
        bus.anchor(99);
        assert_eq!(bus.mixed_until(), Some(5), "the first anchor wins");
        assert_eq!(bus.mix_until(10, &mut out), 5);
        assert!(out.iter().all(|&s| s == 0), "the early block was dropped");
    }
}
