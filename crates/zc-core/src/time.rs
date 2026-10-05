//! Time, in **hundred-nanosecond ticks** ("hns").
//!
//! 100 ns is the unit Windows uses everywhere this app cares about: the
//! `SystemRelativeTime` of a Windows.Graphics.Capture frame, WASAPI QPC positions
//! and Media Foundation sample times. Using it end to end means no unit
//! conversions, and no rounding drift, anywhere on the hot path.

use core::fmt;
use core::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use core::time::Duration;

use crate::media::TrackId;

/// A timestamp or a span of time, in 100 ns ticks. Signed so that differences
/// and small negative offsets (latency compensation) are representable.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hns(pub i64);

impl Hns {
    /// Zero.
    pub const ZERO: Self = Self(0);
    /// Ticks per second.
    pub const PER_SECOND: i64 = 10_000_000;
    /// Ticks per millisecond.
    pub const PER_MILLI: i64 = 10_000;

    /// From raw 100 ns ticks.
    #[must_use]
    pub const fn from_ticks(ticks: i64) -> Self {
        Self(ticks)
    }

    /// From whole milliseconds.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms * Self::PER_MILLI)
    }

    /// From whole seconds.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self(secs * Self::PER_SECOND)
    }

    /// From fractional seconds, rounded to the nearest tick. Not for the hot path.
    #[must_use]
    pub fn from_secs_f64(secs: f64) -> Self {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "saturating float-to-int cast is the intended behaviour"
        )]
        Self((secs * Self::PER_SECOND as f64).round() as i64)
    }

    /// Raw 100 ns ticks.
    #[must_use]
    pub const fn ticks(self) -> i64 {
        self.0
    }

    /// Whole milliseconds (truncated toward zero).
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0 / Self::PER_MILLI
    }

    /// Fractional seconds. For display and logging only.
    #[must_use]
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / Self::PER_SECOND as f64
    }

    /// `self - rhs`, clamped at [`Hns::ZERO`].
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        let d = self.0.saturating_sub(rhs.0);
        if d < 0 { Self::ZERO } else { Self(d) }
    }

    /// As a [`Duration`], or `None` if negative.
    #[must_use]
    pub fn to_duration(self) -> Option<Duration> {
        u64::try_from(self.0).ok().map(|t| Duration::from_nanos(t.saturating_mul(100)))
    }
}

impl From<Duration> for Hns {
    /// Saturates at `i64::MAX` ticks (about 29,000 years).
    fn from(d: Duration) -> Self {
        Self(i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX))
    }
}

impl Add for Hns {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Hns {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for Hns {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl SubAssign for Hns {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl Neg for Hns {
    type Output = Self;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl fmt::Debug for Hns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4}s", self.as_secs_f64())
    }
}

impl fmt::Display for Hns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}s", self.as_secs_f64())
    }
}

/// Frames up to this fraction of an interval early still count as on time, so a
/// jittery 60 Hz source recorded at 60 fps isn't thinned. A tuning knob.
const EARLY_SLACK_DIVISOR: i64 = 4;

/// Turns a capture stream into exactly `fps` frames per second (DESIGN §3).
///
/// WGC delivers a frame on every display refresh where something changed, which
/// gives two problems the encoder must never see:
/// - **Too many frames.** A 200 Hz display delivers up to 200 per second;
///   [`accept`](Self::accept) keeps one per interval, on a fixed grid, so the
///   average is exact even when arrivals sit on a coarse 5 ms grid.
/// - **No frames at all.** A static screen delivers nothing, which would stretch
///   GOPs far past their 1–2 s and break the DVR's eviction (DESIGN §4).
///   [`repeat_due`](Self::repeat_due) says when to re-submit the last frame.
#[derive(Clone, Copy, Debug)]
pub struct FramePacer {
    interval: Hns,
    next_due: Option<Hns>,
}

impl FramePacer {
    /// A pacer for `fps` frames per second (at least 1).
    #[must_use]
    pub fn new(fps: u32) -> Self {
        Self { interval: Hns(Hns::PER_SECOND / i64::from(fps.max(1))), next_due: None }
    }

    /// The time between frames.
    #[must_use]
    pub fn interval(&self) -> Hns {
        self.interval
    }

    /// Whether to encode a frame captured at `t`. Frames arrive in time order.
    pub fn accept(&mut self, t: Hns) -> bool {
        let slack = Hns(self.interval.ticks() / EARLY_SLACK_DIVISOR);
        if let Some(due) = self.next_due
            && t + slack < due
        {
            return false;
        }
        let due = self.next_due.unwrap_or(t);
        // Stay on the frame grid, but after a pause restart from `t` instead of
        // bursting to catch up.
        self.next_due =
            Some(if t - due >= self.interval { t + self.interval } else { due + self.interval });
        true
    }

