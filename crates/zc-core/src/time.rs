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
    /// How much of the raw timeline is cut out for pauses. Every track subtracts the same
    /// amount, so tracks stay aligned and the buffer holds no hole where capture was stopped.
    paused_total: Hns,
    /// While capture is stopped: the PTS where the footage before the pause ended, which the
    /// first keyframe after it must continue from.
    resume_at: Option<Hns>,
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

    /// Capture stopped, and the buffer's footage ends at `end` (session PTS). Until
    /// [`resume`](Self::resume), [`is_paused`](Self::is_paused) holds. Does nothing before the
    /// clock is anchored, or if it is already paused.
    pub fn pause(&mut self, end: Hns) {
        if self.origin.is_some() && self.resume_at.is_none() {
            self.resume_at = Some(end);
        }
    }

    /// Whether capture is paused: only a video keyframe may end it (see [`resume`](Self::resume)).
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.resume_at.is_some()
    }

    /// Capture is running again; `raw` is the first video keyframe's raw timestamp. From here
    /// every track's PTS continues from where the footage ended, so the time spent paused leaves
    /// no hole in the clip and doesn't eat the DVR window.
    pub fn resume(&mut self, raw: Hns) {
        if let (Some(end), Some(origin)) = (self.resume_at.take(), self.origin) {
            let now = raw - origin - self.paused_total;
            if now > end {
                self.paused_total += now - end;
            }
        }
    }

    /// Converts a raw timestamp from `track` into session PTS.
    ///
    /// Returns `None` before the clock is anchored, and for samples that would land
    /// before the start of the session (negative PTS).
    #[must_use]
    pub fn to_pts(&self, track: TrackId, raw: Hns) -> Option<Hns> {
        let pts = raw - self.origin? - self.paused_total + self.offsets[track.index()];
        (pts >= Hns::ZERO).then_some(pts)
    }
}

/// What to do with one captured PCM buffer (see [`SampleTimeline::place`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Raw (QPC-timeline) PTS of the first frame to emit: the first silence frame
    /// if there is any, otherwise the first kept audio frame.
    pub pts: Hns,
    /// Zero frames to emit before the buffer, to close a gap.
    pub silence: u32,
    /// Frames to drop from the start of the buffer, because the device ran fast.
    pub skip: u32,
    /// The timeline (re)started here: drain the encoder before encoding this.
    pub restart: bool,
}

/// Keeps a stream of PCM buffers continuous and on the QPC timeline (DESIGN §6).
///
/// The sound card's sample clock and QPC disagree by 10-100 ppm, up to 180 ms over
/// 30 minutes. Timestamps here count samples, so they are exact and contiguous,
/// but whenever they drift more than [`TOLERANCE`](Self::TOLERANCE) from the
/// buffer's QPC time, the timeline re-aligns by inserting silence or dropping
/// frames. Pure logic, no allocation: it runs on the audio threads.
#[derive(Clone, Debug)]
pub struct SampleTimeline {
    rate: u32,
    /// (frame index, its QPC time): the reference for sample-exact timestamps.
    anchor: Option<(u64, Hns)>,
    /// Index of the next frame to be emitted.
    next: u64,
}

impl SampleTimeline {
    /// How far timestamps may drift from QPC before a correction. A calibration
    /// knob: corrections stay well under the ~45 ms where viewers notice lip-sync
    /// errors (ITU-R BT.1359).
    pub const TOLERANCE: Hns = Hns::from_millis(20);
    /// Gaps longer than this restart the timeline instead of filling with silence.
    pub const MAX_FILL: Hns = Hns::from_secs(1);

