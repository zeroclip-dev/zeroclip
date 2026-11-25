//! Bounded LRU cache for timeline thumbnails, keyed by `(source path, quantized timestamp)`.
//!
//! Quantizing timestamps to [`THUMB_QUANT_HNS`] buckets lets trimming and dragging reuse cached
//! frames instead of re-decoding with ffmpeg on slight shifts, while [`ThumbCache::nearest_for_path`]
//! keeps showing the closest already-loaded frame while new ones decode (no flash to blank).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use zc_core::Hns;

/// Default maximum number of thumbnails held in memory at once.
pub const DEFAULT_THUMB_CACHE_CAPACITY: usize = 200;

/// Quantization step for thumbnail timestamps (`250 ms`). Nearby requests within the same
/// quarter-second bucket share one cached frame.
pub const THUMB_QUANT_HNS: Hns = Hns::from_millis(250);

/// Quantizes `at` to the nearest [`THUMB_QUANT_HNS`] bucket index (`>= 0`).
#[must_use]
pub fn quantize_timestamp(at: Hns) -> i64 {
    let ticks = at.ticks().max(0);
    let step = THUMB_QUANT_HNS.ticks();
    (ticks + step / 2) / step
}

/// Returns the representative [`Hns`] timestamp for the quantized bucket of `at`.
#[cfg(test)]
#[must_use]
pub fn quantized_hns(at: Hns) -> Hns {
    Hns::from_ticks(quantize_timestamp(at) * THUMB_QUANT_HNS.ticks())
}

/// Cache key identifying a single timeline thumbnail frame.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThumbKey {
    /// Path to the source video file.
    pub path: PathBuf,
    /// Quantized timestamp bucket (from [`quantize_timestamp`]).
    pub bucket: i64,
}

impl ThumbKey {
    /// Creates a key for `path` at timestamp `at` (quantized via [`quantize_timestamp`]).
    #[must_use]
    pub fn new(path: PathBuf, at: Hns) -> Self {
        Self { path, bucket: quantize_timestamp(at) }
    }

    /// The representative source timestamp corresponding to this key's bucket.
    #[must_use]
    pub fn timestamp(&self) -> Hns {
        Hns::from_ticks(self.bucket * THUMB_QUANT_HNS.ticks())
    }
}

/// A bounded least-recently-used (LRU) thumbnail cache.
///
/// Generic over `V` so the eviction and lookup logic stays pure and unit-testable without GPUI.
/// When an entry is evicted, it is returned to the caller so GPU textures can be released.
pub struct ThumbCache<V> {
    capacity: usize,
    clock: u64,
    entries: HashMap<ThumbKey, (u64, V)>,
}

impl<V> Default for ThumbCache<V> {
    fn default() -> Self {
        Self::new(DEFAULT_THUMB_CACHE_CAPACITY)
    }
}