    /// Called on a timer: if the current slot went unfilled by `now`, returns the
    /// timestamp at which to re-submit the previous frame, and books that slot.
    ///
    /// The repeat lands on the frame grid when the timer is on time, or at `now`
    /// after a longer stall, so a stall never produces a burst of repeats.
    pub fn repeat_due(&mut self, now: Hns) -> Option<Hns> {
        let due = self.next_due?;
        let slack = Hns(self.interval.ticks() / EARLY_SLACK_DIVISOR);
        // Give a real frame until a little after its slot before repeating.
        if now < due + slack {
            return None;
        }
        let t = if now - due >= self.interval { now } else { due };
        self.accept(t);
        Some(t)
    }
}

/// Maps raw capture timestamps onto the session timeline (DESIGN §6, "Master clock and PTS").
///
/// The clock is anchored once, to the `SystemRelativeTime` of the first captured
/// video frame. Every track (video, game audio, mic) is aligned against that
/// anchor — never against each other — so they cannot drift apart. WASAPI's
/// `u64QPCPosition` and WGC's `SystemRelativeTime` are both QPC-based 100 ns
/// values, so the mapping is a subtraction, plus an optional constant per-track
/// offset for latency compensation (e.g. a mic limiter's lookahead, DESIGN §6).
///
/// The anchor survives encoder re-initialisation: after the watchdog rebuilds the
/// pipeline, PTS continues from this clock rather than restarting at zero (DESIGN §8).
#[derive(Clone, Debug, Default)]
pub struct MasterClock {
    origin: Option<Hns>,
    offsets: [Hns; TrackId::COUNT],
}

impl MasterClock {
    /// An unanchored clock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Anchors the timeline at `raw`. Only the first call has any effect.
    pub fn anchor(&mut self, raw: Hns) {
        if self.origin.is_none() {
            self.origin = Some(raw);
        }
    }

    /// Whether [`anchor`](Self::anchor) has been called.
    #[must_use]
    pub fn is_anchored(&self) -> bool {
        self.origin.is_some()
    }

    /// Sets a constant correction added to every timestamp of `track`.
    pub fn set_track_offset(&mut self, track: TrackId, offset: Hns) {
        self.offsets[track.index()] = offset;
    }

