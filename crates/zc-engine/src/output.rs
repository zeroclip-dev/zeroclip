//! The output stack: where clips and recordings go, and how their bytes reach the disk
//! (DESIGN §1 "Crash-survivable containers", §4 "Disk strategy").
//!
//! Each file is written through one stack, built and driven on an IO thread:
//!
//! ```text
//! container (fMP4) → ChunkedWriter (whole chunks) → PacedWriter (HDD pacing)
//!                  → DurableFile (fsync on flush) → File
//! ```
//!
//! Nothing here runs on the capture, encode or audio threads.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use time::macros::format_description;
use tracing::warn;
use zc_core::dvr::DvrClip;
use zc_core::media::{ContainerFormat, TrackInfo};
use zc_core::settings::Settings;
use zc_mux::chunked::ChunkedWriter;
use zc_mux::container::{self, ContainerWriter};
use zc_mux::durable::DurableFile;
use zc_mux::pacing::{DriveKind, PacedWriter, WritePacer};

/// What the output stack needs from the settings, resolved once per file.
#[derive(Clone, Debug)]
pub(crate) struct OutputConfig {
    /// Folder that new files go in.
    pub(crate) dir: PathBuf,
    /// Container to write. Only fragmented MP4 is written (D18).
    pub(crate) format: ContainerFormat,
    /// Bytes per buffered write (DESIGN §4: 8–16 MiB).
    pub(crate) chunk_bytes: usize,
    /// Throttle for the writes. Unlimited unless the drive is a spinning disk.
    pub(crate) pacer: WritePacer,
}

impl OutputConfig {
    /// The output settings for a file in `dir`, on a drive of kind `drive`.
    ///
    /// The caller probes the drive kind. Pacing applies only to spinning disks, and only while
    /// `pace_hdd_writes` is on (DESIGN §4).
    pub(crate) fn from_settings(settings: &Settings, dir: PathBuf, drive: DriveKind) -> Self {
        let storage = &settings.storage;
        let pacer = if storage.pace_hdd_writes {
            WritePacer::for_drive(drive, u64::from(storage.hdd_write_mb_per_sec) * 1_000_000)
        } else {
            WritePacer::unlimited()
        };
        let format = match settings.recording.container {
            ContainerFormat::Matroska => {
                // Settings::problems flags such a file when it is saved. Writing MP4 beats failing.
                warn!("Matroska isn't written; this file goes out as fragmented MP4 (D18)");
                ContainerFormat::FragmentedMp4
            }
            format => format,
        };
        Self {
            dir,
            format,
            chunk_bytes: (storage.write_chunk_mib.clamp(1, 64) as usize) << 20,
            pacer,
        }
    }
}

/// Creates a new file in `dir` named `ZeroClip <label> <local time>.<ext>`, and returns its
/// path and handle.
///
/// The time is local, so the name reads the way the user expects. Windows forbids colons, so
/// the time uses hyphens. A name already in use gets ` (2)`, ` (3)` and so on: a clip is never
/// overwritten.
///
/// # Errors
/// Any error creating the folder or the file.
pub(crate) fn create_file(dir: &Path, label: &str, ext: &str) -> io::Result<(PathBuf, File)> {
    // The local offset can't always be read (on Unix, in a multithreaded process), so fall back
    // to UTC rather than fail the save.
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    create_file_at(dir, label, ext, now)
}

/// [`create_file`] with the time given, so a test can name two files in the same second.
fn create_file_at(
    dir: &Path,
    label: &str,
    ext: &str,
    when: OffsetDateTime,
) -> io::Result<(PathBuf, File)> {
    fs::create_dir_all(dir)?;
    let format = format_description!("[year]-[month]-[day] [hour]-[minute]-[second]");
    let stamp = when.format(&format).map_err(io::Error::other)?;
    let mut copy = 1;
    loop {
        let suffix = if copy == 1 { String::new() } else { format!(" ({copy})") };
        let path = dir.join(format!("ZeroClip {label} {stamp}{suffix}.{ext}"));
        match File::create_new(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => copy += 1,
            Err(e) => return Err(e),
        }
    }
}

/// When a file's bytes are synced to disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durability {
    /// Sync at every fragment, so a crash loses about one fragment (D18). Recordings.
    PerFragment,
    /// Sync once, when the file is finished. DVR dumps.
    AtFinish,
}

