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

use zc_core::media::{Codec, ContainerFormat, PacketRef, TrackId};

/// Codec-specific description of a track, taken from the encoder's output media type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
    /// A video track.
    Video {
        /// Codec.
        codec: Codec,
        /// Width, pixels.
        width: u32,
        /// Height, pixels.
        height: u32,
        /// Nominal frame rate.
        fps: u32,
    },
    /// An AAC audio track.
    Audio {
        /// Sample rate, Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u16,
    },
}

/// One track in a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackInfo {
    /// Which track.
    pub track: TrackId,
    /// Human-readable name such as "Game" or "Microphone", set when the track starts.
    pub label: String,
    /// What it contains.
    pub kind: TrackKind,
    /// Decoder configuration: SPS/PPS(/VPS) for H.264/HEVC, the sequence header for
    /// AV1, or the `AudioSpecificConfig` for AAC. Media Foundation exposes it as
    /// `MF_MT_MPEG_SEQUENCE_HEADER` / `MF_MT_USER_DATA` on the output type.
    pub codec_private: Vec<u8>,
}

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
