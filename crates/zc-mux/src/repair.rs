//! Startup repair for recordings that a crash left unfinished (D18, DESIGN §8).
//!
//! A recording is written as `<name>.mp4.partial`, and the fMP4 writer syncs it after every
//! fragment. If ZeroClip crashes, the game crashes or the power fails, the file can end in the
//! middle of a fragment. [`repair_partial`] cuts it back to its last complete fragment, writes
//! the `mfra` index that a finished file has (see `fmp4`), and renames it to `<name>.mp4`, so the
//! user loses at most about one GOP.
//!
//! The walk reads only box headers and seeks past each body. It never trusts a size from the
//! file: a size that overflows or runs past the end of the file ends the walk. The bodies that
//! are read are the moofs, which give the index and the length, and the moov, which holds the
//! `mehd`. Each is read in full, up to `MAX_BOX_LEN`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::fmp4::{RandomAccessEntry, mfra};

/// A box header with a 32-bit size: four bytes of size and four of type.
const HEADER_LEN: u64 = 8;
/// A box header with a 64-bit largesize (size field `1`): the 8-byte header, then 8 more bytes.
const LARGE_HEADER_LEN: u64 = 16;
/// The largest box the walk reads whole, a moof or the moov. Ours are a few kilobytes, so a bigger
/// one is taken as corrupt, and a damaged size can't make the walk allocate a large buffer.
const MAX_BOX_LEN: u64 = 1 << 20;

/// What [`repair_partial`] did with a `.partial` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repaired {
    /// The file was cut to its last complete fragment, given an index if it had none, and renamed.
    Truncated {
        /// The final name: `clip.mp4`, or `clip (2).mp4` and so on if that name was taken.
        path: PathBuf,
        /// Bytes kept, which is the end of the last complete fragment, or of the index after it
        /// if the file already had one. An index written by the repair comes after this.
        kept_bytes: u64,
        /// Bytes cut from the end of the file. Zero when the file was already complete.
        dropped_bytes: u64,
    },
    /// The init segment or the first fragment is incomplete. The file was left untouched.
    Unrecoverable,
}

/// Finds where the valid part of an fMP4 file ends: its last complete fragment, or the `mfra`
/// index after the fragments when that is complete too.
///
/// The file is an init segment, `ftyp` then `moov`, followed by a `moof` and its `mdat` for
/// each fragment, and then, once a recording finishes, an `mfra`. This walks those top-level
/// boxes and returns the offset just past the last whole box. It stops at the first box that
/// the file ends inside, at a size of 0 (which means "to the end of the file", and our writer
/// never writes one), at a size too small for its own header, or at a size that runs past the
/// end of the file.
///
/// # Errors
/// Only I/O errors from `r`. A damaged file, or one that is not an MP4 at all, gives `Ok(None)`.
pub fn last_complete_fragment_end<R: Read + Seek>(r: &mut R) -> io::Result<Option<u64>> {
    Ok(whole_parts(r)?.map(|whole| whole.index_end.unwrap_or(whole.fragments_end)))
}

/// Where the whole parts of an fMP4 file end: its fragments, and the index after them if the
/// index is complete.
struct Whole {
    /// The end of the last complete `moof` and `mdat` pair.
    fragments_end: u64,
    /// The end of the `mfra` that follows the fragments, if it is complete.
    index_end: Option<u64>,
}

/// Walks the top-level boxes of an fMP4 file. See [`last_complete_fragment_end`]. Returns `None`
/// if there is no complete fragment.
fn whole_parts<R: Read + Seek>(r: &mut R) -> io::Result<Option<Whole>> {
    let file_len = r.seek(SeekFrom::End(0))?;
    let Some((ftyp, after_ftyp)) = box_at(r, 0, file_len)? else {
        return Ok(None);
    };
    let Some((moov, after_moov)) = box_at(r, after_ftyp, file_len)? else {
        return Ok(None);
    };
    if ftyp != *b"ftyp" || moov != *b"moov" {
        return Ok(None);
    }

    // A fragment is a moof, then its mdat. Anything else, or an incomplete pair, ends the walk.
    let mut fragment_end = None;
    let mut pos = after_moov;
    while let Some((moof, after_moof)) = box_at(r, pos, file_len)? {
        let Some((mdat, after_mdat)) = box_at(r, after_moof, file_len)? else {
            break;
        };
        if moof != *b"moof" || mdat != *b"mdat" {
            break;
        }
        fragment_end = Some(after_mdat);
        pos = after_mdat;
    }
    let Some(fragments_end) = fragment_end else {
        return Ok(None);
    };
    // Only an mfra right after the fragments counts. One that is cut short is not complete.
    let index_end = match box_at(r, fragments_end, file_len)? {
        Some((kind, end)) if kind == *b"mfra" => Some(end),
        _ => None,
    };
    Ok(Some(Whole { fragments_end, index_end }))
}

