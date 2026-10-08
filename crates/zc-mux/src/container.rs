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
