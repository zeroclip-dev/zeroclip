//! BGRA → NV12 colour conversion, on the GPU.
//!
//! WGC hands out BGRA; hardware encoders want NV12. The D3D11 video processor
//! converts between them without the frame ever touching system memory.
//!
//! Phase 1 plan: `ID3D11VideoDevice::CreateVideoProcessorEnumerator` (input BGRA at
//! the capture size, output NV12 at the encode size) → `CreateVideoProcessor` →
//! a small pool of NV12 output textures (`D3D11_BIND_RENDER_TARGET`) with output
//! views → per frame, an input view on the captured texture and
//! `ID3D11VideoContext::VideoProcessorBlt`. The pool also decouples the encoder
//! from WGC's two-frame pool, which recycles textures quickly.
//!
//! (For the Phase 0 spike, the Media Foundation sink writer can do this
//! conversion itself when given a D3D manager — simpler, less control.)

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use zc_core::ports::SourceError;

/// Converts captured BGRA textures into NV12 textures for the encoder.
#[derive(Debug)]
pub struct NvConverter {
    _private: (),
}

impl NvConverter {
    /// Sets up a converter from `input` to `output` size (pixels).
    ///
    /// # Errors
    /// The GPU has no video processor for this format pair.
    pub fn new(
        device: &ID3D11Device,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<Self, SourceError> {
        let _ = (device, input, output);
        todo!("Phase 1: video processor + NV12 texture pool")
    }

    /// Converts one frame; returns a pooled NV12 texture.
    ///
    /// # Errors
    /// Device lost / removed.
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<ID3D11Texture2D, SourceError> {
        let _ = bgra;
        todo!("Phase 1: VideoProcessorBlt")
    }
}