/// Opens the container writer over `file`: fMP4 in whole chunks, paced for the drive, and
/// synced as `durability` says.
///
/// # Errors
/// The container's refusal of the track list, such as a list with no video track.
pub(crate) fn open_writer(
    file: File,
    tracks: &[TrackInfo],
    cfg: &OutputConfig,
    durability: Durability,
) -> io::Result<Box<dyn ContainerWriter>> {
    let durable = match durability {
        Durability::PerFragment => DurableFile::recording(file),
        Durability::AtFinish => DurableFile::dump(file),
    };
    let paced = PacedWriter::new(durable, cfg.pacer);
    container::create(cfg.format, Box::new(ChunkedWriter::new(paced, cfg.chunk_bytes)), tracks)
}

/// Writes a DVR clip into `file`, finishes the container and syncs the file. Runs on an IO
/// thread, after the buffer's snapshot has copied the clip out.
///
/// # Errors
/// The writer's errors, such as a video PTS that goes backwards, or the final sync failing.
pub(crate) fn write_clip(
    clip: &DvrClip,
    file: File,
    tracks: &[TrackInfo],
    cfg: &OutputConfig,
) -> io::Result<()> {
    // The writer chain owns `file` and closes it inside `finish`, so sync through a second
    // handle to the same file. `FlushFileBuffers` covers every handle to it.
    let handle = file.try_clone()?;
    let mut out = open_writer(file, tracks, cfg, Durability::AtFinish)?;
    for packet in &clip.packets {
        out.write_packet(packet.view())?;
    }
    out.finish()?;
    handle.sync_all()
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use zc_core::dvr::{DvrBuffer, DvrConfig};
    use zc_core::media::{Codec, TrackId, TrackKind};
    use zc_core::testing::SyntheticStream;
    use zc_core::time::Hns;

    use super::*;

    /// A directory under the temp dir, removed when the test ends, even if it fails.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("zc-engine-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// One H.264 video track and two AAC tracks, as a recording declares them.
    fn tracks() -> Vec<TrackInfo> {
        vec![
            TrackInfo {
                track: TrackId::Video,
                label: "Game".into(),
                kind: TrackKind::Video { codec: Codec::H264, width: 1920, height: 1080, fps: 60 },
                codec_private: Vec::new(),
            },
            TrackInfo {
                track: TrackId::GameAudio,
                label: "Game audio".into(),
                kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
                codec_private: Vec::new(),
            },
            TrackInfo {
                track: TrackId::Mic,
                label: "Mic".into(),
                kind: TrackKind::Audio { sample_rate: 48_000, channels: 1 },
                codec_private: Vec::new(),
            },
        ]
    }

    fn config(dir: &Path) -> OutputConfig {
        OutputConfig::from_settings(&Settings::default(), dir.to_path_buf(), DriveKind::Ssd)
    }

    #[test]
    fn writes_a_synthetic_clip() {
        let scratch = Scratch::new("clip");
        let cfg = config(&scratch.0);
        let mut buffer =
            DvrBuffer::new(DvrConfig::for_window(Hns::from_secs(3), 20_000_000, 192_000));
        for packet in SyntheticStream::new().take_secs(3) {
            buffer.push(packet.view()).unwrap();
        }
        let clip = buffer.snapshot(Hns::from_secs(3)).unwrap();

        let (path, file) = create_file(&cfg.dir, "Clip", cfg.format.extension()).unwrap();
        write_clip(&clip, file, &tracks(), &cfg).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[4..8], b"ftyp");
    }

    #[test]
    fn never_overwrites() {
        let scratch = Scratch::new("names");
        let when = datetime!(2026-10-08 14:03:27 +2);
        let (first, _) = create_file_at(&scratch.0, "Clip", "mp4", when).unwrap();
        let (second, _) = create_file_at(&scratch.0, "Clip", "mp4", when).unwrap();
        assert_eq!(first.file_name().unwrap(), "ZeroClip Clip 2026-10-08 14-03-27.mp4");
        assert_ne!(first, second);
        assert!(second.to_string_lossy().ends_with(" (2).mp4"), "{second:?}");
    }

    #[test]
    fn a_recording_reaches_the_disk_before_finish() {
        // A crash never gets to finish, so each fragment must already be on disk. Nothing else
        // moves the bytes: the 16 MiB chunk is far from full.
        let scratch = Scratch::new("recording");
        let cfg = config(&scratch.0);
        let (path, file) = create_file(&cfg.dir, "Recording", cfg.format.extension()).unwrap();
        let mut out = open_writer(file, &tracks(), &cfg, Durability::PerFragment).unwrap();
        for packet in SyntheticStream::new().take_secs(3) {
            out.write_packet(packet.view()).unwrap();
        }
        // The keyframes at 1 s and 2 s have closed two fragments.
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 8, "nothing reached the disk before finish");
        assert_eq!(&bytes[4..8], b"ftyp");
        out.finish().unwrap();
    }
}