/// Reads the header of the box at `pos`. Returns its four-character type and the offset where
/// it ends, or `None` if the box cannot be complete: the file ends inside its header, its size
/// is 0 or too small for its header, or it runs past `file_len`. The body is never read.
fn box_at<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    file_len: u64,
) -> io::Result<Option<([u8; 4], u64)>> {
    r.seek(SeekFrom::Start(pos))?;
    let mut header = [0; 8];
    if !read_full(r, &mut header)? {
        return Ok(None);
    }
    let kind = [header[4], header[5], header[6], header[7]];
    let size = match u64::from(u32::from_be_bytes([header[0], header[1], header[2], header[3]])) {
        // Size 0 means "to the end of the file". Our writer never writes one, so it is incomplete.
        0 => return Ok(None),
        // Size 1 means the real size is a 64-bit largesize, right after the type.
        1 => {
            let mut largesize = [0; 8];
            if !read_full(r, &mut largesize)? {
                return Ok(None);
            }
            let size = u64::from_be_bytes(largesize);
            if size < LARGE_HEADER_LEN {
                return Ok(None);
            }
            size
        }
        size if size < HEADER_LEN => return Ok(None),
        size => size,
    };
    // Checked, so a corrupt size near 2^64 cannot wrap around to a small end offset.
    match pos.checked_add(size) {
        Some(end) if end <= file_len => Ok(Some((kind, end))),
        _ => Ok(None),
    }
}

/// Fills `buf` from `r`. Returns `Ok(false)` if the file ends before `buf` is full.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// Repairs one `.partial` recording: cuts it to its last complete fragment, gives it an `mfra`
/// index if it has none, sets its length in the `mehd`, then renames it to its final name, which
/// is the name without the `.partial` suffix.
///
/// A file with data is never replaced. If `clip.mp4` exists and is not empty, the recording
/// becomes `clip (2).mp4`, then `clip (3).mp4`, and so on. An empty `clip.mp4` is taken as the
/// placeholder that the engine creates when a recording starts, so the repaired file replaces
/// it. A file with no complete fragment is left exactly as it was, still named `.partial`.
/// Nothing is deleted.
///
/// The check on the name and the rename are two steps. In between, a file could appear at the
/// chosen name, or a placeholder could be filled. On Windows the rename would then replace it.
/// The gap is a few milliseconds, and nothing else writes these names at startup, so it is
/// accepted. `std` has no atomic rename that refuses to replace.
///
/// # Errors
/// I/O errors from opening, truncating, syncing or renaming the file. `InvalidInput` if
/// `partial` doesn't end in `.partial`, which is checked before the file is touched.
pub fn repair_partial(partial: &Path) -> io::Result<Repaired> {
    let finished = finished_name(partial)?;
    let (kept_bytes, file_len) = {
        let mut file = OpenOptions::new().read(true).write(true).open(partial)?;
        let file_len = file.metadata()?.len();
        let Some(whole) = whole_parts(&mut file)? else {
            return Ok(Repaired::Unrecoverable);
        };
        let kept_bytes = whole.index_end.unwrap_or(whole.fragments_end);
        if kept_bytes < file_len {
            file.set_len(kept_bytes)?;
            file.sync_all()?;
        }
        // A file that stops on a fragment boundary, which is the usual crash, has no index yet.
        if whole.index_end.is_none() {
            append_index(&mut file, whole.fragments_end);
        }
        // The length goes in the file's mehd, so Media Foundation reports it. A failure only costs
        // that, since the file still plays.
        if let Err(error) = set_duration_in(&mut file, whole.fragments_end) {
            warn!(%error, "could not set the length of a repaired recording");
        }
        (kept_bytes, file_len)
    };
    let path = final_path(partial, finished)?;
    fs::rename(partial, &path)?;
    Ok(Repaired::Truncated { path, kept_bytes, dropped_bytes: file_len.saturating_sub(kept_bytes) })
}

/// Writes an `mfra` index after the fragments that end at `end`, so that Media Foundation reports
/// a duration. The index is best effort: if it cannot be built or written, the file keeps its
/// fragments without one, which still plays in VLC, and the reason is logged.
fn append_index(file: &mut File, end: u64) {
    let written = index_trailer(file, end).and_then(|trailer| {
        file.seek(SeekFrom::Start(end))?;
        file.write_all(&trailer)?;
        file.sync_all()
    });
    if let Err(error) = written {
        warn!(%error, "could not index a repaired recording; it will report no duration");
        // Cut off whatever a failed write left after the fragments.
        if let Err(error) = file.set_len(end) {
            warn!(%error, "could not remove a partly written index from a repaired recording");
        }
    }
}

/// What a walk over a file's fragments finds: each track's `mfra` entries, and the length of the
/// presentation, which is where the latest sample of any track ends.
struct Walk {
    tracks: BTreeMap<u32, Vec<RandomAccessEntry>>,
    duration: u64,
}