    /// A timeline for `sample_rate` frames per second.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self { rate: sample_rate.max(1), anchor: None, next: 0 }
    }

    /// The time of the next frame, `None` before the first buffer.
    #[must_use]
    pub fn expected(&self) -> Option<Hns> {
        self.anchor.map(|_| self.time_of(self.next))
    }

    /// Frames of silence owed at `now` to a track that has delivered nothing: from
    /// [`expected`](Self::expected) up to `now − margin`. Rounded down to whole frames, so
    /// the silence never reaches `now − margin`. Zero when nothing is due, or before the
    /// first buffer.
    #[must_use]
    pub fn silence_due(&self, now: Hns, margin: Hns) -> u32 {
        match self.expected() {
            Some(next) => self.frames_in(now - margin - next),
            None => 0,
        }
    }

    fn time_of(&self, n: u64) -> Hns {
        let (anchor_n, anchor_t) = self.anchor.unwrap_or((0, Hns::ZERO));
        let ticks = i128::from(n - anchor_n) * i128::from(Hns::PER_SECOND) / i128::from(self.rate);
        anchor_t + Hns(i64::try_from(ticks).unwrap_or(i64::MAX))
    }

    fn frames_in(&self, d: Hns) -> u32 {
        let frames = i128::from(d.ticks()) * i128::from(self.rate) / i128::from(Hns::PER_SECOND);
        u32::try_from(frames.max(0)).unwrap_or(u32::MAX)
    }

    /// Places a buffer of `frames` frames whose first frame was captured at `qpc`.
    pub fn place(&mut self, qpc: Hns, frames: u32) -> Placement {
        let on_time = |pts| Placement { pts, silence: 0, skip: 0, restart: false };
        let Some(_) = self.anchor else {
            return self.restart(qpc, frames);
        };
        let expected = self.time_of(self.next);
        let err = qpc - expected;
        if err > Self::MAX_FILL {
            self.restart(qpc, frames)
        } else if err > Self::TOLERANCE {
            let silence = self.frames_in(err);
            self.next += u64::from(silence) + u64::from(frames);
            Placement { silence, ..on_time(expected) }
        } else if err < -Self::TOLERANCE {
            let skip = self.frames_in(-err).min(frames);
            self.next += u64::from(frames - skip);
            Placement { skip, ..on_time(expected) }
        } else {
            self.next += u64::from(frames);
            on_time(expected)
        }
    }

    fn restart(&mut self, qpc: Hns, frames: u32) -> Placement {
        self.anchor = Some((self.next, qpc));
        self.next += u64::from(frames);
        Placement { pts: qpc, silence: 0, skip: 0, restart: true }
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
    fn a_pause_is_cut_out_of_every_track_alike() {
        let (video, audio) = (TrackId::Video, TrackId::GameAudio);
        let mut clock = MasterClock::new();
        clock.anchor(Hns::from_secs(100));
        // 10 s of footage, then capture stops.
        assert_eq!(clock.to_pts(video, Hns::from_secs(110)), Some(Hns::from_secs(10)));
        clock.pause(Hns::from_secs(10));
        assert!(clock.is_paused());
        // 50 s later the first keyframe arrives, and the audio 20 ms before it.
        clock.resume(Hns::from_secs(160));
        assert!(!clock.is_paused());
        assert_eq!(clock.to_pts(video, Hns::from_secs(160)), Some(Hns::from_secs(10)));
        let early = Hns::from_secs(160) - Hns::from_millis(20);
        assert_eq!(clock.to_pts(audio, early), Some(Hns::from_secs(10) - Hns::from_millis(20)));
        // A second pause adds to the first.
        clock.pause(Hns::from_secs(12));
        clock.resume(Hns::from_secs(200));
        assert_eq!(clock.to_pts(video, Hns::from_secs(200)), Some(Hns::from_secs(12)));
        assert_eq!(clock.to_pts(audio, Hns::from_secs(201)), Some(Hns::from_secs(13)));
    }

    #[test]
    fn pausing_an_unanchored_clock_or_twice_changes_nothing() {
        let mut clock = MasterClock::new();
        clock.pause(Hns::from_secs(5));
        assert!(!clock.is_paused());
        clock.anchor(Hns::ZERO);
        clock.pause(Hns::from_secs(5));
        clock.pause(Hns::from_secs(9));
        clock.resume(Hns::from_secs(20));
        assert_eq!(clock.to_pts(TrackId::Video, Hns::from_secs(20)), Some(Hns::from_secs(5)));
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

    const MS: i64 = Hns::PER_MILLI;
    /// 480 frames at 48 kHz: 10 ms.
    const BUF: Hns = Hns(100_000);

    #[test]
    fn sample_timeline_first_buffer_restarts() {
        let mut t = SampleTimeline::new(48_000);
        assert_eq!(t.expected(), None);
        let p = t.place(Hns(5_000), 480);
        assert_eq!(p, Placement { pts: Hns(5_000), silence: 0, skip: 0, restart: true });
        assert_eq!(t.expected(), Some(Hns(5_000) + BUF));
    }

    #[test]
    fn sample_timeline_is_sample_exact_when_on_time() {
        let mut t = SampleTimeline::new(48_000);
        let t0 = Hns(123_456_789);
        t.place(t0, 480);
        for k in 1..1_000 {
            let p = t.place(t0 + Hns(k * BUF.ticks()), 480);
            assert_eq!(
                p,
                Placement { pts: t0 + Hns(k * BUF.ticks()), silence: 0, skip: 0, restart: false }
            );
        }
    }

    #[test]
    fn sample_timeline_ignores_jitter_within_tolerance() {
        let mut t = SampleTimeline::new(48_000);
        t.place(Hns::ZERO, 480);
        for k in 1..500 {
            let noise = [5, -5, 3, -2, 0][k as usize % 5] * MS;
            let p = t.place(Hns(k * BUF.ticks() + noise), 480);
            assert_eq!((p.silence, p.skip, p.restart), (0, 0, false));
            assert_eq!(p.pts, Hns(k * BUF.ticks()), "timestamps stay sample-exact");
        }
    }

    #[test]
    fn sample_timeline_fills_a_gap_with_silence() {
        let mut t = SampleTimeline::new(48_000);
        t.place(Hns::ZERO, 480);
        let p = t.place(BUF + Hns(50 * MS), 480);
        assert_eq!((p.pts, p.silence, p.skip), (BUF, 2_400, 0));
        assert_eq!(t.expected(), Some(BUF + Hns(50 * MS) + BUF));
    }

    #[test]
    fn sample_timeline_drops_frames_when_the_device_runs_fast() {
        let mut t = SampleTimeline::new(48_000);
        t.place(Hns::ZERO, 4_800);
        let p = t.place(Hns(100 * MS - 30 * MS), 4_800);
        assert_eq!((p.pts, p.silence, p.skip), (Hns(100 * MS), 0, 1_440));
    }

    #[test]
    fn sample_timeline_restarts_after_a_long_gap() {
        let mut t = SampleTimeline::new(48_000);
        t.place(Hns::ZERO, 480);
        let late = Hns::from_secs(5);
        let p = t.place(late, 480);
        assert!(p.restart);
        assert_eq!(p.pts, late);
    }

    #[test]
    fn sample_timeline_silence_due_counts_whole_frames_up_to_the_margin() {
        let margin = Hns::from_millis(100);
        let mut t = SampleTimeline::new(48_000);
        assert_eq!(t.silence_due(Hns::from_secs(9), margin), 0, "not anchored yet");
        t.place(Hns::ZERO, 480); // audio ends at 10 ms
        assert_eq!(t.silence_due(Hns::from_millis(100), margin), 0, "audio already runs past it");
        assert_eq!(t.silence_due(Hns::from_millis(110), margin), 0, "exactly at the margin");
        assert_eq!(t.silence_due(Hns::from_millis(120), margin), 480, "10 ms is 480 frames");
        assert_eq!(t.silence_due(Hns::from_millis(121), margin), 528, "11 ms is 528 frames");
    }

    #[test]
    fn sample_timeline_silence_due_never_reaches_the_margin() {
        // Padding in odd steps, at two rates, must stop within a frame of `now - margin`,
        // never past it: a late real buffer is captured after that point.
        let margin = Hns::from_millis(100);
        for rate in [44_100, 48_000] {
            let mut t = SampleTimeline::new(rate);
            t.place(Hns::from_secs(1), 480);
            let mut now = Hns::from_secs(1);
            for _ in 0..2_000 {
                now += Hns::from_millis(7) + Hns(3_333);
                let limit = now - margin;
                let frames = t.silence_due(now, margin);
                let next = t.expected().expect("anchored");
                let p = t.place(next, frames);
                assert_eq!((p.silence, p.skip, p.restart), (0, 0, false));
                if frames > 0 {
                    let end = t.expected().expect("anchored");
                    assert!(end <= limit, "rate {rate}: silence ran to {end}, past {limit}");
                    assert!(limit - end < Hns::from_millis(1), "rate {rate}: {end} lags {limit}");
                }
            }
        }
    }

    #[test]
    fn sample_timeline_bounds_drift_over_30_minutes() {
        for ppm in [-100.0_f64, 100.0] {
            let mut t = SampleTimeline::new(48_000);
            let step = BUF.ticks() as f64 * (1.0 + ppm * 1e-6);
            for k in 0..180_000_i64 {
                let qpc = Hns((k as f64 * step).round() as i64);
                let p = t.place(qpc, 480);
                // Where the first kept frame lands, versus where it really was captured.
                let kept_at = p.pts + Hns(i64::from(p.silence) * Hns::PER_SECOND / 48_000);
                let captured_at = qpc + Hns(i64::from(p.skip) * Hns::PER_SECOND / 48_000);
                let error = (kept_at - captured_at).ticks().abs();
                assert!(
                    error <= (SampleTimeline::TOLERANCE + Hns(MS)).ticks(),
                    "{ppm} ppm, buffer {k}: {error}"
                );
            }
        }
    }
}
