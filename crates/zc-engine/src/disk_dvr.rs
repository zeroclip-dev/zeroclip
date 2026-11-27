//! The disk-backed DVR buffer.
//!
//! In disk mode the recorder's [`DvrBuffer`](zc_core::dvr::DvrBuffer) is only a small staging
//! area (about [`STAGING_BYTES`]): the hot path pushes into it exactly as in RAM mode and never
//! touches the disk (CONTRIBUTING.md rule 2). A [`Persister`] thread tails it. Once a GOP is
//! finished, and the audio that goes with it has had `PERSIST_LAG` to arrive, the thread copies
//! it out in short lock holds and writes it to the [`DiskRing`] as one record. Staging keeps
//! its copy until it evicts it as it would in RAM mode, so a snapshot never sees a hole between
//! "written" and "still staged".
//!
//! # The ring file
//!
//! `zeroclip-dvr.ring` is created at its full size and overwritten in place, never grown.
//! Records are appended at a write cursor. A record never straddles the end of the file: when
//! the next one doesn't fit, the cursor goes back to 0 and the tail stays unused. Each record is
//!
//! ```text
//! header   magic u32 | packets u32 | seq u64 | payload_len u64       (24 bytes, little endian)
//! table    per packet: track u8 | keyframe u8 | pts i64 | duration i64 | len u32   (22 bytes)
//! payload  the packets' bytes, in table order
//! ```
//!
//! A record is one GOP's video plus the audio from the same time span, so evicting a record
//! (whole GOPs, oldest first) takes its audio with it. The index of records (offset, length,
//! per-packet metadata) lives in RAM and is small. The table in the file is for crash rescue to
//! read the ring later.
//!
//! The file is sized once, at start, for the largest window among the profiles. A profile switch
//! never resizes it. Only a settings change that asks for a larger window grows it, and the
//! records already in it stay valid.
//!
//! # Reading while the writer wraps
//!
//! A clip is read on the clip IO thread, never under the recorder lock. The writer removes a
//! record from the index (under the index lock) *before* it writes over its bytes. So after
//! reading a record, the reader asks the index whether it is still there: if yes, no write can
//! have touched it while it was read; if not, it may be torn and is dropped. The header's
//! sequence number is checked too, so a stale index can't read another record's bytes.
//!
//! # Saving without the RAM
//!
//! A `DiskClip` never holds the ring part of a clip. It copies the staged part first (at most
//! the staging area, so a slow disk can't make staging evict it), then reads the ring records one
//! at a time into one reused buffer and hands their packets to the file writer, then the staged
//! packets. Peak RAM is about the staging area, one record and the writer's chunk, whatever the
//! window is. Records come in time order and a record's audio stops at the next GOP start, so
//! each track stays in order without sorting.
//!
//! If a record is overwritten after packets were written, the clip has a hole: the writer reports
//! `Lapped` and the caller starts over from a fresh plan.
// TODO: teach crash rescue to read the ring file after a crash. The records are
// self-describing, but nothing reads them yet. `DiskRing::create` truncates an old ring, so
// rescue-after-restart has to read the file before the engine creates the ring.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use zc_core::dvr::{DvrClip, DvrError};
use zc_core::media::{PacketMeta, PacketRef, TrackId};
use zc_core::time::Hns;
use zc_mux::pacing::{PacedWriter, WritePacer};

use crate::recorder::{GopCopy, Recorder};

/// RAM kept for staging, like the recording spool's budget. About 13 s of 20 Mbps video: the GOP
/// being written, the finished ones waiting for the persister, and room to spare when the disk
/// stalls.
pub const STAGING_BYTES: usize = 32 << 20;

/// The file name of the ring inside the ring folder.
pub const RING_FILE_NAME: &str = "zeroclip-dvr.ring";

/// How long a finished GOP waits before it is written, so late audio (the Master mix trails real
/// time) lands in the same record as its video.
const PERSIST_LAG: Hns = Hns::from_secs(1);

/// How often the persister looks for a finished GOP.
const POLL: Duration = Duration::from_millis(250);

