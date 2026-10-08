//! Container writers (DESIGN §1 "Crash-survivable containers", §5 recording modes).
//!
//! Standard MP4 writes its index at the end of the file, so a crash mid-session
//! leaves an unplayable file. Every recording and DVR dump is therefore written as
//! fragmented MP4 (D18): an init segment, then one `moof` and `mdat` pair per GOP.
//! A file cut at a fragment boundary still plays. Export remuxes to plain MP4, a
//! stream copy, so it is still instant.
//!
//! [`create`] returns the fMP4 [`ContainerWriter`]. It is pure Rust over
//! `std::io::Write`, so it can sit on top of the
//! [`ChunkedWriter`](crate::chunked::ChunkedWriter) and be tested with synthetic
//! packets. Matroska is not implemented (D18).

use std::io;

use zc_core::media::{ContainerFormat, PacketRef};

use crate::fmp4::Fmp4Writer;

// The track declarations live in zc-core so the video adapter can report them without
// depending on this crate. Re-exported here so `container::TrackInfo` still resolves.
pub use zc_core::media::{TrackInfo, TrackKind};

/// Writes interleaved packets into a crash-survivable container.
pub trait ContainerWriter: Send {
    /// Appends one packet. Packets must be in non-decreasing PTS order per track.
    ///
    /// The writer rebases time so that the first video keyframe is at zero. Packets
    /// before it are dropped, since a file must start on a keyframe.
    ///
    /// # Errors
    /// I/O errors from the underlying writer. A video PTS that goes backwards, because
    /// B-frames are not supported and the file would be broken.
    fn write_packet(&mut self, packet: PacketRef<'_>) -> io::Result<()>;

    /// Closes the current fragment (an fMP4 `moof` and `mdat` pair) and flushes it to
    /// the writer below.
    ///
    /// The fragment is durable only once the IO stack below syncs it to disk. A
    /// [`ChunkedWriter`](crate::chunked::ChunkedWriter) can hold one chunk (16 MiB) in
    /// RAM, so a ZeroClip crash can lose that much. Recordings sync every fragment,
    /// which bounds the loss to about one second. DVR dumps sync once, at the end. A
    /// game crash loses nothing, since ZeroClip is a separate process.
    ///
    /// # Errors
    /// I/O errors from the underlying writer.
    fn flush_fragment(&mut self) -> io::Result<()>;

    /// Flushes and finalises the file.
    ///
    /// # Errors
    /// I/O errors from the underlying writer.
    fn finish(self: Box<Self>) -> io::Result<()>;
}

/// Creates a writer for `format` over `out`, declaring `tracks` up front.
///
/// Fragmented MP4 is the only format written (D18). The header is written with the first
/// fragment, because it needs parameter sets from the first keyframe.
///
/// # Errors
/// [`io::ErrorKind::Unsupported`] for Matroska, or for a codec that is not written yet.
/// [`io::ErrorKind::InvalidInput`] for a track list the writer cannot use: no video
/// track, a track declared twice, or a kind that does not match its track ID.
pub fn create(
    format: ContainerFormat,
    out: Box<dyn io::Write + Send>,
    tracks: &[TrackInfo],
) -> io::Result<Box<dyn ContainerWriter>> {
    match format {
        ContainerFormat::FragmentedMp4 => Ok(Box::new(Fmp4Writer::new(out, tracks.to_vec())?)),
        ContainerFormat::Matroska => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Matroska output isn't implemented; ZeroClip writes fragmented MP4",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use zc_core::media::{Codec, TrackId};
    use zc_core::testing::SyntheticStream;

    use super::*;

    /// A writer output that the test keeps a handle on, to read the file back.
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

    /// Splits a buffer into its boxes: each box's type and body (without the header).
    fn boxes(mut buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut found = Vec::new();
        while buf.len() >= 8 {
            let size = u32::from_be_bytes(buf[..4].try_into().expect("4 bytes")) as usize;
            let kind: [u8; 4] = buf[4..8].try_into().expect("4 bytes");
            found.push((kind, &buf[8..size]));
            buf = &buf[size..];
        }
        found
    }

    /// The body of the first box of type `kind` directly inside `buf`.
    fn child<'a>(buf: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
        let (_, body) = boxes(buf).into_iter().find(|(k, _)| k == kind).expect("box present");
        body
    }

    #[test]
    fn master_is_declared_before_game_and_mic_whatever_the_input_order() {
        // Players and Discord read only the first audio track, so Master must come first (D30).
        let audio = |track: TrackId, label: &str| TrackInfo {
            track,
            label: label.into(),
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            codec_private: Vec::new(),
        };
        let video = TrackInfo {
            track: TrackId::Video,
            label: "Video".into(),
            kind: TrackKind::Video { codec: Codec::H264, width: 320, height: 240, fps: 60 },
            codec_private: Vec::new(),
        };
        let tracks = [
            audio(TrackId::Mic, "Mic"),
            video,
            audio(TrackId::GameAudio, "Game"),
            audio(TrackId::Master, "Master"),
        ];
        let out = SharedBuf::default();
        let mut writer = create(ContainerFormat::FragmentedMp4, Box::new(out.clone()), &tracks)
            .expect("a valid track list");
        for packet in SyntheticStream::new().with_master().take_secs(1) {
            writer.write_packet(packet.view()).expect("packets are in order");
        }
        writer.finish().expect("finish writes the file");

        let file = out.bytes();
        let moov = child(&file, b"moov");
        let traks: Vec<&[u8]> = boxes(moov)
            .into_iter()
            .filter(|(kind, _)| kind == b"trak")
            .map(|(_, body)| body)
            .collect();
        let names: Vec<String> = traks
            .iter()
            .map(|trak| {
                let hdlr = child(child(trak, b"mdia"), b"hdlr");
                String::from_utf8_lossy(&hdlr[24..hdlr.len() - 1]).into_owned()
            })
            .collect();
        // The tkhd body is version and flags, creation and modification times, then the ID.
        let ids: Vec<u32> = traks
            .iter()
            .map(|trak| {
                u32::from_be_bytes(child(trak, b"tkhd")[12..16].try_into().expect("4 bytes"))
            })
            .collect();
        assert_eq!(names, ["Video", "Master", "Game", "Mic"]);
        assert_eq!(ids, [1, 2, 3, 4], "track IDs follow the declared order");
    }
}