/// Walks the fragments that end at `end`, by reading each moof in full, up to [`MAX_BOX_LEN`]. The
/// entries are the writer's: a first-sample entry per fragment and track. Anything unexpected is an
/// error, and the caller keeps the file as it is.
fn walk_fragments<R: Read + Seek>(r: &mut R, end: u64) -> io::Result<Walk> {
    let (_, after_ftyp) = box_at(r, 0, end)?.ok_or_else(malformed)?;
    let (_, mut pos) = box_at(r, after_ftyp, end)?.ok_or_else(malformed)?;
    let mut tracks: BTreeMap<u32, Vec<RandomAccessEntry>> = BTreeMap::new();
    let mut duration = 0;
    while pos < end {
        let (moof, after_moof) = box_at(r, pos, end)?.ok_or_else(malformed)?;
        let (mdat, after_mdat) = box_at(r, after_moof, end)?.ok_or_else(malformed)?;
        let moof_len = after_moof - pos;
        if moof != *b"moof" || mdat != *b"mdat" || moof_len > MAX_BOX_LEN {
            return Err(malformed());
        }
        let mut bytes = vec![0; usize::try_from(moof_len).map_err(|_| malformed())?];
        r.seek(SeekFrom::Start(pos))?;
        r.read_exact(&mut bytes)?;
        let (_, body, _) = split_box(&bytes).ok_or_else(malformed)?;
        let found = trafs(body).ok_or_else(malformed)?;
        for (index, traf) in found.into_iter().enumerate() {
            let traf_number = u8::try_from(index + 1).map_err(|_| malformed())?;
            let entry = RandomAccessEntry { time: traf.base, moof_offset: pos, traf_number };
            tracks.entry(traf.track_id).or_default().push(entry);
            duration = duration.max(traf.end);
        }
        pos = after_mdat;
    }
    Ok(Walk { tracks, duration })
}

/// Builds the `mfra` index of the fragments that end at `end`. See [`walk_fragments`].
fn index_trailer<R: Read + Seek>(r: &mut R, end: u64) -> io::Result<Vec<u8>> {
    let walk = walk_fragments(r, end)?;
    let indexes: Vec<(u32, &[RandomAccessEntry])> =
        walk.tracks.iter().map(|(&track_id, entries)| (track_id, entries.as_slice())).collect();
    mfra(&indexes)
}

/// Sets the length of the fragmented file at `path` in its `mehd`, so that Media Foundation
/// reports the whole length, not the start of its last fragment (ISO/IEC 14496-12 §8.8.2). The
/// length is where the latest sample of any track ends. A file with no `mehd` is left as it is.
///
/// # Errors
/// I/O errors, and a file whose fragments or init segment cannot be read.
pub fn set_fragment_duration(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let Some(whole) = whole_parts(&mut file)? else {
        return Ok(());
    };
    set_duration_in(&mut file, whole.fragments_end)
}

/// Writes the length of the fragments that end at `end` into the file's `mehd`, if it has one,
/// and syncs the file.
fn set_duration_in(file: &mut File, end: u64) -> io::Result<()> {
    let Some(at) = mehd_duration_offset(file, end)? else {
        return Ok(());
    };
    let walk = walk_fragments(file, end)?;
    file.seek(SeekFrom::Start(at))?;
    file.write_all(&walk.duration.to_be_bytes())?;
    file.sync_all()
}

/// The file offset of the duration in the `mehd` of the init segment, if the file has one.
/// `mehd` is a version 1 FullBox, so its 64-bit duration follows the 8-byte header and the version
/// and flags. Only that version is written, so any other is left alone.
fn mehd_duration_offset<R: Read + Seek>(r: &mut R, end: u64) -> io::Result<Option<u64>> {
    let Some((_, after_ftyp)) = box_at(r, 0, end)? else {
        return Ok(None);
    };
    let Some((moov, after_moov)) = box_at(r, after_ftyp, end)? else {
        return Ok(None);
    };
    if moov != *b"moov" {
        return Ok(None);
    }
    let moov_len = after_moov - after_ftyp;
    if moov_len > MAX_BOX_LEN {
        return Err(malformed());
    }
    let mut moov_box = vec![0; usize::try_from(moov_len).map_err(|_| malformed())?];
    r.seek(SeekFrom::Start(after_ftyp))?;
    r.read_exact(&mut moov_box)?;
    let (_, moov_body, _) = split_box(&moov_box).ok_or_else(malformed)?;
    let Some((mvex_at, mvex)) = child_box(moov_body, b"mvex") else {
        return Ok(None);
    };
    let Some((mehd_at, mehd)) = child_box(mvex, b"mehd") else {
        return Ok(None);
    };
    if mehd.len() != 12 || mehd[0] != 1 {
        return Ok(None);
    }
    // Each child's offset counts from its parent's body. A body follows its 8-byte header, so
    // mehd's body is at the moov's body, plus mvex's offset and header, plus mehd's offset and
    // header. The duration follows the 4 bytes of version and flags.
    Ok(Some(after_ftyp + HEADER_LEN + mvex_at + HEADER_LEN + mehd_at + HEADER_LEN + 4))
}