const MAGIC: u32 = u32::from_le_bytes(*b"ZCR1");
const HEADER_LEN: usize = 24;
const TABLE_ENTRY_LEN: usize = 22;

/// One GOP on disk: where it is and what is in it. Kept in RAM.
#[derive(Debug)]
struct Record {
    seq: u64,
    /// File offset of the header.
    at: u64,
    /// Bytes from the header to the end of the payload.
    len: u64,
    /// The GOP's keyframe PTS.
    start: Hns,
    /// Metadata and payload length of each packet, in table order.
    packets: Box<[(PacketMeta, u32)]>,
}

impl Record {
    /// Where the payload starts, counted from `at`.
    fn payload_at(&self) -> usize {
        HEADER_LEN + self.packets.len() * TABLE_ENTRY_LEN
    }
}

struct Index {
    capacity: u64,
    /// Where the next record goes.
    write_at: u64,
    next_seq: u64,
    /// Bumped by `clear`. Work started in an older epoch is thrown away.
    epoch: u64,
    /// Oldest first. Records leave only from the front (see `append`), so "still here" is
    /// `front.seq <= seq`.
    records: VecDeque<Arc<Record>>,
}

/// The ring file and the index of what is in it.
#[derive(Debug)]
pub struct DiskRing {
    file: File,
    path: PathBuf,
    index: Mutex<Index>,
    write_errors: AtomicU64,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").field("records", &self.records.len()).finish_non_exhaustive()
    }
}

/// What a clip will read from the ring, chosen under the recorder lock.
#[derive(Debug)]
pub struct DiskPlan {
    epoch: u64,
    /// The oldest staged keyframe and the start the clip aims for, to choose again after a lap.
    staged_from: Hns,
    target: Hns,
    records: Vec<Arc<Record>>,
}

impl DiskPlan {
    /// Whether the ring has nothing to add to the staged part.
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl DiskRing {
    /// Creates `<dir>/zeroclip-dvr.ring` at `capacity` bytes, replacing an old one. The file is
    /// sized now, so a full disk fails here and not in the middle of a game.
    ///
    /// # Errors
    /// Whatever creating or sizing the file fails with.
    pub fn create(dir: &Path, capacity: u64) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(RING_FILE_NAME);
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
        file.set_len(capacity)?;
        let index =
            Index { capacity, write_at: 0, next_seq: 0, epoch: 0, records: VecDeque::new() };
        Ok(Self { file, path, index: Mutex::new(index), write_errors: AtomicU64::new(0) })
    }

