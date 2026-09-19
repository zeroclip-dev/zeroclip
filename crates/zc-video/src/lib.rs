//! # zc-video — the GPU video pipeline (DESIGN §3)
//!
//! ```text
//! Windows.Graphics.Capture ──ID3D11Texture2D (BGRA, VRAM)──► convert (BGRA→NV12 on the GPU)
//!        │ SystemRelativeTime                                   │ ID3D11Texture2D (NV12, VRAM)
//!        ▼                                                      ▼
//!   PTS (raw, 100 ns)                     Media Foundation hardware encoder (async MFT)
//!                                                               │ IMFSample, system memory
//!                                                               ▼
//!                                              PacketRef ──► PacketSink (the recorder)
//! ```
//!
//! The load-bearing promise: **a frame never leaves the GPU until it is encoded**.
//! Capture, colour conversion and encode all share one D3D11 device (with
//! multithread protection on), and textures are handed over by reference.
//!
//! | Module | Job | Phase |
//! |---|---|---|
//! | [`device`] | D3D11 device with video support and multithread protection | 1 |
//! | [`capture`] | WGC capture item, frame pool, session | 1 |
//! | [`convert`] | BGRA → NV12 with the D3D11 video processor | 1 |
//! | [`encoder`] | capability probe (the codec ladder) and the hardware encoder | 1 |
//! | [`screenshot`] | one frame → staging texture → PNG | 1 |
//!
//! Prove the path first in `spikes/phase0-capture-encode`, then move the code here.

pub mod capture;
pub mod convert;
pub mod device;
pub mod encoder;
pub mod screenshot;
mod source;

pub use source::GpuVideoSource;
