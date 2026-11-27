//! Files that survive a power cut.
//!
//! [`File::flush`] does nothing. Written bytes sit in the Windows file cache until Windows
//! decides to write them, so a power cut loses them. [`File::sync_data`] calls
//! `FlushFileBuffers`, which returns only once the disk has the data
//! (<https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers>).
//! That is plain `std`, so zc-mux stays free of the `windows` crate.

use std::fs::File;
use std::io::{self, Write};

/// A [`File`] whose [`flush`](Write::flush) syncs it to disk, or does nothing, depending on
/// how it was made.
///
/// It goes at the bottom of the IO stack: container, then
/// [`ChunkedWriter`](crate::chunked::ChunkedWriter), then
/// [`PacedWriter`](crate::pacing::PacedWriter), then this. The container flushes at the end of
/// every fragment, so a recording syncs once per fragment.
#[derive(Debug)]
pub struct DurableFile {
    file: File,
    sync_on_flush: bool,
}

impl DurableFile {
    /// A recording: every flush syncs, so a crash loses at most the fragment in progress.
    #[must_use]
    pub fn recording(file: File) -> Self {
        Self { file, sync_on_flush: true }
    }

    /// A DVR dump: flushes only hand bytes to the OS. Syncing each fragment would wait on about
    /// 300 syncs for a 5-minute dump, adding 3–6 s on a spinning disk. Call
    /// [`sync`](Self::sync) once at the end instead.
    #[must_use]
    pub fn dump(file: File) -> Self {
        Self { file, sync_on_flush: false }
    }

    /// Syncs the file's data and metadata to disk. On Windows this is `FlushFileBuffers`.
    ///
    /// # Errors
    /// Any error from the operating system.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

impl Write for DurableFile {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.file.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.sync_on_flush { self.file.sync_data() } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use zc_core::media::{Codec, ContainerFormat, Packet, PacketMeta, TrackId};
    use zc_core::time::Hns;

    use super::*;
    use crate::chunked::ChunkedWriter;
    use crate::container::{self, TrackInfo, TrackKind};

    const MIB: usize = 1024 * 1024;

    /// A directory under the temp dir, removed when the test ends, even if it fails.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("zc-mux-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn video() -> TrackInfo {
        TrackInfo {
            track: TrackId::Video,
            label: "Game".into(),
            kind: TrackKind::Video { codec: Codec::H264, width: 320, height: 240, fps: 60 },
            codec_private: Vec::new(),
        }
    }

    /// A video packet at `ms` milliseconds with a 4-byte payload.
    fn packet(ms: i64, keyframe: bool) -> Packet {
        Packet {
            meta: PacketMeta {
                track: TrackId::Video,
                pts: Hns::from_millis(ms),
                duration: Hns::from_millis(500),
                keyframe,
            },
            data: vec![0x65, 1, 2, 3],
        }
    }

    /// The top-level box types of an MP4 file, in order. Panics unless the boxes exactly fill
    /// the bytes, so a torn box or a missing tail fails the test.
    fn top_level_boxes(mut bytes: &[u8]) -> Vec<String> {
        let mut kinds = Vec::new();
        while bytes.len() >= 8 {
            let size =
                usize::try_from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .unwrap();
            assert!((8..=bytes.len()).contains(&size), "a {size}-byte box runs past the end");
            kinds.push(String::from_utf8_lossy(&bytes[4..8]).into_owned());
            bytes = &bytes[size..];
        }
        assert!(bytes.is_empty(), "{} stray bytes at the end", bytes.len());
        kinds
    }

    #[test]
    fn recording_fragments_reach_the_file_before_the_chunk_fills() {
        let scratch = Scratch::new("durable");
        let path = scratch.0.join("recording.mp4");
        let file = File::create_new(&path).unwrap();
        // A 16 MiB chunk that these few packets never fill, so only the flush can move bytes.
        let out = ChunkedWriter::new(DurableFile::recording(file), 16 * MIB);
        let mut writer =
            container::create(ContainerFormat::FragmentedMp4, Box::new(out), &[video()]).unwrap();
        // Keyframes at 0, 1 s and 2 s. The 1 s keyframe closes the first fragment and the 2 s
        // keyframe closes the second. The 2 s keyframe itself waits in the writer.
        for (ms, keyframe) in [(0, true), (500, false), (1000, true), (1500, false), (2000, true)] {
            writer.write_packet(packet(ms, keyframe).view()).unwrap();
        }
        let bytes = fs::read(&path).unwrap();
        assert_eq!(top_level_boxes(&bytes), ["ftyp", "moov", "moof", "mdat", "moof", "mdat"]);
        writer.finish().unwrap();
    }
}