    /// Converts a raw timestamp from `track` into session PTS.
    ///
    /// Returns `None` before the clock is anchored, and for samples that would land
    /// before the start of the session (negative PTS).
    #[must_use]
    pub fn to_pts(&self, track: TrackId, raw: Hns) -> Option<Hns> {
        let pts = raw - self.origin? + self.offsets[track.index()];
        (pts >= Hns::ZERO).then_some(pts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_conversions() {
        assert_eq!(Hns::from_millis(1).ticks(), 10_000);
        assert_eq!(Hns::from_secs(2), Hns::from_millis(2_000));
        assert_eq!(Hns::from_secs_f64(0.5), Hns::from_millis(500));
        assert_eq!(Hns::from(Duration::from_millis(250)), Hns::from_millis(250));
        assert_eq!(Hns::from_millis(1500).to_duration(), Some(Duration::from_millis(1500)));
        assert_eq!(Hns(-1).to_duration(), None);
        assert_eq!(Hns::from_millis(1999).as_millis(), 1999);
    }

    #[test]
    fn arithmetic() {
        let mut t = Hns::from_secs(1);
        t += Hns::from_millis(500);
        assert_eq!(t - Hns::from_secs(1), Hns::from_millis(500));
        assert_eq!(Hns::from_secs(1).saturating_sub(Hns::from_secs(2)), Hns::ZERO);
        assert_eq!(-Hns(5), Hns(-5));
        assert_eq!(format!("{}", Hns::from_millis(1234)), "1.234s");
    }

    /// Feeds frames at `hz` for one second into a pacer at `fps`; returns how many it kept.
    fn kept_in_one_second(hz: i64, fps: u32) -> usize {
        let mut pacer = FramePacer::new(fps);
        (0..hz).filter(|k| pacer.accept(Hns(k * Hns::PER_SECOND / hz))).count()
    }

    #[test]
    fn frame_pacer_thins_high_refresh_rates_to_the_target() {
        // The Phase 0 spike on a 200 Hz display: without pacing on a grid, the
        // nearest refresh after 16.7 ms is 20 ms, which gives 50 fps instead of 60.
        assert!((59..=61).contains(&kept_in_one_second(200, 60)));
        assert!((59..=61).contains(&kept_in_one_second(144, 60)));
        assert!((29..=31).contains(&kept_in_one_second(144, 30)));
    }

    #[test]
    fn frame_pacer_keeps_every_frame_of_a_matching_or_slower_source() {
        assert_eq!(kept_in_one_second(30, 60), 30, "it never duplicates");
        let mut pacer = FramePacer::new(60);
        let jitter = [1, -1, 0, 1, -1];
        let kept = (0..60)
            .filter(|&k| {
                let t = k * Hns::PER_SECOND / 60 + jitter[k as usize % 5] * Hns::PER_MILLI;
                pacer.accept(Hns(t))
            })
            .count();
        assert_eq!(kept, 60, "±1 ms jitter at 60 Hz keeps every frame");
    }

    #[test]
    fn frame_pacer_restarts_after_a_pause_without_bursting() {
        let mut pacer = FramePacer::new(60);
        assert!(pacer.accept(Hns::ZERO));
        assert!(pacer.accept(Hns::from_secs(1)), "the first frame after a gap is kept");
        let soon = Hns::from_secs(1) + Hns::from_millis(5);
        assert!(!pacer.accept(soon), "the next one waits a full interval");
        assert!(pacer.accept(Hns::from_secs(1) + pacer.interval()));
    }

    #[test]
    fn frame_pacer_repeats_on_the_grid_while_the_screen_is_static() {
        let mut pacer = FramePacer::new(60);
        let i = pacer.interval();
        assert!(pacer.accept(Hns::ZERO));
        assert_eq!(pacer.repeat_due(i), None, "the slot isn't late yet");
        // A timer ticking mid-slot fills every slot, stamped on the grid.
        for k in 1..=59 {
            let now = Hns(k * i.ticks() + i.ticks() / 2);
            assert_eq!(pacer.repeat_due(now), Some(Hns(k * i.ticks())));
        }
    }

    #[test]
    fn frame_pacer_real_frames_win_over_repeats() {
        let mut pacer = FramePacer::new(60);
        let i = pacer.interval();
        assert!(pacer.accept(Hns::ZERO));
        assert!(pacer.accept(i));
        assert_eq!(pacer.repeat_due(i + Hns(i.ticks() / 2)), None, "slot 1 was filled");
    }

    #[test]
    fn frame_pacer_does_not_burst_repeats_after_a_stall() {
        let mut pacer = FramePacer::new(60);
        assert!(pacer.accept(Hns::ZERO));
        assert_eq!(pacer.repeat_due(Hns::from_secs(1)), Some(Hns::from_secs(1)));
        assert_eq!(pacer.repeat_due(Hns::from_secs(1)), None);
    }

    #[test]
    fn master_clock_pts_is_relative_to_the_anchor() {
        let mut clock = MasterClock::new();
        clock.anchor(Hns(1_000_000));
        assert_eq!(clock.to_pts(TrackId::Video, Hns(1_500_000)), Some(Hns(500_000)));
        assert_eq!(clock.to_pts(TrackId::GameAudio, Hns(1_000_000)), Some(Hns::ZERO));
    }

    #[test]
    fn master_clock_anchor_is_set_once() {
        // A pipeline re-init re-anchoring would restart PTS at zero, which makes
        // files seek incorrectly (DESIGN §8). The first anchor wins.
        let mut clock = MasterClock::new();
        clock.anchor(Hns(100));
        clock.anchor(Hns(900));
        assert!(clock.is_anchored());
        assert_eq!(clock.to_pts(TrackId::Video, Hns(1_100)), Some(Hns(1_000)));
    }

    #[test]
    fn master_clock_rejects_samples_before_the_session() {
        let mut clock = MasterClock::new();
        assert_eq!(clock.to_pts(TrackId::Video, Hns(5)), None, "not anchored yet");
        clock.anchor(Hns(1_000));
        assert_eq!(clock.to_pts(TrackId::Mic, Hns(999)), None);
    }

    #[test]
    fn master_clock_applies_per_track_offsets() {
        // DESIGN §6: a lookahead limiter delays the mic by a constant; that delay is
        // compensated explicitly, per track, instead of being discovered later as drift.
        let mut clock = MasterClock::new();
        clock.anchor(Hns::ZERO);
        clock.set_track_offset(TrackId::Mic, Hns::from_millis(-5));
        let t = Hns::from_millis(100);
        assert_eq!(clock.to_pts(TrackId::Mic, t), Some(Hns::from_millis(95)));
        assert_eq!(clock.to_pts(TrackId::Video, t), Some(t), "other tracks untouched");
    }
}
