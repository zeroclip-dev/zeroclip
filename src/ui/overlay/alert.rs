//! The pure parts of the failure popup: how long it stays, how its text is cut to fit, and when a
//! repeat of the same message is shown again. No GPUI and no Win32 here, so these run in CI.

use std::time::{Duration, Instant};

/// How bad a failure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertLevel {
    Warning,
    Error,
}

/// How long the popup stays up in all, fades included. An error stays until closed or this long.
pub fn lifetime(level: AlertLevel) -> Duration {
    match level {
        AlertLevel::Error => Duration::from_secs(8),
        AlertLevel::Warning => Duration::from_secs(5),
    }
}

/// The same message inside this window is one popup with a count, not a new popup each time.
pub const COALESCE: Duration = Duration::from_secs(30);

/// A line cut to `max` characters, with an ellipsis if anything was cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// Longest title and detail, in characters. The popup shows one line of each.
const TITLE_MAX: usize = 34;
const DETAIL_MAX: usize = 56;

/// Splits an engine message into a title and one detail line. "Mic lost: it was unplugged." becomes
/// "Mic lost" and "it was unplugged."; a message with no such break gets a generic title.
pub fn split(level: AlertLevel, message: &str) -> (String, String) {
    let message = message.trim();
    let cut = [": ", ". "]
        .iter()
        .filter_map(|sep| message.find(sep).map(|at| (at, sep.len())))
        .min()
        .filter(|&(at, _)| at > 0 && at <= TITLE_MAX);
    match cut {
        Some((at, sep)) => {
            let rest = message[at + sep..].trim();
            let title = message[..at].trim_end_matches(['.', ':']);
            (truncate(title, TITLE_MAX), truncate(rest, DETAIL_MAX))
        }
        None => {
            let title = match level {
                AlertLevel::Error => "Recording problem",
                AlertLevel::Warning => "ZeroClip warning",
            };
            (title.to_owned(), truncate(message, DETAIL_MAX))
        }
    }
}

/// Decides which failures reach the popup. Remembers each message for [`COALESCE`].
#[derive(Default)]
pub struct Coalescer {
    seen: Vec<Seen>,
}

struct Seen {
    message: String,
    first: Instant,
    shown: Instant,
    count: u32,
}

impl Coalescer {
    /// `Some(count)` when the popup should be shown (or refreshed) for `message` now: `1` the first
    /// time, more for repeats while the popup from the last showing is still up. `None` for a
    /// repeat that nobody would see, so a flapping device can't make a storm.
    pub fn admit(&mut self, now: Instant, level: AlertLevel, message: &str) -> Option<u32> {
        self.seen.retain(|s| now.duration_since(s.first) < COALESCE);
        if let Some(s) = self.seen.iter_mut().find(|s| s.message == message) {
            s.count += 1;
            if now.duration_since(s.shown) >= lifetime(level) {
                return None;
            }
            s.shown = now;
            return Some(s.count);
        }
        self.seen.push(Seen { message: message.to_owned(), first: now, shown: now, count: 1 });
        Some(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_stay_longer_than_warnings() {
        assert_eq!(lifetime(AlertLevel::Error), Duration::from_secs(8));
        assert_eq!(lifetime(AlertLevel::Warning), Duration::from_secs(5));
    }

    #[test]
    fn truncate_cuts_with_an_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        assert_eq!(truncate("exactly5", 8), "exactly5");
    }

    #[test]
    fn split_finds_title_and_detail() {
        let (t, d) = split(AlertLevel::Error, "Recording failed: the disk is full");
        assert_eq!((t.as_str(), d.as_str()), ("Recording failed", "the disk is full"));
        let (t, d) = split(AlertLevel::Warning, "The disk is slow. Clips are written gently.");
        assert_eq!((t.as_str(), d.as_str()), ("The disk is slow", "Clips are written gently."));
    }

    #[test]
    fn split_falls_back_to_a_generic_title_and_cuts_long_text() {
        let long = "x".repeat(200);
        let (t, d) = split(AlertLevel::Error, &long);
        assert_eq!(t, "Recording problem");
        assert_eq!(d.chars().count(), DETAIL_MAX);
        let (t, _) = split(AlertLevel::Warning, "no break here");
        assert_eq!(t, "ZeroClip warning");
    }

    #[test]
    fn repeats_are_counted_not_stormed() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        let e = AlertLevel::Error;
        assert_eq!(c.admit(t0, e, "Mic lost"), Some(1));
        // Popup still up: refreshed with a count.
        assert_eq!(c.admit(t0 + Duration::from_secs(2), e, "Mic lost"), Some(2));
        // Gone from screen, still inside 30 s: stays quiet.
        assert_eq!(c.admit(t0 + Duration::from_secs(15), e, "Mic lost"), None);
        // A different message is its own popup.
        assert_eq!(c.admit(t0 + Duration::from_secs(16), e, "Disk full"), Some(1));
        // After the window it is news again.
        assert_eq!(c.admit(t0 + Duration::from_secs(40), e, "Mic lost"), Some(1));
    }
}
