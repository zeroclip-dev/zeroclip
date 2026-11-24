//! Waveform peaks for the editor timeline (DESIGN §9, "Waveform").
//!
//! A background task decodes the audio (ffmpeg → raw f32 PCM on stdout), computes
//! one RMS value per bucket, and hands the array to the UI to draw. Results are
//! cached by file hash in `zc-store`, so reopening a clip is instant.

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
}
