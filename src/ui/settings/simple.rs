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

/// One quality choice on the Simple view: the frame rate and bitrate range it sets. The numbers
/// are the Advanced ones, so picking a preset and then opening Advanced shows the same values.
#[derive(Debug, PartialEq, Eq)]
pub struct Quality {
    pub label: &'static str,
    pub fps: u32,
    pub min_kbps: u32,
    pub max_kbps: u32,
}

/// Low to Ultra. High is what a fresh install uses.
pub const QUALITIES: [Quality; 4] = [
    Quality { label: "Low", fps: 30, min_kbps: 2_500, max_kbps: 8_000 },
    Quality { label: "Medium", fps: 60, min_kbps: 3_500, max_kbps: 12_000 },
    Quality { label: "High", fps: 60, min_kbps: 5_000, max_kbps: 20_000 },
    Quality { label: "Ultra", fps: 60, min_kbps: 8_000, max_kbps: 35_000 },
];

/// The preset the current settings match exactly, or `None` for custom values from Advanced.
pub fn current_quality(fps: u32, min_kbps: u32, max_kbps: u32) -> Option<usize> {
    QUALITIES.iter().position(|q| (q.fps, q.min_kbps, q.max_kbps) == (fps, min_kbps, max_kbps))
}

/// "60 fps, up to 20 Mbps", the line under the choice.
pub fn quality_line(q: &Quality) -> String {
    format!(
        "{} fps, up to {} Mbps. Higher quality uses more space and memory.",
        q.fps,
        q.max_kbps / 1_000
    )
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
/// the nearest folder that does is asked instead. Called while drawing, so the answer is kept
/// for a few seconds instead of asking the disk on every frame.
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST: Mutex<Option<(std::path::PathBuf, Instant, Option<u64>)>> = Mutex::new(None);
    const KEEP: Duration = Duration::from_secs(5);
    if let Ok(last) = LAST.lock()
        && let Some((path, at, free)) = last.as_ref()
        && path == dir
        && at.elapsed() < KEEP
    {
        return *free;
    }
    let free = dir
        .ancestors()
        .find(|p| p.exists())
        .and_then(|existing| zc_win::drive::free_bytes(existing).ok());
    if let Ok(mut last) = LAST.lock() {
        *last = Some((dir.to_path_buf(), Instant::now(), free));
    }
    free
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_install_is_high_quality() {
        let v = zc_core::settings::VideoSettings::default();
        assert_eq!(current_quality(v.fps, v.min_bitrate_kbps, v.max_bitrate_kbps), Some(2));
    }

    #[test]
    fn custom_values_match_no_preset() {
        assert_eq!(current_quality(144, 5_000, 20_000), None);
        assert_eq!(current_quality(60, 5_000, 20_001), None);
    }

    #[test]
    fn presets_get_bigger_each_step() {
        assert!(QUALITIES.windows(2).all(|w| w[0].max_kbps < w[1].max_kbps));
    }

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