/// The offset from the start of `parent` of its first child box of type `kind`, and that child's
/// body. `parent` is a box body, so its children follow one another from its first byte.
fn child_box<'a>(parent: &'a [u8], kind: &[u8; 4]) -> Option<(u64, &'a [u8])> {
    let mut rest = parent;
    while !rest.is_empty() {
        let start = u64::try_from(parent.len() - rest.len()).ok()?;
        let (found, body, after) = split_box(rest)?;
        if &found == kind {
            return Some((start, body));
        }
        rest = after;
    }
    None
}

/// The error for a fragment that the index walk cannot read.
fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "a fragment of the recording is malformed")
}

/// Splits the first box off `buf`: its type, its body, and the bytes after it. Only 32-bit sizes
/// are read here, since the writer never puts a larger box inside a moof.
fn split_box(buf: &[u8]) -> Option<([u8; 4], &[u8], &[u8])> {
    let (header, _) = buf.split_first_chunk::<8>()?;
    let size =
        usize::try_from(u32::from_be_bytes([header[0], header[1], header[2], header[3]])).ok()?;
    if !(8..=buf.len()).contains(&size) {
        return None;
    }
    Some(([header[4], header[5], header[6], header[7]], &buf[8..size], &buf[size..]))
}

/// What the index and the length need from a `traf`: its track, the base time of its first sample,
/// and where its last sample ends.
struct TrafTimes {
    track_id: u32,
    base: u64,
    end: u64,
}

/// Each `traf` in a moof body, in order.
fn trafs(moof: &[u8]) -> Option<Vec<TrafTimes>> {
    let mut found = Vec::new();
    let mut rest = moof;
    while !rest.is_empty() {
        let (kind, body, after) = split_box(rest)?;
        if &kind == b"traf" {
            found.push(traf_times(body)?);
        }
        rest = after;
    }
    Some(found)
}

/// Reads a `traf`'s `tfhd` (track ID), its `tfdt` (base time, version 1 or 0) and its `trun`. The
/// trun must have the writer's layout, with flags 0x701: a data offset, then a duration, size and
/// flags for each sample. The traf ends once the durations of all its samples have passed.
fn traf_times(traf: &[u8]) -> Option<TrafTimes> {
    let (mut track_id, mut base, mut trun) = (None, None, None);
    let mut rest = traf;
    while !rest.is_empty() {
        let (kind, body, after) = split_box(rest)?;
        match &kind {
            b"tfhd" => track_id = Some(u32::from_be_bytes(body.get(4..8)?.try_into().ok()?)),
            b"tfdt" => {
                base = Some(match body.first()? {
                    1 => u64::from_be_bytes(body.get(4..12)?.try_into().ok()?),
                    _ => u64::from(u32::from_be_bytes(body.get(4..8)?.try_into().ok()?)),
                });
            }
            b"trun" => trun = Some(body),
            _ => {}
        }
        rest = after;
    }
    let (track_id, base, trun) = (track_id?, base?, trun?);
    if u32::from_be_bytes(trun.get(0..4)?.try_into().ok()?) & 0x00FF_FFFF != 0x701 {
        return None;
    }
    let samples = u32::from_be_bytes(trun.get(4..8)?.try_into().ok()?);
    if samples == 0 {
        return None;
    }
    // Each sample is 12 bytes, after the count and the data offset.
    let entries = trun.get(12..)?;
    if entries.len() != 12 * usize::try_from(samples).ok()? {
        return None;
    }
    // The length check above means there is no partial entry left over, so `.0` is every entry.
    let duration: u64 = entries
        .as_chunks::<12>()
        .0
        .iter()
        .map(|entry| u64::from(u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]])))
        .sum();
    Some(TrafTimes { track_id, base, end: base + duration })
}

/// The final name of a `.partial` file: its name without the `.partial` suffix.
fn finished_name(partial: &Path) -> io::Result<&str> {
    partial
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_suffix(".partial"))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a recording is repaired from a .partial file",
            )
        })
}

/// The name a finished recording gets, in the same folder as `partial`. That is `clip.mp4`
/// when the name is free, or holds only an empty placeholder. Otherwise it is the first of
/// `clip (2).mp4`, `clip (3).mp4` and so on that is free, with the copy number before the
/// extension.
fn final_path(partial: &Path, finished: &str) -> io::Result<PathBuf> {
    let plain = partial.with_file_name(finished);
    let usable = match metadata_of(&plain)? {
        None => true,
        Some(metadata) => metadata.is_file() && metadata.len() == 0,
    };
    if usable {
        return Ok(plain);
    }
    let (stem, ext) = match finished.rfind('.') {
        Some(dot) if dot > 0 => finished.split_at(dot),
        _ => (finished, ""),
    };
    let mut copy = 2;
    loop {
        let candidate = partial.with_file_name(format!("{stem} ({copy}){ext}"));
        if metadata_of(&candidate)?.is_none() {
            return Ok(candidate);
        }
        copy += 1;
    }
}

