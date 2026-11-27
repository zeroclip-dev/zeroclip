//! # zc-editor — the lightning editor
//!
//! Speed comes from never re-encoding: trims and stitches are FFmpeg *stream
//! copies*, so they finish in well under a second. The price is that cuts land on
//! keyframes — which is why the encoder uses a fixed 1–2 s keyframe interval, and
//! why the scrub bar shows the snap points.
//!
//! FFmpeg is run as a separate `ffmpeg.exe` process, never linked: that keeps its
//! LGPL/GPL build choices out of this crate's licensing.
//!
//! - [`snap`]: where a cut can actually happen.
//! - [`concat`](mod@concat): can these clips be stitched without re-encoding?
//! - [`waveform`]: RMS peaks for the timeline.
//! - [`ffmpeg`]: locating and driving `ffmpeg.exe`.

pub mod concat;
pub mod ffmpeg;
pub mod snap;
pub mod waveform;
