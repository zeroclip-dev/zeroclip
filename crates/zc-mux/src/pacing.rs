//! Write pacing for spinning disks (DESIGN §4, "Pace DVR dumps on HDD").
//!
//! The HDD problem isn't throughput — 2.5 MB/s of capture is nothing — it's seek
//! contention with the game's own asset streaming. A 750 MB DVR dump written flat
//! out saturates the drive for ~7 s and the game stutters. On an HDD, dump at
//! 30–50 MB/s on a low-priority thread instead; on an SSD, write at full speed.

use std::io::{self, Write};
use std::thread;
use std::time::Instant;

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
        let Some(rate) = self.bytes_per_sec.filter(|&r| r > 0) else {
            return Hns::ZERO;
        };
        // `u128` so that even `u64::MAX` bytes cannot overflow. 10_000_000 is ticks per second.
        let due = u128::from(written) * 10_000_000 / u128::from(rate);
        Hns::from_ticks(i64::try_from(due).unwrap_or(i64::MAX)).saturating_sub(elapsed)
    }
}

/// A [`Write`] adapter that sleeps so that the bytes written through it stay under a
/// [`WritePacer`]'s limit.
///
/// The sleep blocks the calling thread, so use this only on an IO thread, never on the
/// capture, encode or audio threads. Wrap the file in it and put the
/// [`ChunkedWriter`](crate::chunked::ChunkedWriter) above it: each write it sees is then
/// one whole chunk, so the pacing happens per chunk.
#[derive(Debug)]
pub struct PacedWriter<W> {
    inner: W,
    pacer: WritePacer,
    written: u64,
    /// Set on the first write, so the clock starts when the dump does.
    started: Option<Instant>,
}

impl<W: Write> PacedWriter<W> {
    /// Wraps `inner`, pacing its writes with `pacer`.
    #[must_use]
    pub fn new(inner: W, pacer: WritePacer) -> Self {
        Self { inner, pacer, written: 0, started: None }
    }
}

impl<W: Write> Write for PacedWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let started = *self.started.get_or_insert_with(Instant::now);
        let n = self.inner.write(data)?;
        self.written = self.written.saturating_add(n as u64);
        if let Some(wait) =
            self.pacer.delay(self.written, Hns::from(started.elapsed())).to_duration()
            && !wait.is_zero()
        {
            thread::sleep(wait);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
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
    fn pacer_waits_when_ahead_of_schedule() {
        let p = WritePacer::limited(40 * MB);
        // 80 MB at 40 MB/s should take 2 s; only 1 s has passed.
        assert_eq!(p.delay(80 * MB, Hns::from_secs(1)), Hns::from_secs(1));
    }

    #[test]
    fn pacer_never_waits_when_on_or_behind_schedule() {
        let p = WritePacer::limited(40 * MB);
        assert_eq!(p.delay(80 * MB, Hns::from_secs(2)), Hns::ZERO);
        assert_eq!(p.delay(80 * MB, Hns::from_secs(5)), Hns::ZERO);
        assert_eq!(p.delay(0, Hns::ZERO), Hns::ZERO);
    }

    #[test]
    fn pacer_unlimited_never_waits() {
        assert_eq!(WritePacer::unlimited().delay(u64::MAX, Hns::ZERO), Hns::ZERO);
    }
}