    /// Where the file is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's size in bytes.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.index().capacity
    }

    /// Writes that failed, for the engine to warn about.
    #[must_use]
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    fn index(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.index().epoch
    }

    /// The keyframe PTS of the oldest record, if the ring holds any.
    pub(crate) fn oldest_start(&self) -> Option<Hns> {
        self.index().records.front().map(|r| r.start)
    }

    /// Forgets every record. The next ones start at the beginning of the file.
    pub fn clear(&self) {
        let mut index = self.index();
        index.records.clear();
        index.write_at = 0;
        index.epoch += 1;
    }

    /// Makes the file at least `capacity` bytes. Never shrinks it. The records stay valid: the
    /// cursor uses the new room once it reaches the old end, evicting as before until then. A
    /// write in progress stays inside the old size, so this needs no lock against the persister.
    /// Returns whether the file grew.
    ///
    /// The actor calls it when a settings change asks for a larger window than the file holds.
    ///
    /// # Errors
    /// Whatever sizing the file fails with. The old size stays in force then.
    pub fn grow(&self, capacity: u64) -> io::Result<bool> {
        if capacity <= self.capacity() {
            return Ok(false);
        }
        self.file.set_len(capacity)?;
        self.index().capacity = capacity;
        Ok(true)
    }

    /// Picks the records a clip needs from before the staging area: those that start before
    /// `staged_from`, back to the newest one that starts at or before `target`. Fewer when the
    /// ring doesn't reach back that far.
    pub(crate) fn plan(&self, staged_from: Hns, target: Hns) -> DiskPlan {
        let index = self.index();
        let mut records = Vec::new();
        for record in index.records.iter().rev().filter(|r| r.start < staged_from) {
            records.push(Arc::clone(record));
            if record.start <= target {
                break;
            }
        }
        records.reverse();
        DiskPlan { epoch: index.epoch, staged_from, target, records }
    }

    /// Whether the record `seq` has not been evicted since `epoch`.
    fn is_live(&self, epoch: u64, seq: u64) -> bool {
        let index = self.index();
        index.epoch == epoch && index.records.front().is_some_and(|front| front.seq <= seq)
    }

    fn read_record(&self, record: &Record, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.resize(usize::try_from(record.len).map_err(io::Error::other)?, 0);
        read_exact_at(&self.file, buf, record.at)?;
        let magic = u32::from_le_bytes(buf[0..4].try_into().map_err(io::Error::other)?);
        let seq = u64::from_le_bytes(buf[8..16].try_into().map_err(io::Error::other)?);
        if magic != MAGIC || seq != record.seq {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ring record header mismatch"));
        }
        Ok(())
    }

    /// Writes `clip` (one GOP, ending at `end`) as the newest record, if the ring is still in
    /// `epoch`. `buf` is scratch space for encoding. Runs on the persister thread.
    fn append(
        &self,
        epoch: u64,
        clip: &DvrClip,
        buf: &mut Vec<u8>,
        pacer: WritePacer,
    ) -> io::Result<()> {
        let Some(start) = clip.video().next().map(|p| p.meta.pts) else { return Ok(()) };
        let packets: Box<[(PacketMeta, u32)]> =
            clip.packets.iter().map(|p| (p.meta, p.data.len() as u32)).collect();
        encode(buf, clip);
        let len = buf.len() as u64;

        let (seq, at) = {
            let mut index = self.index();
            if index.epoch != epoch {
                return Ok(());
            }
            if len > index.capacity {
                return Err(io::Error::other("a GOP is larger than the ring file"));
            }
            let at = if index.write_at + len > index.capacity { 0 } else { index.write_at };
            // Evict before writing, so a reader that finds its record still here knows no write
            // has touched it. Records have different sizes, so after a wrap the oldest one (the
            // tail of the file) may lie clear of the new record while a newer one at the head of
            // the file is in its way. Drop everything up to the newest record in the way: records
            // only leave from the front.
            let in_the_way = |r: &Arc<Record>| r.at < at + len && at < r.at + r.len;
            if let Some(newest) = index.records.iter().rposition(in_the_way) {
                index.records.drain(..=newest);
            }
            index.write_at = at + len;
            let seq = index.next_seq;
            index.next_seq += 1;
            (seq, at)
        };
        buf[8..16].copy_from_slice(&seq.to_le_bytes());
        PacedWriter::new(FileAt { file: &self.file, pos: at }, pacer).write_all(buf)?;

        let mut index = self.index();
        if index.epoch == epoch {
            index.records.push_back(Arc::new(Record { seq, at, len, start, packets }));
        }
        Ok(())
    }
}

/// A record was overwritten after packets of the clip were already written, so the clip would
/// have a hole. Carried inside an [`io::Error`]; see [`is_lapped`].
#[derive(Debug, thiserror::Error)]
#[error("the DVR ring file was overwritten while the clip was being saved")]
pub(crate) struct Lapped;

/// Whether `error` is [`Lapped`].
pub(crate) fn is_lapped(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Lapped>())
}

/// A clip whose newest part is copied into RAM and whose older part is still in the ring file.
/// Made by [`Recorder::copy_plan`]; written by [`stream`](Self::stream), which reads the ring
/// one record at a time (see the module docs).
pub(crate) struct DiskClip {
    staged: DvrClip,
    ring: Arc<DiskRing>,
    plan: DiskPlan,
    /// Where the clip ends, if it isn't at the newest frame.
    until: Option<Hns>,
}

