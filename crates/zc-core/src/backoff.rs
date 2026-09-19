//! Retry schedule for protected (HDCP/DRM) content (DESIGN §3).
//!
//! Windows.Graphics.Capture fails with `DXGI_ERROR_ACCESS_DENIED` while protected
//! content (a Netflix tab, say) is on screen, and gives no reliable event when it
//! goes away. So capture retries on an exponential backoff — 250 ms doubling up to
//! 5 s — and yields black/placeholder frames meanwhile, so PTS stays continuous.

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

    /// The HDCP schedule from DESIGN §3: 250 ms up to 5 s.
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
        todo!("Phase 1: doubling, capped (see the `backoff_*` specs)")
    }

    /// Back to the initial delay, after a successful capture.
    pub fn reset(&mut self) {
        todo!("Phase 1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "spec: Backoff::next_delay (Phase 1)"]
    fn backoff_doubles_up_to_the_cap() {
        let mut b = Backoff::hdcp();
        let delays: Vec<i64> = (0..7).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(delays, [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000]);
    }

    #[test]
    #[ignore = "spec: Backoff::reset (Phase 1)"]
    fn backoff_reset_starts_over() {
        let mut b = Backoff::hdcp();
        for _ in 0..4 {
            b.next_delay();
        }
        b.reset();
        assert_eq!(b.next_delay(), Hns::from_millis(250));
    }
}
