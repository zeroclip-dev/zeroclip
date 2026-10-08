//! # zc-mux — containers and disk I/O
//!
//! Everything between "here are encoded packets" and "bytes on disk":
//!
//! - [`container`]: crash-survivable container writers (fragmented MP4 or MKV,
//!   DESIGN §1). Which format becomes the default is an open question.
//! - [`bitstream`]: Annex-B NAL units, length prefixes, and the `avcC` and AAC
//!   configuration records that MP4 keeps apart from the samples.
//! - [`chunked`]: buffered writes in 8–16 MB blocks, so an HDD's head moves as
//!   little as possible (DESIGN §4, "Disk strategy").
//! - [`pacing`]: gentle write pacing for DVR dumps on spinning disks.
//!
//! Platform-independent. Drive-type detection (is this an HDD?) needs Windows and
//! lives in `zc-win`; its answer is a [`pacing::DriveKind`].

pub mod bitstream;
pub mod chunked;
pub mod container;
pub mod pacing;