impl DiskClip {
    pub(crate) fn new(
        staged: DvrClip,
        ring: Arc<DiskRing>,
        plan: DiskPlan,
        until: Option<Hns>,
    ) -> Self {
        Self { staged, ring, plan, until }
    }

    /// Chooses the ring records again, after a [`Lapped`] stream. The staged part stays.
    pub(crate) fn replan(&mut self) {
        self.plan = self.ring.plan(self.plan.staged_from, self.plan.target);
    }

    /// Hands every packet to `out`, oldest first: the ring's records, then the staged packets.
    /// Returns the clip's length, like [`DvrClip::duration`].
    ///
    /// A record the writer overwrote before anything was written is skipped, together with the
    /// ones before it, so the clip starts at the keyframe of the next intact record. After that it
    /// is a hole, and the error is [`Lapped`].
    ///
    /// # Errors
    /// `out`'s errors, a failed read of a record that is still in the ring, or [`Lapped`].
    pub(crate) fn stream(
        &self,
        out: &mut dyn FnMut(PacketRef<'_>) -> io::Result<()>,
    ) -> io::Result<Hns> {
        let mut span = Span::default();
        // One buffer for every record: the biggest GOP, not the window.
        let mut buf = Vec::new();
        let mut written = false;
        for record in &self.plan.records {
            let read = self.ring.read_record(record, &mut buf);
            // After the read, never before: see the module docs.
            let live = self.ring.is_live(self.plan.epoch, record.seq);
            match (read, live) {
                (Ok(()), true) => {
                    let mut at = record.payload_at();
                    for &(meta, len) in &*record.packets {
                        let data = &buf[at..at + len as usize];
                        at += len as usize;
                        if self.until.is_none_or(|end| meta.pts < end) {
                            span.add(meta);
                            out(PacketRef { meta, data })?;
                            written = true;
                        }
                    }
                }
                (Err(error), true) => return Err(error),
                (_, false) if written => return Err(io::Error::other(Lapped)),
                (_, false) => {
                    tracing::debug!(seq = record.seq, "DVR ring record overwritten before a read");
                }
            }
        }
        if let Some(first) = self.staged.video().next()
            && let Some(ring_end) = span.video_end
            && first.meta.pts - ring_end > first.meta.duration
        {
            // The persister fell behind and staging evicted a GOP it hadn't written.
            tracing::warn!(gap = ?(first.meta.pts - ring_end), "DVR clip jumps in time between the ring and staging");
        }
        for packet in &self.staged.packets {
            span.add(packet.meta);
            out(packet.view())?;
        }
        Ok(span.duration())
    }

    /// The whole clip in RAM. Only for tests and [`Recorder::snapshot`]: a save streams instead.
    pub(crate) fn collect(&self) -> Result<DvrClip, DvrError> {
        let mut packets = Vec::new();
        self.stream(&mut |p| {
            packets.push(p.to_packet());
            Ok(())
        })
        .map_err(|e| {
            if is_lapped(&e) { DvrError::Changed } else { DvrError::Disk(e.to_string()) }
        })?;
        Ok(DvrClip { packets })
    }
}

/// What a stream has handed out so far: for the length, and to check the order.
struct Span {
    last_pts: [Hns; TrackId::COUNT],
    first_video: Option<Hns>,
    video_end: Option<Hns>,
}

impl Default for Span {
    fn default() -> Self {
        Self { last_pts: [Hns(i64::MIN); TrackId::COUNT], first_video: None, video_end: None }
    }
}

impl Span {
    fn add(&mut self, meta: PacketMeta) {
        let last = &mut self.last_pts[meta.track.index()];
        // Records are in time order and a record's packets stop at the next GOP, so there is no
        // global sort. This is what makes that safe.
        debug_assert!(meta.pts >= *last, "{:?} went back in time", meta.track);
        *last = meta.pts;
        if meta.track == TrackId::Video {
            self.first_video.get_or_insert(meta.pts);
            self.video_end = Some(meta.end());
        }
    }

