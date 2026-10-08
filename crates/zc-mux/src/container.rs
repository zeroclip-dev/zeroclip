//! Container writers (DESIGN §1 "Crash-survivable containers", §5 recording modes).
//!
//! Standard MP4 writes its index at the end of the file, so a crash mid-session
//! leaves an unplayable file. Every direct-to-disk recording therefore uses a
//! fragmented container that stays playable at any truncation point: fragmented
//! MP4 or MKV. Export remuxes to plain MP4 — a stream copy, so still instant.
//!
//! Phase 3 decides fMP4 vs MKV (docs/OPEN_QUESTIONS.md) and implements one
//! [`ContainerWriter`] for it — pure Rust over `std::io::Write`, so it can sit on top
//! of the [`ChunkedWriter`](crate::chunked::ChunkedWriter) and be tested with
//! synthetic packets.

use std::io;

use zc_core::media::{ContainerFormat, PacketRef};

// The track declarations live in zc-core so the video adapter can report them without
// depending on this crate. Re-exported here so `container::TrackInfo` still resolves.
pub use zc_core::media::{TrackInfo, TrackKind};

/// Writes interleaved packets into a crash-survivable container.
pub trait ContainerWriter: Send {
    /// Appends one packet. Packets must be in non-decreasing PTS order per track.
    ///
    /// # Errors
    /// I/O errors from the underlying writer.
    fn write_packet(&mut self, packet: PacketRef<'_>) -> io::Result<()>;

    /// Closes the current fragment (fMP4 `moof`+`mdat`, or MKV cluster). After this
    /// returns, everything written so far plays even if the process dies.
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
/// # Errors
/// I/O errors writing the header.
pub fn create(
    format: ContainerFormat,
    out: Box<dyn io::Write + Send>,
    tracks: &[TrackInfo],
) -> io::Result<Box<dyn ContainerWriter>> {
    let _ = (format, out, tracks);
    todo!("Phase 3: implement the chosen container (fMP4 or MKV)")
}
