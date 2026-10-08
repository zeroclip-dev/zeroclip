//! Startup repair for recordings that a crash left unfinished (D18, DESIGN §8).
//!
//! A recording is written as `<name>.mp4.partial`, and the fMP4 writer syncs it after every
//! fragment. If ZeroClip crashes, the game crashes or the power fails, the file can end in the
//! middle of a fragment. [`repair_partial`] cuts it back to its last complete fragment and
//! renames it to `<name>.mp4`, so the user loses at most about one GOP.
//!
//! The walk reads only box headers and seeks past each body. It never reads a body into
//! memory, and it never trusts a size from the file: a size that overflows or runs past the
//! end of the file ends the walk.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// A box header with a 32-bit size: four bytes of size and four of type.
const HEADER_LEN: u64 = 8;
/// A box header with a 64-bit largesize (size field `1`): the 8-byte header, then 8 more bytes.
const LARGE_HEADER_LEN: u64 = 16;

/// What [`repair_partial`] did with a `.partial` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repaired {
    /// The file was cut to its last complete fragment and renamed.
    Truncated {
        /// The final name: `clip.mp4`, or `clip (2).mp4` and so on if that name was taken.
        path: PathBuf,
        /// Bytes kept, which is the end of the last complete fragment.
        kept_bytes: u64,
        /// Bytes cut from the end of the file. Zero when the file was already complete.
        dropped_bytes: u64,
    },
    /// The init segment or the first fragment is incomplete. The file was left untouched.
    Unrecoverable,
}

/// Finds where the last complete fragment of an fMP4 file ends.
///
/// The file is an init segment, `ftyp` then `moov`, followed by a `moof` and its `mdat` for
/// each fragment. This walks those top-level boxes and returns the offset just past the last
/// `mdat` that is complete. It stops at the first box that the file ends inside, at a size
/// of 0 (which means "to the end of the file", and our writer never writes one), at a size
/// too small for its own header, or at a size that runs past the end of the file.
///
/// # Errors
/// Only I/O errors from `r`. A damaged file, or one that is not an MP4 at all, gives `Ok(None)`.
pub fn last_complete_fragment_end<R: Read + Seek>(r: &mut R) -> io::Result<Option<u64>> {
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
    Ok(fragment_end)
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

/// Repairs one `.partial` recording: cuts it to its last complete fragment, then renames it
/// to its final name, which is the name without the `.partial` suffix.
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
        let Some(kept_bytes) = last_complete_fragment_end(&mut file)? else {
            return Ok(Repaired::Unrecoverable);
        };
        if kept_bytes < file_len {
            file.set_len(kept_bytes)?;
            file.sync_all()?;
        }
        (kept_bytes, file_len)
    };
    let path = final_path(partial, finished)?;
    fs::rename(partial, &path)?;
    Ok(Repaired::Truncated { path, kept_bytes, dropped_bytes: file_len.saturating_sub(kept_bytes) })
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

    use zc_core::media::{Codec, ContainerFormat, TrackId, TrackInfo, TrackKind};
    use zc_core::testing::SyntheticStream;

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
        let file = recording(3);
        let boxes = boxes(&file);
        let (frag1, frag2, frag3) = (boxes[3].2, boxes[5].2, boxes[7].2);
        for cut in 0..=file.len() as u64 {
            let expected = match cut {
                c if c < frag1 => None,
                c if c < frag2 => Some(frag1),
                c if c < frag3 => Some(frag2),
                _ => Some(frag3),
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
        assert_eq!(
            fs::read(dir.join("clip.mp4")).expect("final file"),
            &file[..frag2_end as usize]
        );
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
        assert_eq!(fs::read(dir.join("clip.mp4")).expect("repaired"), file);
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
        assert_eq!(fs::read(dir.join("clip (2).mp4")).expect("repaired"), file);
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
        assert_eq!(fs::read(dir.join("clip (3).mp4")).expect("repaired"), file);
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
}
