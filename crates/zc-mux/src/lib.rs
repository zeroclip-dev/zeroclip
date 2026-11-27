//! # zc-mux — containers and disk I/O
//!
//! Everything between "here are encoded packets" and "bytes on disk":
//!
//! - [`container`]: crash-survivable container writers. ZeroClip writes fragmented MP4;
//!   the MP4 boxes live in the private `fmp4` module.
//! - [`bitstream`]: Annex-B NAL units, length prefixes, and the `avcC`, `hvcC` and AAC
//!   configuration records that MP4 keeps apart from the samples.
//! - [`chunked`]: buffered writes in 8–16 MB blocks, so an HDD's head moves as
//!   little as possible.
//! - [`durable`]: files whose flushes sync to disk, so a recording survives a power cut.
//! - [`pacing`]: gentle write pacing for DVR dumps on spinning disks.
//! - [`repair`]: startup repair of a recording that a crash left unfinished.
//!
//! Platform-independent. Drive-type detection (is this an HDD?) needs Windows and
//! lives in `zc-win`; its answer is a [`pacing::DriveKind`].

pub mod bitstream;
pub mod chunked;
pub mod container;
pub mod durable;
mod fmp4;
pub mod pacing;
pub mod repair;
