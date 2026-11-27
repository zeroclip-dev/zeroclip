//! Retry schedule for protected (HDCP/DRM) content.
//!
//! Windows.Graphics.Capture gives no event when protected (DRM) content comes and
//! goes, and usually keeps delivering frames anyway. If starting capture does
//! fail with access denied, it is retried on this schedule: 250 ms doubling up to
//! 5 s.

use crate::time::Hns;

/// Exponential backoff with a cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    initial: Hns,
    max: Hns,
    next: Hns,
}

impl Backoff {
    /// Starts at `initial`, doubles on every retry, never exceeds `max`.
    #[must_use]
    pub fn new(initial: Hns, max: Hns) -> Self {
        Self { initial, max, next: initial }
    }

    /// The HDCP schedule: 250 ms up to 5 s.
    #[must_use]
    pub fn hdcp() -> Self {
        Self::new(Hns::from_millis(250), Hns::from_secs(5))
    }

    /// The first delay.
    #[must_use]
    pub fn initial(&self) -> Hns {
        self.initial
    }

    /// The cap.
    #[must_use]
    pub fn max(&self) -> Hns {
        self.max
    }

    /// How long to wait before the next retry; advances the schedule.
    pub fn next_delay(&mut self) -> Hns {
        let delay = Hns(self.next.ticks().min(self.max.ticks()));
        self.next = Hns(self.next.ticks().saturating_mul(2).min(self.max.ticks()));
        delay
    }

    /// Back to the initial delay, after a successful capture.
    pub fn reset(&mut self) {
        self.next = self.initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut b = Backoff::hdcp();
        let delays: Vec<i64> = (0..7).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(delays, [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000]);
    }

    #[test]
    fn backoff_reset_starts_over() {
        let mut b = Backoff::hdcp();
        for _ in 0..4 {
            b.next_delay();
        }
        b.reset();
        assert_eq!(b.next_delay(), Hns::from_millis(250));
    }
}