    fn duration(&self) -> Hns {
        match (self.first_video, self.video_end) {
            (Some(first), Some(end)) => end - first,
            _ => Hns::ZERO,
        }
    }
}

/// Encodes `clip` as a record into `buf`. The sequence number is patched in by the caller.
fn encode(buf: &mut Vec<u8>, clip: &DvrClip) {
    buf.clear();
    let payload: u64 = clip.packets.iter().map(|p| p.data.len() as u64).sum();
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&(clip.packets.len() as u32).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&payload.to_le_bytes());
    for p in &clip.packets {
        buf.push(p.meta.track.index() as u8);
        buf.push(u8::from(p.meta.keyframe));
        buf.extend_from_slice(&p.meta.pts.ticks().to_le_bytes());
        buf.extend_from_slice(&p.meta.duration.ticks().to_le_bytes());
        buf.extend_from_slice(&(p.data.len() as u32).to_le_bytes());
    }
    for p in &clip.packets {
        buf.extend_from_slice(&p.data);
    }
}

/// A [`Write`] at a fixed place in a file, so [`PacedWriter`] can pace a positioned write.
struct FileAt<'a> {
    file: &'a File,
    pos: u64,
}

impl Write for FileAt<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let written = write_at(self.file, data, self.pos)?;
        self.pos += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Positioned I/O leaves the file cursor alone, so the writer and a clip's reader can share one
// handle without a lock around the I/O.
#[cfg(windows)]
fn write_at(file: &File, data: &[u8], pos: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, data, pos)
}

#[cfg(unix)]
fn write_at(file: &File, data: &[u8], pos: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, data, pos)
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut pos: u64) -> io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let read = std::os::windows::fs::FileExt::seek_read(file, buf, pos)?;
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(file, buf, pos)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[read..];
        pos += read as u64;
    }
    Ok(())
}

/// The persister's own state: how far it has got.
#[derive(Default)]
pub(crate) struct PersistState {
    /// End of the last GOP handled, and the ring epoch it was handled in.
    done: Option<(Hns, u64)>,
    scratch: Vec<u8>,
}

impl PersistState {
    /// Writes every GOP that is ready. Returns how many records were written.
    pub(crate) fn step(
        &mut self,
        recorder: &Recorder,
        ring: &DiskRing,
        pacer: WritePacer,
    ) -> io::Result<usize> {
        let mut written = 0;
        loop {
            let epoch = ring.epoch();
            let after = self.done.filter(|&(_, e)| e == epoch).map(|(end, _)| end);
            let Some(gop) = recorder.next_gop(after, PERSIST_LAG) else { return Ok(written) };
            match gop {
                GopCopy::Ready { clip, end, epoch } => {
                    ring.append(epoch, &clip, &mut self.scratch, pacer)?;
                    self.done = Some((end, epoch));
                    written += 1;
                }
                // The staging area was lapped or cleared under the copy. Skip the GOP.
                GopCopy::Lost { end, epoch } => self.skip_lost(ring, end, epoch),
            }
        }
    }

    /// Skips a GOP that couldn't be copied. The ring's footage up to it can't join the footage
    /// after it without a hole, so the ring is emptied and starts again after the lost GOP.
    fn skip_lost(&mut self, ring: &DiskRing, end: Hns, epoch: u64) {
        // A changed epoch means the ring was cleared already, which is the same thing.
        if ring.epoch() == epoch {
            ring.clear();
        }
        self.done = Some((end, ring.epoch()));
    }
}

