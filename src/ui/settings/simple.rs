//! Helpers for the Settings screen's Simple view: the "Clip the last N minutes" number and the
//! live line under it that says what the buffer needs. Pure and tested here; the view itself is in
//! `settings/mod.rs`.

use std::path::Path;

use zc_core::dvr::{SimpleCheck, simple_mode_check};

/// The stepper's limits, in minutes.
pub const MIN_MINUTES: f64 = 1.0;
pub const MAX_MINUTES: f64 = 240.0;

/// The buffer length in whole minutes, as the number shows it. Never below one minute.
pub fn minutes_from_buffer_secs(secs: u32) -> u32 {
    (secs / 60).max(1)
}

/// Bytes as gigabytes with one decimal, e.g. "1.4 GB". Rounds half up, in integers.
fn gigabytes(bytes: u64) -> String {
    let tenths = bytes.saturating_add(50_000_000) / 100_000_000;
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

/// The video and audio bitrates added together, in bits per second. Settings hold kbit/s.
pub fn buffer_bitrate_bps(video_kbps: u32, audio_kbps: u32) -> u64 {
    (u64::from(video_kbps) + u64::from(audio_kbps)) * 1_000
}

/// The live line under the number, and whether it is the warning colour.
#[derive(Debug, PartialEq, Eq)]
pub struct StorageLine {
    pub text: String,
    pub too_small: bool,
}

/// What `minutes` of footage needs at `bitrate_bps`, against `free` bytes on the drive. `None`
/// means the free space couldn't be read; the line then says the need and nothing more.
pub fn storage_line(minutes: u32, bitrate_bps: u64, free: Option<u64>) -> StorageLine {
    // With no free-space reading, ask for "enough" room so the check still reports the need.
    match simple_mode_check(minutes, bitrate_bps, free.unwrap_or(u64::MAX)) {
        SimpleCheck::InvalidMinutes => {
            StorageLine { text: "Choose at least one minute.".to_owned(), too_small: false }
        }
        SimpleCheck::TooLittleSpace { needed_bytes, free_bytes } => StorageLine {
            text: format!(
                "Not enough space: needs {}, only {} free",
                gigabytes(needed_bytes),
                gigabytes(free_bytes)
            ),
            too_small: true,
        },
        SimpleCheck::Ok { needed_bytes } => {
            let need = format!("{minutes} min needs about {}", gigabytes(needed_bytes));
            let text = match free {
                Some(free) => format!("{need}, {} free", gigabytes(free)),
                None => format!("{need}. Free space unknown."),
            };
            StorageLine { text, too_small: false }
        }
    }
}

/// Free bytes on the drive that will hold the clips. The output folder may not exist yet, so
/// the nearest folder that does is asked instead.
pub fn free_space(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    zc_win::drive::free_bytes(existing).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_seconds_become_whole_minutes() {
        assert_eq!(minutes_from_buffer_secs(1_200), 20);
        assert_eq!(minutes_from_buffer_secs(90), 1);
        assert_eq!(minutes_from_buffer_secs(30), 1);
        assert_eq!(minutes_from_buffer_secs(0), 1);
    }

    #[test]
    fn bytes_read_as_gigabytes_with_one_decimal() {
        assert_eq!(gigabytes(0), "0.0 GB");
        assert_eq!(gigabytes(1_400_000_000), "1.4 GB");
        assert_eq!(gigabytes(1_449_999_999), "1.4 GB");
        assert_eq!(gigabytes(1_450_000_000), "1.5 GB");
        assert_eq!(gigabytes(212_000_000_000), "212.0 GB");
    }

    #[test]
    fn bitrate_adds_video_and_audio_in_bits_per_second() {
        assert_eq!(buffer_bitrate_bps(20_000, 192), 20_192_000);
    }

    #[test]
    fn storage_line_says_what_fits() {
        // 20 min at 8 Mbps is 1.2 GB, plus the 10% margin: 1.32 GB.
        let line = storage_line(20, 8_000_000, Some(212_000_000_000));
        assert_eq!(line.text, "20 min needs about 1.3 GB, 212.0 GB free");
        assert!(!line.too_small);
    }

    #[test]
    fn storage_line_warns_when_the_drive_is_too_small() {
        let line = storage_line(20, 8_000_000, Some(1_000_000_000));
        assert_eq!(line.text, "Not enough space: needs 1.3 GB, only 1.0 GB free");
        assert!(line.too_small);
    }

    #[test]
    fn storage_line_without_a_free_space_reading_gives_the_need_only() {
        let line = storage_line(20, 8_000_000, None);
        assert_eq!(line.text, "20 min needs about 1.3 GB. Free space unknown.");
        assert!(!line.too_small);
    }

    #[test]
    fn zero_minutes_is_not_a_window() {
        let line = storage_line(0, 8_000_000, Some(1_000_000_000));
        assert_eq!(line.text, "Choose at least one minute.");
        assert!(!line.too_small);
    }
}