/// The metadata of whatever is at `path`, a dangling symlink included, or `None` if nothing is.
/// Other errors are passed on rather than read as "free", so a rename never replaces a file
/// that could not be checked.
fn metadata_of(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    use zc_core::media::{
        Codec, ContainerFormat, Packet, PacketMeta, TrackId, TrackInfo, TrackKind,
    };
    use zc_core::testing::SyntheticStream;
    use zc_core::time::Hns;

    use super::*;
    use crate::container::create;

    /// A writer's output, kept by the test so the bytes can be read back. The writer owns one
    /// clone, the test keeps another.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("the lock is never poisoned in tests").clone()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("the lock is never poisoned in tests").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A video track and the game and mic audio tracks, as a recording declares them.
    fn recording_tracks() -> Vec<TrackInfo> {
        let video = TrackInfo {
            track: TrackId::Video,
            label: "Video".into(),
            kind: TrackKind::Video { codec: Codec::H264, width: 320, height: 240, fps: 60 },
            codec_private: Vec::new(),
        };
        let audio = |track, label: &str| TrackInfo {
            track,
            label: label.into(),
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            codec_private: Vec::new(),
        };
        vec![video, audio(TrackId::GameAudio, "Game"), audio(TrackId::Mic, "Mic")]
    }

    /// A complete fMP4 file of `secs` seconds, which has one fragment per second.
    fn recording(secs: i64) -> Vec<u8> {
        let out = SharedBuf::default();
        let mut writer =
            create(ContainerFormat::FragmentedMp4, Box::new(out.clone()), &recording_tracks())
                .expect("a recording writer");
        for packet in SyntheticStream::new().take_secs(secs) {
            writer.write_packet(packet.view()).expect("packets are accepted");
        }
        writer.finish().expect("finish");
        out.bytes()
    }

    /// The top-level boxes of a file, as (type, start, end). Reads the 32-bit sizes that the
    /// writer uses, so it is for test fixtures only.
    fn boxes(file: &[u8]) -> Vec<([u8; 4], u64, u64)> {
        let mut found = Vec::new();
        let mut start = 0;
        while start < file.len() {
            let size = u32::from_be_bytes(file[start..start + 4].try_into().expect("4 bytes"));
            let kind: [u8; 4] = file[start + 4..start + 8].try_into().expect("4 bytes");
            let end = start + size as usize;
            found.push((kind, start as u64, end as u64));
            start = end;
        }
        found
    }

    /// The end of the last complete fragment of `bytes`, for a file cut at its end.
    fn end_of(bytes: &[u8]) -> Option<u64> {
        last_complete_fragment_end(&mut Cursor::new(bytes)).expect("a cursor never fails")
    }

    /// The bytes of a file's init segment, `ftyp` and `moov`.
    fn init_of(file: &[u8]) -> Vec<u8> {
        file[..boxes(file)[1].2 as usize].to_vec()
    }

    fn u32_at(buf: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
    }

    fn u64_at(buf: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(buf[at..at + 8].try_into().expect("8 bytes"))
    }

    /// Asserts that an `mfra` runs from `at` to the end of `file`, with one `tfra` per track, and
    /// that the offset in each `tfra` entry is a `moof`.
    fn assert_index_follows(file: &[u8], at: u64) {
        let index = &file[at as usize..];
        assert_eq!(&index[4..8], b"mfra", "an index follows the fragments");
        assert_eq!(u32_at(index, 0) as usize, index.len(), "mfra runs to the end of the file");
        assert_eq!(u32_at(index, index.len() - 4) as usize, index.len(), "mfro holds the size");
        let body = &index[8..];
        let mut tfras = 0;
        for (kind, start, end) in boxes(body) {
            if &kind == b"tfra" {
                tfras += 1;
                // The tfra's body is after its 8-byte header: a version, flags, track ID, the
                // length word, the entry count, then 19-byte entries.
                let tfra = &body[start as usize + 8..end as usize];
                for entry in tfra[16..].as_chunks::<19>().0.iter() {
                    let moof = u64_at(entry, 8) as usize;
                    assert_eq!(&file[moof + 4..moof + 8], b"moof", "an entry points at a moof");
                }
            }
        }
        assert_eq!(tfras, 3, "one tfra per track");
    }

    /// A scratch folder under the temp directory, removed when it goes out of scope, even if
    /// the test panics.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("zc-repair-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn clean_file_ends_at_its_length() {
        let file = recording(3);
        assert_eq!(end_of(&file), Some(file.len() as u64));
    }

    #[test]
    fn truncated_mid_mdat_of_the_third_fragment_keeps_two() {
        let file = recording(3);
        let boxes = boxes(&file);
        let (frag2_end, mdat3_start) = (boxes[5].2, boxes[7].1);
        let cut = mdat3_start as usize + 20;
        assert_eq!(end_of(&file[..cut]), Some(frag2_end));
    }

    #[test]
    fn truncated_inside_the_third_moof_keeps_two() {
        let file = recording(3);
        let boxes = boxes(&file);
        let (frag2_end, moof3_start, moof3_end) = (boxes[5].2, boxes[6].1, boxes[6].2);
        for cut in [moof3_start + 4, moof3_start + 8, moof3_start + 30, moof3_end - 1] {
            assert_eq!(end_of(&file[..cut as usize]), Some(frag2_end), "cut at {cut}");
        }
    }

    #[test]
    fn truncated_inside_moov_is_none() {
        let file = recording(3);
        let moov = boxes(&file)[1];
        assert_eq!(end_of(&file[..(moov.1 as usize + 40)]), None);
    }

    #[test]
    fn trailing_size_zero_box_is_incomplete() {
        // A size of 0 means "to the end of the file". The file ends after the last whole
        // fragment, so the box is incomplete and the end is still the last fragment's.
        let mut file = recording(2);
        let complete = file.len() as u64;
        file.extend_from_slice(&[0, 0, 0, 0, b'm', b'o', b'o', b'f', 1, 2, 3, 4]);
        assert_eq!(end_of(&file), Some(complete));
    }

    #[test]
    fn every_cut_point_keeps_only_whole_fragments() {
        // Sweeps every possible truncation of a three-fragment file, so no cut point is missed.
        // The index after the fragments counts only once it is whole, which is the full length.
        let file = recording(3);
        let len = file.len() as u64;
        let boxes = boxes(&file);
        let (frag1, frag2, frag3) = (boxes[3].2, boxes[5].2, boxes[7].2);
        for cut in 0..=len {
            let expected = match cut {
                c if c < frag1 => None,
                c if c < frag2 => Some(frag1),
                c if c < frag3 => Some(frag2),
                c if c < len => Some(frag3),
                _ => Some(len),
            };
            assert_eq!(end_of(&file[..cut as usize]), expected, "cut at {cut}");
        }
    }

    #[test]
    fn largesize_boxes_are_read_as_64_bit_sizes() {
        // Two hand-made fragments whose boxes all use the 64-bit size form (size field 1).
        // Each moof and mdat is 16 + 8 bytes, so a fragment is 48 bytes.
        let init = init_of(&recording(1));
        let mut file = init.clone();
        for _ in 0..2 {
            for (kind, body) in [(b"moof", [0u8; 8]), (b"mdat", [1; 8])] {
                file.extend_from_slice(&[0, 0, 0, 1]);
                file.extend_from_slice(kind);
                file.extend_from_slice(&(LARGE_HEADER_LEN + body.len() as u64).to_be_bytes());
                file.extend_from_slice(&body);
            }
        }
        assert_eq!(end_of(&file), Some(file.len() as u64));
        // One byte short of the second mdat: only the first fragment is complete.
        assert_eq!(end_of(&file[..file.len() - 1]), Some(init.len() as u64 + 48));
    }

    #[test]
    fn corrupt_sizes_stop_the_walk_without_panicking() {
        // A complete file, then a tail with a corrupt header. The walk must keep what it had.
        let file = recording(2);
        let complete = file.len() as u64;
        let tails: [&[u8]; 7] = [
            // largesize 2^63 - 1 and 2^63: far past the end, and near the overflow point.
            &[0, 0, 0, 1, b'm', b'o', b'o', b'f', 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            &[0, 0, 0, 1, b'm', b'o', b'o', b'f', 0x80, 0, 0, 0, 0, 0, 0, 0],
            // largesize 4, smaller than its own 16-byte header.
            &[0, 0, 0, 1, b'm', b'o', b'o', b'f', 0, 0, 0, 0, 0, 0, 0, 4],
            // 32-bit size 4, smaller than the 8-byte header.
            &[0, 0, 0, 4, b'm', b'o', b'o', b'f'],
            // 32-bit size 4 GiB, with nothing after it.
            &[0xFF, 0xFF, 0xFF, 0xFF, b'm', b'd', b'a', b't'],
            // 16 bytes claimed, 8 present.
            &[0, 0, 0, 16, b'm', b'o', b'o', b'f'],
            // A header cut short: 3 of the 8 bytes.
            &[0, 0, 0],
        ];
        for tail in tails {
            let mut corrupt = file.clone();
            corrupt.extend_from_slice(tail);
            assert_eq!(end_of(&corrupt), Some(complete), "tail {tail:02x?}");
        }
    }

    #[test]
    fn not_an_mp4_is_none() {
        assert_eq!(end_of(b""), None);
        assert_eq!(end_of(b"this is not a recording at all"), None);
        // A moov on its own, without the ftyp in front, is not a recording either.
        let file = recording(1);
        assert_eq!(end_of(&file[boxes(&file)[1].1 as usize..]), None);
    }

    #[test]
    fn repair_truncates_and_renames_to_the_final_name() {
        let dir = ScratchDir::new("renames");
        let file = recording(3);
        let boxes = boxes(&file);
        let (frag2_end, cut) = (boxes[5].2, boxes[7].1 + 20);
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file[..cut as usize]).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert_eq!(
            outcome,
            Repaired::Truncated {
                path: dir.join("clip.mp4"),
                kept_bytes: frag2_end,
                dropped_bytes: cut - frag2_end,
            }
        );
        assert!(!partial.exists());
        let repaired = fs::read(dir.join("clip.mp4")).expect("final file");
        // The init segment's mehd now holds the length, so the fragments are compared from its end.
        let init_end = boxes[1].2 as usize;
        assert_eq!(
            &repaired[init_end..frag2_end as usize],
            &file[init_end..frag2_end as usize],
            "fragments kept"
        );
        assert_index_follows(&repaired, frag2_end);
    }

    #[test]
    fn repair_renames_over_an_empty_placeholder() {
        // The engine reserves the final name with an empty file when a recording starts.
        let dir = ScratchDir::new("placeholder");
        fs::write(dir.join("clip.mp4"), b"").expect("placeholder");
        let file = recording(1);
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert_eq!(
            outcome,
            Repaired::Truncated {
                path: dir.join("clip.mp4"),
                kept_bytes: file.len() as u64,
                dropped_bytes: 0,
            }
        );
        assert_eq!(fs::read(dir.join("clip.mp4")).expect("repaired"), finished(&dir, &file));
        assert!(!dir.join("clip (2).mp4").exists(), "no copy number for a placeholder");
        assert!(!partial.exists());
    }

    #[test]
    fn repair_keeps_a_non_empty_file_and_takes_the_next_name() {
        let dir = ScratchDir::new("clash");
        fs::write(dir.join("clip.mp4"), b"first").expect("existing");
        let file = recording(1);
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert_eq!(
            outcome,
            Repaired::Truncated {
                path: dir.join("clip (2).mp4"),
                kept_bytes: file.len() as u64,
                dropped_bytes: 0,
            }
        );
        assert_eq!(fs::read(dir.join("clip.mp4")).expect("kept"), b"first");
        assert_eq!(fs::read(dir.join("clip (2).mp4")).expect("repaired"), finished(&dir, &file));
    }

    #[test]
    fn repair_skips_copy_numbers_that_are_taken() {
        let dir = ScratchDir::new("taken");
        fs::write(dir.join("clip.mp4"), b"first").expect("existing");
        fs::write(dir.join("clip (2).mp4"), b"second").expect("existing");
        let file = recording(1);
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert_eq!(
            outcome,
            Repaired::Truncated {
                path: dir.join("clip (3).mp4"),
                kept_bytes: file.len() as u64,
                dropped_bytes: 0,
            }
        );
        assert_eq!(fs::read(dir.join("clip.mp4")).expect("first"), b"first");
        assert_eq!(fs::read(dir.join("clip (2).mp4")).expect("second"), b"second");
        assert_eq!(fs::read(dir.join("clip (3).mp4")).expect("repaired"), finished(&dir, &file));
    }

    #[test]
    fn unrecoverable_files_are_left_untouched() {
        let dir = ScratchDir::new("unrecoverable");
        let file = recording(2);
        // The init segment is whole, but the first fragment is cut short.
        let torn = file[..boxes(&file)[1].2 as usize + 30].to_vec();
        let partial = dir.join("torn.mp4.partial");
        fs::write(&partial, &torn).expect("partial file");

        assert_eq!(repair_partial(&partial).expect("repair"), Repaired::Unrecoverable);

        assert_eq!(fs::read(&partial).expect("still there"), torn, "not truncated");
        assert!(!dir.join("torn.mp4").exists(), "not renamed");
    }

    #[test]
    fn a_name_without_the_partial_suffix_is_refused_before_the_file_is_touched() {
        let dir = ScratchDir::new("refused");
        let file = recording(2);
        let path = dir.join("clip.mp4");
        fs::write(&path, &file[..file.len() - 5]).expect("file");

        let error = repair_partial(&path).expect_err("not a .partial file");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&path).expect("untouched").len(), file.len() - 5);
    }

    #[test]
    fn repaired_file_ends_with_the_index_a_finished_file_has() {
        // Cut into the third fragment: the repair keeps two fragments and must index them.
        let dir = ScratchDir::new("indexed");
        let file = recording(3);
        let parts = boxes(&file);
        let (frag2_end, cut) = (parts[5].2, parts[7].1 + 20);
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file[..cut as usize]).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        let Repaired::Truncated { path, .. } = outcome else {
            panic!("expected a repaired file, not {outcome:?}");
        };
        let repaired = fs::read(path).expect("repaired file");
        let init_end = parts[1].2 as usize;
        assert_eq!(&repaired[init_end..frag2_end as usize], &file[init_end..frag2_end as usize]);
        assert_index_follows(&repaired, frag2_end);
        // The index is the one a finished two-second recording has, since it holds the same
        // fragments at the same offsets.
        let two = recording(2);
        let two_index = boxes(&two).last().expect("a box").1 as usize;
        assert_eq!(&repaired[frag2_end as usize..], &two[two_index..]);
    }

    #[test]
    fn a_file_that_stops_on_a_fragment_gets_the_index_of_a_finished_one() {
        // A crash between two keyframes leaves a file that ends on a fragment, with no index.
        // The repair adds one, and the result is byte for byte the finished recording.
        let dir = ScratchDir::new("boundary");
        let file = recording(3);
        let frag3_end = boxes(&file)[7].2;
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file[..frag3_end as usize]).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert_eq!(
            outcome,
            Repaired::Truncated {
                path: dir.join("clip.mp4"),
                kept_bytes: frag3_end,
                dropped_bytes: 0,
            }
        );
        assert_eq!(fs::read(dir.join("clip.mp4")).expect("repaired"), finished(&dir, &file));
    }

    #[test]
    fn a_moof_too_big_to_index_keeps_the_file_without_an_index() {
        // A 2 MiB moof is over MAX_BOX_LEN. The fragments are whole, so they are kept, and the
        // index is skipped rather than the repair failing.
        let dir = ScratchDir::new("big-moof");
        let init = init_of(&recording(1));
        let mut file = init.clone();
        file.extend_from_slice(&(2u32 << 20).to_be_bytes());
        file.extend_from_slice(b"moof");
        file.resize(init.len() + (2 << 20), 0);
        file.extend_from_slice(&[0, 0, 0, 8, b'm', b'd', b'a', b't']);
        let partial = dir.join("big.mp4.partial");
        fs::write(&partial, &file).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert_eq!(
            outcome,
            Repaired::Truncated {
                path: dir.join("big.mp4"),
                kept_bytes: file.len() as u64,
                dropped_bytes: 0,
            }
        );
        assert_eq!(fs::read(dir.join("big.mp4")).expect("repaired"), file, "kept, unindexed");
    }

    /// The body of the first box of type `kind` directly inside `buf`.
    fn body_of<'a>(buf: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
        let (_, start, end) =
            boxes(buf).into_iter().find(|(found, _, _)| found == kind).expect("box present");
        &buf[start as usize + 8..end as usize]
    }

    /// The length written in a file's `mehd`: its 64-bit duration, after the version and flags.
    fn mehd_length(file: &[u8]) -> u64 {
        u64_at(body_of(body_of(body_of(file, b"moov"), b"mvex"), b"mehd"), 4)
    }

    /// `file` as a finished recording has it: its length written into its `mehd`.
    fn finished(dir: &ScratchDir, file: &[u8]) -> Vec<u8> {
        let path = dir.join("finished-check.mp4");
        fs::write(&path, file).expect("finished copy");
        set_fragment_duration(&path).expect("length");
        fs::read(&path).expect("finished copy")
    }

    /// A video-only file of two fragments. Its last sample starts at 120 ms and lasts 40 ms, so
    /// the file is 160 ms long.
    fn short_video_file() -> Vec<u8> {
        let out = SharedBuf::default();
        let video = recording_tracks().remove(0);
        let mut writer = create(ContainerFormat::FragmentedMp4, Box::new(out.clone()), &[video])
            .expect("writer");
        for (ms, keyframe) in [(0, true), (40, false), (80, true), (120, false)] {
            let packet = Packet {
                meta: PacketMeta {
                    track: TrackId::Video,
                    pts: Hns::from_millis(ms),
                    duration: Hns::from_millis(40),
                    keyframe,
                },
                data: vec![0x65, 1, 2, 3],
            };
            writer.write_packet(packet.view()).expect("packet");
        }
        writer.finish().expect("finish");
        out.bytes()
    }

    #[test]
    fn set_fragment_duration_writes_where_the_last_sample_ends() {
        let dir = ScratchDir::new("length");
        let path = dir.join("clip.mp4");
        fs::write(&path, short_video_file()).expect("file");

        set_fragment_duration(&path).expect("length");

        // 160 ms, in 100 ns ticks.
        assert_eq!(mehd_length(&fs::read(&path).expect("file")), 1_600_000);
    }

    #[test]
    fn repair_sets_the_length_of_the_fragments_it_keeps() {
        // Only the first fragment is whole. Its last sample starts at 40 ms and lasts 40 ms.
        let dir = ScratchDir::new("kept-length");
        let file = short_video_file();
        let first_end = boxes(&file)[3].2;
        let partial = dir.join("clip.mp4.partial");
        fs::write(&partial, &file[..first_end as usize]).expect("partial file");

        let outcome = repair_partial(&partial).expect("repair");

        assert!(matches!(outcome, Repaired::Truncated { .. }), "{outcome:?}");
        let repaired = fs::read(dir.join("clip.mp4")).expect("repaired");
        assert_eq!(mehd_length(&repaired), 800_000, "80 ms");
    }
}
