//! The QueryPerformanceCounter clock, in [`Hns`].
//!
//! WGC's `SystemRelativeTime` and WASAPI's `u64QPCPosition` are both QPC-based
//! and already in 100 ns units; this clock reads "now" on the same timeline, e.g.
//! to timestamp a placeholder frame during an HDCP pause.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use zc_core::Hns;

/// Current QPC time, in 100 ns ticks.
#[must_use]
pub fn now() -> Hns {
    let mut ticks = 0_i64;
    // SAFETY: `ticks` is a valid, writable i64 for the duration of the call.
    // QPC cannot fail on Windows XP and later; if it somehow does, 0 is harmless.
    if unsafe { QueryPerformanceCounter(&raw mut ticks) }.is_err() {
        return Hns::ZERO;
    }
    ticks_to_hns(ticks, frequency())
}

/// QPC ticks per second. Fixed at boot, so read once.
#[must_use]
pub fn frequency() -> i64 {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut freq = 0_i64;
        // SAFETY: `freq` is a valid, writable i64 for the duration of the call.
        // Cannot fail on Windows XP and later; fall back to the common 10 MHz.
        match unsafe { QueryPerformanceFrequency(&raw mut freq) } {
            Ok(()) if freq > 0 => freq,
            _ => Hns::PER_SECOND,
        }
    })
}

/// Converts QPC ticks at `frequency` Hz into 100 ns ticks, without overflow.
#[must_use]
pub fn ticks_to_hns(ticks: i64, frequency: i64) -> Hns {
    let hns = i128::from(ticks) * i128::from(Hns::PER_SECOND) / i128::from(frequency.max(1));
    Hns(i64::try_from(hns).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_common_frequencies() {
        assert_eq!(ticks_to_hns(10_000_000, 10_000_000), Hns::from_secs(1));
        assert_eq!(ticks_to_hns(3_000_000, 3_000_000), Hns::from_secs(1));
        // A day of uptime at 24 MHz doesn't overflow the intermediate product.
        let day = 24_000_000_i64 * 86_400;
        assert_eq!(ticks_to_hns(day, 24_000_000), Hns::from_secs(86_400));
    }

    #[test]
    fn clock_moves_forward() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(now() > a);
    }
}