impl<V> ThumbCache<V> {
    /// Creates a cache bounded to `capacity` entries (at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self { capacity, clock: 0, entries: HashMap::with_capacity(capacity) }
    }

    /// Number of entries currently stored.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether `key` is currently cached without updating recency.
    #[cfg(test)]
    #[must_use]
    pub fn contains(&self, key: &ThumbKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Looks up `key` and marks it most-recently-used on a hit.
    pub fn get(&mut self, key: &ThumbKey) -> Option<&V> {
        let stamp = self.next_stamp();
        let entry = self.entries.get_mut(key)?;
        entry.0 = stamp;
        Some(&entry.1)
    }

    /// Inserts `value` under `key`, marking it most-recently-used.
    ///
    /// Returns the evicted least-recently-used value (or the replaced previous value for `key`)
    /// so callers can dispose GPU resources cleanly.
    pub fn insert(&mut self, key: ThumbKey, value: V) -> Option<V> {
        let stamp = self.next_stamp();
        if let Some(existing) = self.entries.get_mut(&key) {
            existing.0 = stamp;
            return Some(std::mem::replace(&mut existing.1, value));
        }
        let evicted = if self.entries.len() >= self.capacity {
            let lru_key = self
                .entries
                .iter()
                .min_by_key(|(_, (used_at, _))| *used_at)
                .map(|(k, _)| k.clone());
            lru_key.and_then(|k| self.entries.remove(&k).map(|(_, v)| v))
        } else {
            None
        };
        self.entries.insert(key, (stamp, value));
        evicted
    }

    /// Returns the exact cached value for `(path, target)` if present, otherwise the cached frame
    /// for the same `path` whose bucket is closest to `target`. Returns `None` only when nothing
    /// for `path` is cached yet.
    ///
    /// Keeping the nearest cached frame visible prevents flashing to blank while new thumbnails
    /// decode during trimming or dragging.
    #[must_use]
    pub fn nearest_for_path(&self, path: &Path, target: Hns) -> Option<&V> {
        let target_bucket = quantize_timestamp(target);
        // Fast path: check exact quantized bucket first.
        let mut best: Option<(i64, &V)> = None;
        for (key, (_, value)) in &self.entries {
            if key.path != path {
                continue;
            }
            let dist = (key.bucket - target_bucket).abs();
            if dist == 0 {
                return Some(value);
            }
            if best.is_none_or(|(best_dist, _)| dist < best_dist) {
                best = Some((dist, value));
            }
        }
        best.map(|(_, v)| v)
    }

    fn next_stamp(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_quantization_groups_nearby_timestamps_and_separates_files() {
        let clip_a = PathBuf::from("a.mp4");
        let clip_b = PathBuf::from("b.mp4");

        // Negative clamps to bucket 0.
        assert_eq!(quantize_timestamp(Hns::from_millis(-500)), 0);
        // 250 ms step: [0, 124 ms] -> 0, [125 ms, 374 ms] -> 1, [375 ms, 624 ms] -> 2.
        assert_eq!(quantize_timestamp(Hns::from_millis(0)), 0);
        assert_eq!(quantize_timestamp(Hns::from_millis(120)), 0);
        assert_eq!(quantize_timestamp(Hns::from_millis(130)), 1);
        assert_eq!(quantize_timestamp(Hns::from_millis(250)), 1);
        assert_eq!(quantize_timestamp(Hns::from_millis(370)), 1);
        assert_eq!(quantize_timestamp(Hns::from_millis(380)), 2);

        // Nearby timestamps produce equal keys; different files do not.
        assert_eq!(
            ThumbKey::new(clip_a.clone(), Hns::from_millis(240)),
            ThumbKey::new(clip_a.clone(), Hns::from_millis(260))
        );
        assert_ne!(
            ThumbKey::new(clip_a.clone(), Hns::from_millis(250)),
            ThumbKey::new(clip_b, Hns::from_millis(250))
        );
        assert_eq!(quantized_hns(Hns::from_millis(260)), Hns::from_millis(250));
    }

    #[test]
    fn cache_hit_returns_stored_value_and_promotes_recency() {
        let mut cache = ThumbCache::new(2);
        let p = PathBuf::from("clip.mp4");
        let k0 = ThumbKey::new(p.clone(), Hns::from_millis(0));
        let k1 = ThumbKey::new(p.clone(), Hns::from_millis(500));
        let k2 = ThumbKey::new(p, Hns::from_millis(1_000));

        assert_eq!(cache.insert(k0.clone(), "frame-0"), None);
        assert_eq!(cache.insert(k1.clone(), "frame-1"), None);
        // Accessing k0 promotes it to MRU, so k1 becomes the LRU victim.
        assert_eq!(cache.get(&k0), Some(&"frame-0"));
        assert_eq!(cache.insert(k2.clone(), "frame-2"), Some("frame-1"));

        assert_eq!(cache.len(), 2);
        assert!(cache.contains(&k0));
        assert!(!cache.contains(&k1));
        assert!(cache.contains(&k2));
    }

    #[test]
    fn cache_evicts_least_recently_used_at_capacity() {
        let mut cache = ThumbCache::new(3);
        let p = PathBuf::from("clip.mp4");
        for i in 0..3 {
            let key = ThumbKey::new(p.clone(), Hns::from_secs(i));
            assert_eq!(cache.insert(key, i), None);
        }
        assert_eq!(cache.len(), 3);
        // Inserting a 4th evicts the oldest (0 s).
        let evicted = cache.insert(ThumbKey::new(p.clone(), Hns::from_secs(3)), 3);
        assert_eq!(evicted, Some(0));
        assert_eq!(cache.len(), 3);
        assert!(!cache.contains(&ThumbKey::new(p.clone(), Hns::from_secs(0))));
        assert!(cache.contains(&ThumbKey::new(p, Hns::from_secs(3))));
    }

    #[test]
    fn nearest_for_path_falls_back_to_closest_frame_without_crossing_files() {
        let mut cache = ThumbCache::new(10);
        let clip_a = PathBuf::from("a.mp4");
        let clip_b = PathBuf::from("b.mp4");
        cache.insert(ThumbKey::new(clip_a.clone(), Hns::from_secs(1)), "a-1s");
        cache.insert(ThumbKey::new(clip_a.clone(), Hns::from_secs(5)), "a-5s");
        cache.insert(ThumbKey::new(clip_b.clone(), Hns::from_secs(2)), "b-2s");

        // Exact hit
        assert_eq!(cache.nearest_for_path(&clip_a, Hns::from_secs(1)), Some(&"a-1s"));
        // Missing 2 s falls back to nearest frame in clip_a (1 s, not 5 s, and never clip_b's 2 s)
        assert_eq!(cache.nearest_for_path(&clip_a, Hns::from_secs(2)), Some(&"a-1s"));
        assert_eq!(cache.nearest_for_path(&clip_a, Hns::from_secs(4)), Some(&"a-5s"));
        // Uncached file returns None
        assert_eq!(cache.nearest_for_path(Path::new("other.mp4"), Hns::from_secs(1)), None);
    }
}
