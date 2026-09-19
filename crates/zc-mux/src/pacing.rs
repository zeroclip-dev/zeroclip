//! Write pacing for spinning disks (DESIGN §4, "Pace DVR dumps on HDD").
//!
//! The HDD problem isn't throughput — 2.5 MB/s of capture is nothing — it's seek
//! contention with the game's own asset streaming. A 750 MB DVR dump written flat
//! out saturates the drive for ~7 s and the game stutters. On an HDD, dump at
//! 30–50 MB/s on a low-priority thread instead; on an SSD, write at full speed.

use zc_core::Hns;

/// What kind of drive a path lives on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DriveKind {
    /// No seek penalty (SSD/NVMe).
    Ssd,
    /// Seek penalty (spinning disk): pace writes.
    Hdd,
    /// Couldn't tell; treat as SSD.
    #[default]
    Unknown,
}

impl DriveKind {
    /// From the `IncursSeekPenalty` answer of `StorageDeviceSeekPenaltyProperty`.
    #[must_use]
    pub fn from_seek_penalty(incurs_seek_penalty: Option<bool>) -> Self {
        match incurs_seek_penalty {
            Some(true) => Self::Hdd,
            Some(false) => Self::Ssd,
            None => Self::Unknown,
        }
    }
}

/// A throughput limit for bulk writes. Pure arithmetic with injected time, so it
/// is deterministic to test; the IO thread sleeps for whatever it returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePacer {
    bytes_per_sec: Option<u64>,
}

impl WritePacer {
    /// Limits writes to `bytes_per_sec`.
    #[must_use]
    pub fn limited(bytes_per_sec: u64) -> Self {
        Self { bytes_per_sec: Some(bytes_per_sec) }
    }

    /// No limit (SSDs).
    #[must_use]
    pub fn unlimited() -> Self {
        Self { bytes_per_sec: None }
    }

    /// The right pacer for a drive.
    #[must_use]
    pub fn for_drive(kind: DriveKind, hdd_bytes_per_sec: u64) -> Self {
        match kind {
            DriveKind::Hdd => Self::limited(hdd_bytes_per_sec),
            DriveKind::Ssd | DriveKind::Unknown => Self::unlimited(),
        }
    }

    /// How long to wait before the next write, given that `written` bytes have
    /// gone out in the `elapsed` time since the dump started. Zero when on or behind schedule.
    #[must_use]
    pub fn delay(&self, written: u64, elapsed: Hns) -> Hns {
        let _ = (written, elapsed, self.bytes_per_sec);
        todo!("Phase 3: pacing arithmetic (see the `pacer_*` specs)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    #[test]
    fn drive_kind_from_seek_penalty() {
        assert_eq!(DriveKind::from_seek_penalty(Some(true)), DriveKind::Hdd);
        assert_eq!(DriveKind::from_seek_penalty(Some(false)), DriveKind::Ssd);
        assert_eq!(WritePacer::for_drive(DriveKind::Unknown, 40 * MB), WritePacer::unlimited());
    }

    #[test]
    #[ignore = "spec: WritePacer::delay (Phase 3)"]
    fn pacer_waits_when_ahead_of_schedule() {
        let p = WritePacer::limited(40 * MB);
        // 80 MB at 40 MB/s should take 2 s; only 1 s has passed.
        assert_eq!(p.delay(80 * MB, Hns::from_secs(1)), Hns::from_secs(1));
    }

    #[test]
    #[ignore = "spec: WritePacer::delay (Phase 3)"]
    fn pacer_never_waits_when_on_or_behind_schedule() {
        let p = WritePacer::limited(40 * MB);
        assert_eq!(p.delay(80 * MB, Hns::from_secs(2)), Hns::ZERO);
        assert_eq!(p.delay(80 * MB, Hns::from_secs(5)), Hns::ZERO);
        assert_eq!(p.delay(0, Hns::ZERO), Hns::ZERO);
    }

    #[test]
    #[ignore = "spec: WritePacer::delay (Phase 3)"]
    fn pacer_unlimited_never_waits() {
        assert_eq!(WritePacer::unlimited().delay(u64::MAX, Hns::ZERO), Hns::ZERO);
    }
}
