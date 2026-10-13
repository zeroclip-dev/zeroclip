//! The recording spool: a hand-off from the hot path to the IO thread (CONTRIBUTING.md rule 2,
//! DESIGN §5).
//!
//! A recording's [`PacketWriter`] runs under the recorder's lock, on the encoder and
//! audio threads, so it may not touch the disk or allocate. [`SpoolWriter`] therefore
//! only copies each packet into a pre-allocated [`PacketRing`]. An IO thread runs
//! [`drain`], which pops the packets and writes them to the container. The two sides
//! share one mutex, held only for those copies and never across I/O.
//!
//! When the ring is full, the spool drops packets up to the next video keyframe and
//! then resumes. A recording never has a gap in the middle of a GOP: whatever comes
//! after an overflow starts on a keyframe, and everything before the gap is whole.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use zc_core::Hns;
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::ring::PacketRing;
use zc_core::sink::{PacketWriter, SinkError};
use zc_mux::container::ContainerWriter;

/// Byte budget of a recording spool: about 13 s of 20 Mbps video, enough to ride out a
/// disk stall.
const SPOOL_BYTES: usize = 32 << 20;

/// Shared state, behind one mutex.
struct State {
    ring: PacketRing,
    /// Set when the writer is dropped. The reader drains what is left, then stops.
    closed: bool,
    /// Set at the start and after an overflow. Packets are dropped until the next
    /// video keyframe, so a recording always resumes on a GOP boundary.
    skip_until_keyframe: bool,
}

struct Spool {
    state: Mutex<State>,
    /// Signalled when a packet arrives or the writer closes.
    ready: Condvar,
}

impl Spool {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The reader is the only other party, and a panic on it must not stop the hot
        // path, so recover the state instead of propagating the poison (as `Recorder` does).
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The hot-path end of a spool. Attach it to the recorder's [`Tee`](zc_core::sink::Tee).
/// Dropping it closes the spool, and the reader stops once it has drained the rest.
pub struct SpoolWriter {
    spool: Arc<Spool>,
}

impl PacketWriter for SpoolWriter {
    // Never allocates, never blocks on I/O, and holds the lock only for one copy.
    fn write(&mut self, packet: PacketRef<'_>) -> Result<(), SinkError> {
        let mut state = self.spool.lock();
        if state.skip_until_keyframe {
            if packet.meta.track != TrackId::Video || !packet.meta.keyframe {
                return Ok(());
            }
            state.skip_until_keyframe = false;
        }
        if !state.ring.push(packet) {
            // Only the first dropped packet is reported. The rest up to the next keyframe
            // are dropped silently, so one overflow is one error.
            state.skip_until_keyframe = true;
            return Err(SinkError::Full);
        }
        drop(state);
        self.spool.ready.notify_one();
        Ok(())
    }
}

impl Drop for SpoolWriter {
    fn drop(&mut self) {
        self.spool.lock().closed = true;
        self.spool.ready.notify_all();
    }
}

/// The IO-thread end of a spool. See [`drain`].
pub struct SpoolReader {
    spool: Arc<Spool>,
}

impl SpoolReader {
    /// Waits for the next packet, copies its bytes into `buf` and returns its metadata.
    /// Returns `None` once the writer is dropped and the spool is empty.
    ///
    /// The lock is held only for the copy, and released while waiting.
    pub fn next(&self, buf: &mut Vec<u8>) -> Option<PacketMeta> {
        let mut state = self.spool.lock();
        loop {
            if let Some(meta) = state.ring.pop_front_into(buf) {
                return Some(meta);
            }
            if state.closed {
                return None;
            }
            state = self.spool.ready.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Creates a recording spool with the default budget of [`SPOOL_BYTES`].
#[must_use]
pub fn spool() -> (SpoolWriter, SpoolReader) {
    spool_with_capacity(SPOOL_BYTES)
}

/// Creates a spool that holds `bytes` of packets. Tests use a small one to force an
/// overflow.
///
/// The ring has one packet slot per KiB, so the count and the byte budget run out
/// together. A 32 MiB spool holds 32,768 slots, far more than the roughly 2,000 packets
/// that 13 s of 60 fps video and two audio tracks need.
#[must_use]
pub fn spool_with_capacity(bytes: usize) -> (SpoolWriter, SpoolReader) {
    let spool = Arc::new(Spool {
        state: Mutex::new(State {
            ring: PacketRing::new(bytes, bytes / 1024),
            closed: false,
            skip_until_keyframe: true,
        }),
        ready: Condvar::new(),
    });
    (SpoolWriter { spool: Arc::clone(&spool) }, SpoolReader { spool })
}

/// Runs on the IO thread. Writes every packet from `reader` into `out` until the writer
/// is dropped, then finishes the file and renames `partial` to its final name.
///
/// `partial` must end in `.partial` (for example `clip.mp4.partial`). The final name
/// drops that suffix. If anything fails, the `.partial` file is left in place for the
/// startup repair (P3.22a).
///
/// The rename replaces whatever is at the final name (`std::fs::rename` does on Windows),
/// so `drain` refuses to rename over a file that has data in it. `output::create_recording`
/// reserves the final name with an empty placeholder, so the file this replaces is normally
/// that placeholder.
///
/// Returns the final path and the recording's duration, from the first video packet to
/// the end of the last one.
///
/// # Errors
/// I/O errors from `out`, from `finish` or from the rename. `InvalidInput` if `partial`
/// doesn't end in `.partial`. `AlreadyExists` if the final name holds a non-empty file.
pub fn drain(
    reader: SpoolReader,
    mut out: Box<dyn ContainerWriter>,
    partial: &Path,
) -> io::Result<(PathBuf, Hns)> {
    if partial.extension().is_none_or(|ext| ext != "partial") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a recording is written to a .partial file",
        ));
    }
    let mut buf = Vec::new();
    let mut first_video: Option<Hns> = None;
    let mut last_video_end = Hns::ZERO;
    while let Some(meta) = reader.next(&mut buf) {
        out.write_packet(PacketRef { meta, data: &buf })?;
        if meta.track == TrackId::Video {
            first_video.get_or_insert(meta.pts);
            last_video_end = meta.pts + meta.duration;
        }
    }
    out.finish()?;
    // The recording still plays without its length in the mehd, so a failure is only logged.
    if let Err(error) = zc_mux::repair::set_fragment_duration(partial) {
        tracing::warn!(error = %error, "could not set a recording's length");
    }
    let duration = first_video.map_or(Hns::ZERO, |first| last_video_end - first);
    let destination = final_path(partial);
    if std::fs::metadata(&destination).is_ok_and(|meta| meta.len() > 0) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", destination.display()),
        ));
    }
    std::fs::rename(partial, &destination)?;
    Ok((destination, duration))
}