/// The thread that writes finished GOPs to the ring. Dropping it stops the thread.
pub struct Persister {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Persister {
    /// Hands `ring` to `recorder` (clips read it, `clear` empties it) and starts the thread that
    /// fills it. `pacer` limits the write rate on an HDD.
    ///
    /// # Errors
    /// The OS refused to start the thread.
    pub fn start(
        recorder: &Arc<Recorder>,
        ring: &Arc<DiskRing>,
        pacer: WritePacer,
    ) -> io::Result<Self> {
        recorder.attach_ring(Arc::clone(ring));
        let (stop, stopped) = mpsc::channel::<()>();
        let (recorder, ring) = (Arc::clone(recorder), Arc::clone(ring));
        let thread = thread::Builder::new().name("zc-io-ring".into()).spawn(move || {
            let mut state = PersistState::default();
            // A message or a closed channel both mean stop; only a timeout means "look again".
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(POLL) {
                if let Err(error) = state.step(&recorder, &ring, pacer) {
                    ring.write_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(%error, "DVR ring write failed");
                }
            }
        })?;
        Ok(Self { stop: Some(stop), thread: Some(thread) })
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("DVR ring thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use zc_core::dvr::DvrConfig;
    use zc_core::testing::SyntheticStream;

    use super::*;
    use zc_core::media::Packet;

    use crate::recorder::Copied;

    /// A scratch folder that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("zc-ring-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Staging for about six 60 KB GOPs, so the persister has to keep up.
    const STAGING: DvrConfig = DvrConfig { video_bytes: 400_000, audio_bytes: 200_000 };
    const ROOMY: DvrConfig = DvrConfig { video_bytes: 10_000_000, audio_bytes: 1_000_000 };

    /// A disk-mode recorder and an all-RAM one, fed the same packets. The disk one is written to
    /// the ring after every second of footage, as the thread would.
    struct Pair {
        disk: Recorder,
        ram: Recorder,
        ring: Arc<DiskRing>,
        state: PersistState,
        stream: SyntheticStream,
    }

    impl Pair {
        fn new(scratch: &Scratch, ring_bytes: u64) -> Self {
            Self::with_staging(scratch, ring_bytes, STAGING)
        }

        fn with_staging(scratch: &Scratch, ring_bytes: u64, staging: DvrConfig) -> Self {
            let disk = Recorder::new(staging);
            let ring = Arc::new(DiskRing::create(&scratch.0, ring_bytes).expect("ring file"));
            disk.attach_ring(Arc::clone(&ring));
            let ram = Recorder::new(ROOMY);
            Self { disk, ram, ring, state: PersistState::default(), stream: SyntheticStream::new() }
        }

        fn feed(&mut self, secs: i64) {
            self.feed_with(secs, |_| {});
        }

        /// Like `feed`, with `each` called before every second, to change the stream.
        fn feed_with(&mut self, secs: i64, mut each: impl FnMut(&mut SyntheticStream)) {
            for _ in 0..secs {
                each(&mut self.stream);
                for p in self.stream.take_secs(1) {
                    self.disk.push(p.view());
                    self.ram.push(p.view());
                }
                self.state.step(&self.disk, &self.ring, WritePacer::unlimited()).expect("persist");
            }
        }
    }

    use zc_core::sink::PacketSink;

    /// A clip is whole: it starts on a keyframe, has no missing video frame, and every payload
    /// is as the stream made it.
    fn assert_intact(clip: &DvrClip) {
        let video: Vec<&Packet> = clip.video().collect();
        assert!(video.first().is_some_and(|p| p.meta.keyframe), "a clip starts on a keyframe");
        for pair in video.windows(2) {
            let (a, b) = (&pair[0].data, &pair[1].data);
            assert_eq!(
                SyntheticStream::sequence_number(b),
                SyntheticStream::sequence_number(a) + 1,
                "no video frame may be missing"
            );
        }
        for p in &clip.packets {
            assert!(p.data[5..].iter().all(|&b| b == 0xAB), "a payload was torn");
        }
        assert!(clip.packets.windows(2).all(|w| w[0].meta.pts <= w[1].meta.pts));
    }

    #[test]
    fn a_lost_gop_empties_the_ring_instead_of_leaving_a_hole() {
        let scratch = Scratch::new("lost");
        let mut pair = Pair::new(&scratch, 8_000_000);
        pair.feed(10);
        assert!(pair.ring.oldest_start().is_some());
        let epoch = pair.ring.epoch();
        let end = pair.disk.video_end();
        pair.state.skip_lost(&pair.ring, end, epoch);
        assert!(pair.ring.oldest_start().is_none(), "older records can't join newer ones");
        assert_eq!(pair.state.done, Some((end, pair.ring.epoch())));
    }

    #[test]
    fn a_disk_clip_has_the_same_packets_as_a_ram_clip() {
        let scratch = Scratch::new("same");
        let mut pair = Pair::new(&scratch, 8_000_000);
        pair.feed(30);
        assert!(pair.ring.index().records.len() >= 20, "most of the footage went through the ring");
        // The status bar counts the ring too, not just the few seconds staged.
        assert_eq!(pair.disk.buffered(), pair.ram.buffered());

        for window in [3, 10, 25, 3_600] {
            let window = Hns::from_secs(window);
            assert_eq!(pair.disk.snapshot(window).unwrap(), pair.ram.snapshot(window).unwrap());
            let end = pair.ram.video_end() - Hns::from_millis(5_500);
            assert_eq!(
                pair.disk.snapshot_until(window, end).unwrap(),
                pair.ram.snapshot_until(window, end).unwrap()
            );
        }
    }

    #[test]
    fn a_clip_after_the_ring_wraps_is_the_newest_footage_from_a_keyframe() {
        let scratch = Scratch::new("wrap");
        // About nine 108 KB records fit, so 40 s wraps the file several times.
        let mut pair = Pair::new(&scratch, 1_000_000);
        pair.feed(40);

        let window = Hns::from_secs(6);
        let clip = pair.disk.snapshot(window).unwrap();
        assert_intact(&clip);
        assert_eq!(
            clip,
            pair.ram.snapshot(window).unwrap(),
            "a short window is the newest footage"
        );

        // Everything the ring holds, oldest end included.
        let all = pair.disk.snapshot(Hns::from_secs(3_600)).unwrap();
        assert_intact(&all);
        let first = all.video().next().unwrap().meta.pts;
        assert!(first > Hns::from_secs(25), "the oldest footage was overwritten: {first:?}");
        assert_eq!(all.video().last(), pair.ram.snapshot(window).unwrap().video().last());
    }

    #[test]
    fn a_record_overwritten_between_plan_and_read_is_dropped() {
        let scratch = Scratch::new("overwrite");
        // Room for about eleven records.
        let mut pair = Pair::new(&scratch, 1_200_000);
        pair.feed(20);
        let planned = pair.disk.snapshot(Hns::from_secs(3_600)).unwrap();
        let plan = pair.disk.plan_clip(Hns::from_secs(3_600), None).unwrap();
        let Copied::Disk(clip) = pair.disk.copy_plan(plan).unwrap() else {
            panic!("a long window comes partly from the ring")
        };

        // Three more seconds wrap the cursor over the oldest records the plan counted on.
        pair.feed(3);
        let clip = clip.collect().unwrap();
        assert_intact(&clip);
        let first = clip.video().next().unwrap().meta.pts;
        let planned_first = planned.video().next().unwrap().meta.pts;
        assert!(first > planned_first, "the overwritten records must go: {first:?}");
        // Nothing is lost after the cut: the clip still runs to the newest planned frame.
        assert_eq!(clip.video().last(), planned.video().last());
    }

    #[test]
    fn clear_forgets_the_ring_and_a_stale_write_is_discarded() {
        let scratch = Scratch::new("clear");
        let mut pair = Pair::new(&scratch, 8_000_000);
        pair.feed(10);
        pair.disk.clear();
        assert!(pair.ring.index().records.is_empty());
        assert!(pair.disk.snapshot(Hns::from_secs(60)).is_err());
        // A GOP copied before the clear and written after it must not come back.
        let before = pair.ring.epoch();
        pair.ring.clear();
        let gop = DvrClip { packets: pair.stream.take_secs(1) };
        let mut scratch_buf = Vec::new();
        pair.ring.append(before, &gop, &mut scratch_buf, WritePacer::unlimited()).unwrap();
        assert!(pair.ring.index().records.is_empty());
    }

    #[test]
    fn growing_makes_room_and_the_file_never_shrinks() {
        let scratch = Scratch::new("grow");
        let mut pair = Pair::new(&scratch, 1_000_000);
        pair.feed(10);
        let before = pair.disk.snapshot(Hns::from_secs(3_600)).unwrap();
        let held = pair.ring.index().records.len();

        assert!(!pair.ring.grow(500_000).unwrap(), "a smaller size is ignored");
        assert_eq!(std::fs::metadata(pair.ring.path()).unwrap().len(), 1_000_000);

        assert!(pair.ring.grow(4_000_000).unwrap());
        assert_eq!(std::fs::metadata(pair.ring.path()).unwrap().len(), 4_000_000);
        assert_eq!(pair.ring.capacity(), 4_000_000);
        assert_eq!(pair.disk.snapshot(Hns::from_secs(3_600)).unwrap(), before);

        // The cursor reaches the new room, and the ring holds more than it could before.
        pair.feed(20);
        assert_intact(&pair.disk.snapshot(Hns::from_secs(3_600)).unwrap());
        assert!(pair.ring.index().records.len() > held + 5, "{held}");
    }

    /// No two records in the index share a byte of the file.
    fn assert_disjoint(ring: &DiskRing) {
        let index = ring.index();
        for (i, a) in index.records.iter().enumerate() {
            assert!(a.at + a.len <= index.capacity);
            for b in index.records.iter().skip(i + 1) {
                assert!(a.at + a.len <= b.at || b.at + b.len <= a.at, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn gops_of_different_sizes_wrap_the_ring_without_a_torn_record() {
        let sizes = [500, 2_000, 800, 1_700, 600, 1_200, 1_900, 700, 1_400, 900, 1_800];
        // A ring of two to five records of these sizes: the cursor often wraps with the oldest
        // record (the tail of the file) clear of the new one while a newer record is in the way.
        for (n, ring_bytes) in [330_000, 380_000, 450_000, 520_000, 640_000].into_iter().enumerate()
        {
            let scratch = Scratch::new(&format!("varied{n}"));
            let staging = DvrConfig { video_bytes: 900_000, audio_bytes: 200_000 };
            let mut pair = Pair::with_staging(&scratch, ring_bytes, staging);
            let mut second = n;
            for _ in 0..80 {
                pair.feed_with(1, |stream| {
                    *stream = stream.clone().frame_bytes(sizes[second % sizes.len()]);
                    second += 3;
                });
                assert_disjoint(&pair.ring);
                // Every clip, from every window, is whole: no torn record and no read error.
                for window in [4, 3_600] {
                    let clip = pair.disk.snapshot(Hns::from_secs(window)).unwrap();
                    assert_intact(&clip);
                    let newest = pair.ram.snapshot(Hns::from_secs(1)).unwrap();
                    assert_eq!(clip.video().last(), newest.video().last());
                }
            }
            assert!(pair.ring.index().next_seq > 40, "the ring wrapped many times");
        }
    }

    #[test]
    fn a_record_overwritten_after_packets_were_written_laps_the_stream() {
        let scratch = Scratch::new("lapped");
        let mut pair = Pair::new(&scratch, 1_200_000);
        pair.feed(20);
        let plan = pair.disk.plan_clip(Hns::from_secs(3_600), None).unwrap();
        let Copied::Disk(mut clip) = pair.disk.copy_plan(plan).unwrap() else {
            panic!("a long window comes partly from the ring")
        };

        // The writer wraps over every planned record once the first packet is out.
        let mut first = true;
        let error = clip
            .stream(&mut |_| {
                if std::mem::take(&mut first) {
                    pair.feed(8);
                }
                Ok(())
            })
            .unwrap_err();
        assert!(is_lapped(&error), "{error}");

        // A fresh plan reads what is left, from an intact keyframe.
        clip.replan();
        let mut packets = Vec::new();
        clip.stream(&mut |p| {
            packets.push(p.to_packet());
            Ok(())
        })
        .unwrap();
        assert_intact(&DvrClip { packets });
    }
}
