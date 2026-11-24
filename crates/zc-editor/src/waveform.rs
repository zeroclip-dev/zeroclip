//! Waveform peaks for the editor timeline (DESIGN §9, "Waveform").
//!
//! A background task decodes the audio (ffmpeg → raw f32 PCM on stdout), computes
//! one RMS value per bucket, and hands the array to the UI to draw. Results are
//! cached by file hash in `zc-store`, so reopening a clip is instant.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// How much of each end of a file goes into its key.
const KEY_SPAN: u64 = 64 * 1024;

/// A cache key for a file: its length plus a hash of its first and last 64 KiB.
///
/// std's `DefaultHasher` isn't stable across Rust releases, so this uses FNV-1a, which is.
/// Reading 128 KiB beats hashing a 750 MB file.
/// ponytail: same size + head + tail collide; acceptable for a cache, a full hash if not.
///
/// # Errors
/// Any I/O error opening or reading `path`.
pub fn file_key(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut hash = fnv1a(FNV_OFFSET, &len.to_le_bytes());
    let mut span = Vec::new();
    (&mut file).take(KEY_SPAN).read_to_end(&mut span)?;
    hash = fnv1a(hash, &span);
    file.seek(SeekFrom::Start(len.saturating_sub(KEY_SPAN)))?;
    span.clear();
    (&mut file).take(KEY_SPAN).read_to_end(&mut span)?;
    hash = fnv1a(hash, &span);
    Ok(format!("{len:x}-{hash:016x}"))
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Splits `samples` into `buckets` equal spans and returns the RMS of each.
/// Returns an empty vector when there are no samples or no buckets.
#[must_use]
pub fn rms_peaks(samples: &[f32], buckets: usize) -> Vec<f32> {
    if samples.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let len = samples.len();
    (0..buckets)
        .map(|b| {
            // Bucket `b` covers `[b·len/buckets, (b+1)·len/buckets)`: spans differ by at most
            // one sample and cover everything. Only empty when there are more buckets than samples.
            let span = &samples[b * len / buckets..(b + 1) * len / buckets];
            if span.is_empty() {
                return 0.0;
            }
            // f64 accumulation, so long buckets don't lose precision.
            let sum: f64 = span.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
            (sum / span.len() as f64).sqrt() as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_rms_of_a_constant_is_its_magnitude() {
        let peaks = rms_peaks(&[-0.5; 1_000], 10);
        assert_eq!(peaks.len(), 10);
        assert!(peaks.iter().all(|&p| (p - 0.5).abs() < 1e-4));
    }

    #[test]
    fn waveform_separates_loud_from_quiet() {
        let mut samples = vec![0.0_f32; 500];
        samples.extend([0.8_f32; 500]);
        let peaks = rms_peaks(&samples, 2);
        assert!(peaks[0].abs() < 1e-4);
        assert!((peaks[1] - 0.8).abs() < 1e-4, "{peaks:?}");
    }

    #[test]
    fn waveform_handles_empty_input() {
        assert!(rms_peaks(&[], 10).is_empty());
        assert!(rms_peaks(&[0.1, 0.2], 0).is_empty());
    }

    /// Writes `bytes` to a uniquely named temp file and returns its path.
    fn temp_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("zc-waveform-{tag}-{}.bin", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn file_key_is_stable_for_equal_files_and_changes_with_the_tail() {
        // Bigger than both spans, so the head and tail reads are separate.
        let a: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut changed = a.clone();
        *changed.last_mut().unwrap() ^= 0xff;
        let p1 = temp_file("same-1", &a);
        let p2 = temp_file("same-2", &a);
        let p3 = temp_file("tail", &changed);
        let k1 = file_key(&p1).unwrap();
        assert_eq!(k1, file_key(&p2).unwrap());
        assert_ne!(k1, file_key(&p3).unwrap());
        assert!(k1.starts_with(&format!("{:x}-", a.len())));
        for p in [p1, p2, p3] {
            let _ = std::fs::remove_file(p);
        }
    }
}