/// The name a recording gets once it is finished: `partial` without its `.partial` suffix.
#[must_use]
pub(crate) fn final_path(partial: &Path) -> PathBuf {
    partial.with_extension("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zc_core::testing::SyntheticStream;

    /// What the fake container was given: video frame numbers in order, and whether
    /// `finish` ran.
    #[derive(Default)]
    struct Seen {
        frames: Vec<u32>,
        finished: bool,
    }

    /// A container that records what it is given. With `fail_after`, `write_packet`
    /// fails once that many packets have been written.
    struct FakeContainer {
        seen: Arc<Mutex<Seen>>,
        fail_after: Option<usize>,
    }

    impl ContainerWriter for FakeContainer {
        fn write_packet(&mut self, packet: PacketRef<'_>) -> io::Result<()> {
            let mut seen = self.seen.lock().expect("test lock");
            if self.fail_after == Some(seen.frames.len()) {
                return Err(io::Error::other("disk gone"));
            }
            seen.frames.push(SyntheticStream::sequence_number(packet.data));
            Ok(())
        }

        fn flush_fragment(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn finish(self: Box<Self>) -> io::Result<()> {
            self.seen.lock().expect("test lock").finished = true;
            Ok(())
        }
    }

    /// A fresh scratch directory for one test's `.partial` file.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zc-spool-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Pops one packet without blocking, as the IO thread would, and returns its frame number.
    fn pop_frame(reader: &SpoolReader) -> Option<u32> {
        let mut buf = Vec::new();
        reader.spool.lock().ring.pop_front_into(&mut buf)?;
        Some(SyntheticStream::sequence_number(&buf))
    }

    #[test]
    fn spool_starts_at_a_keyframe_and_drains_in_order() {
        let (mut writer, reader) = spool();
        let mut stream = SyntheticStream::new();
        for n in 0..130 {
            let packet = stream.next_video();
            // Frames 0..10 never reach the spool, so it starts mid-GOP at frame 10.
            if n >= 10 {
                writer.write(packet.view()).expect("the spool has room");
            }
        }
        drop(writer);

        let dir = scratch_dir("in-order");
        let partial = dir.join("clip.mp4.partial");
        std::fs::write(&partial, b"").expect("partial file");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let container = FakeContainer { seen: Arc::clone(&seen), fail_after: None };
        let (path, duration) = drain(reader, Box::new(container), &partial).expect("drain");

        let seen = seen.lock().expect("test lock");
        assert_eq!(seen.frames.first(), Some(&60), "starts at the first keyframe");
        assert_eq!(seen.frames, (60..130).collect::<Vec<_>>());
        assert!(seen.finished, "finish was called");
        assert_eq!(path, dir.join("clip.mp4"));
        assert!(path.exists() && !partial.exists(), "renamed on success");
        assert_eq!(duration, stream.frame_pts(130) - stream.frame_pts(60));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spool_skips_to_the_next_keyframe_when_full() {
        // Four 1,000-byte frames fit in 4 KiB, so the spool overflows on the fifth.
        let (mut writer, reader) = spool_with_capacity(4 << 10);
        let mut stream = SyntheticStream::new();
        let mut recorded = Vec::new();
        let mut rejected = 0;
        for n in 0..200 {
            // The IO thread pops one packet every 10 frames, so the encoder outruns it.
            if n % 10 == 0 {
                recorded.extend(pop_frame(&reader));
            }
            match writer.write(stream.next_video().view()) {
                Ok(()) => {}
                Err(SinkError::Full) => rejected += 1,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(rejected > 0, "the spool must overflow for this test to mean anything");

        drop(writer);
        let mut buf = Vec::new();
        while reader.next(&mut buf).is_some() {
            recorded.push(SyntheticStream::sequence_number(&buf));
        }

        assert_eq!(recorded[0], 0, "the recording starts on a keyframe");
        for pair in recorded.windows(2) {
            let (before, after) = (pair[0], pair[1]);
            // A gap is allowed only if it ends on a keyframe, so the file never resumes mid-GOP.
            assert!(
                after == before + 1 || after.is_multiple_of(60),
                "resumed mid-GOP: frame {before} then frame {after}"
            );
        }
    }

    #[test]
    fn spool_leaves_the_partial_file_when_the_container_fails() {
        let (mut writer, reader) = spool();
        let mut stream = SyntheticStream::new();
        for _ in 0..10 {
            writer.write(stream.next_video().view()).expect("the spool has room");
        }
        drop(writer);

        let dir = scratch_dir("failed");
        let partial = dir.join("clip.mp4.partial");
        std::fs::write(&partial, b"").expect("partial file");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let container = FakeContainer { seen: Arc::clone(&seen), fail_after: Some(3) };

        assert!(drain(reader, Box::new(container), &partial).is_err());
        assert!(partial.exists(), "the partial file stays for the startup repair");
        assert!(!dir.join("clip.mp4").exists());
        assert!(!seen.lock().expect("test lock").finished);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_never_replaces_a_file_with_data() {
        let (writer, reader) = spool();
        drop(writer);

        let dir = scratch_dir("no-replace");
        let partial = dir.join("clip.mp4.partial");
        std::fs::write(&partial, b"recording").expect("partial file");
        let finished = dir.join("clip.mp4");
        std::fs::write(&finished, b"an earlier clip").expect("finished clip");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let container = FakeContainer { seen: Arc::clone(&seen), fail_after: None };

        let err = drain(reader, Box::new(container), &partial).expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&finished).expect("finished clip"), b"an earlier clip");
        assert_eq!(std::fs::read(&partial).expect("partial file"), b"recording");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_replaces_an_empty_placeholder() {
        let (writer, reader) = spool();
        drop(writer);

        let dir = scratch_dir("placeholder");
        let partial = dir.join("clip.mp4.partial");
        std::fs::write(&partial, b"").expect("partial file");
        std::fs::write(dir.join("clip.mp4"), b"").expect("placeholder");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let container = FakeContainer { seen, fail_after: None };

        let (path, _) = drain(reader, Box::new(container), &partial).expect("drain");
        assert_eq!(path, dir.join("clip.mp4"));
        assert!(!partial.exists(), "renamed over the placeholder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
