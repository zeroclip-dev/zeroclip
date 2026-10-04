//! BGRA → NV12 colour conversion, on the GPU.
//!
//! WGC hands out BGRA; hardware encoders want NV12. The D3D11 video processor
//! ([ID3D11VideoContext::VideoProcessorBlt](https://learn.microsoft.com/windows/win32/api/d3d11/nf-d3d11-id3d11videocontext-videoprocessorblt))
//! converts between them without the frame ever touching system memory.
//!
//! The encode size is fixed when capture starts. When the capture size changes
//! (a resolution change, alt-tab out of exclusive fullscreen) the converter scales
//! and letterboxes into the same NV12 size, so the encoder and the stream never
//! restart.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;
use zc_core::ports::SourceError;
use zc_win::error::WinResultExt;

/// NV12 output textures in flight between the converter and the encoder.
// ponytail: a fixed ring without release tracking; switch to MFCreateTrackedSample if
// an encoder ever holds more than a few inputs and frames tear.
const RING: usize = 8;

/// Converts captured BGRA textures into NV12 textures for the encoder.
#[derive(Debug)]
pub struct NvConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    textures: Vec<ID3D11Texture2D>,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
    next: usize,
    input: (u32, u32),
    output: (u32, u32),
}

impl NvConverter {
    /// Sets up a converter from `input` to `output` size (pixels). `output` must be
    /// even in both dimensions (NV12 chroma is subsampled 2×2).
    ///
    /// # Errors
    /// The GPU has no video processor for this format pair.
    pub fn new(
        device: &ID3D11Device,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<Self, SourceError> {
        let video_device = device.cast::<ID3D11VideoDevice>().context("ID3D11VideoDevice")?;
        // SAFETY: plain COM call on a live device.
        let video_context = unsafe { device.GetImmediateContext() }
            .and_then(|c| c.cast::<ID3D11VideoContext1>())
            .context("ID3D11VideoContext1")?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: output.0,
            Height: output.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let textures = (0..RING)
            .map(|_| {
                let mut texture = None;
                // SAFETY: `desc` is valid and `texture` is a valid out-parameter.
                unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                    .context("CreateTexture2D (NV12)")?;
                texture.ok_or_else(|| SourceError::Other("CreateTexture2D returned nothing".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (enumerator, processor, outputs) =
            Self::processor(&video_device, &video_context, &textures, input, output)?;
        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            textures,
            outputs,
            next: 0,
            input,
            output,
        })
    }

    /// Builds the video processor for one input size, plus the output views (which
    /// are tied to the enumerator), and sets the stream state.
    fn processor(
        video_device: &ID3D11VideoDevice,
        context: &ID3D11VideoContext1,
        textures: &[ID3D11Texture2D],
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<
        (ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor, Vec<ID3D11VideoProcessorOutputView>),
        SourceError,
    > {
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: plain COM calls with valid descriptors; every out-parameter is a
        // valid `Option` that outlives its call.
        unsafe {
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content)
                .context("CreateVideoProcessorEnumerator")?;
            let support = enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
                .context("CheckVideoProcessorFormat")?;
            if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                return Err(SourceError::Unsupported(
                    "the GPU's video processor can't output NV12".into(),
                ));
            }
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .context("CreateVideoProcessor")?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let outputs = textures
                .iter()
                .map(|texture| {
                    let mut view = None;
                    video_device
                        .CreateVideoProcessorOutputView(
                            texture,
                            &enumerator,
                            &view_desc,
                            Some(&mut view),
                        )
                        .context("CreateVideoProcessorOutputView")?;
                    view.ok_or_else(|| SourceError::Other("no output view".into()))
                })
                .collect::<Result<Vec<_>, _>>()?;

            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // sRGB in, BT.709 limited range out; the encoder's colour tags must match.
            context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            let source = RECT { left: 0, top: 0, right: input.0 as i32, bottom: input.1 as i32 };
            let target = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            context.VideoProcessorSetStreamDestRect(
                &processor,
                0,
                true,
                Some(&fit_rect(input, output)),
            );
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 },
                },
            };
            context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            Ok((enumerator, processor, outputs))
        }
    }

    /// Converts one frame of `size` pixels into the next NV12 texture of the ring;
    /// returns its slot (see [`textures`](Self::textures)).
    ///
    /// # Errors
    /// Device lost or removed.
    pub fn convert(
        &mut self,
        bgra: &ID3D11Texture2D,
        size: (u32, u32),
    ) -> Result<usize, SourceError> {
        if size != self.input {
            tracing::info!(from = ?self.input, to = ?size, "capture size changed; letterboxing");
            (self.enumerator, self.processor, self.outputs) = Self::processor(
                &self.video_device,
                &self.video_context,
                &self.textures,
                size,
                self.output,
            )?;
            self.input = size;
        }
        let slot = self.next;
        let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: plain COM call; `view` is a valid out-parameter.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                bgra,
                &self.enumerator,
                &view_desc,
                Some(&mut view),
            )
        }
        .context("CreateVideoProcessorInputView")?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(view),
            ..Default::default()
        };
        // SAFETY: the processor, output view and stream are valid for the call.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.outputs[slot],
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: `pInputSurface` is dropped exactly once, here. Skipping this leaks
        // an input view (and a texture reference) every frame.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("VideoProcessorBlt")?;
        self.next = (slot + 1) % RING;
        Ok(slot)
    }

    /// The NV12 ring. The encoder pre-creates one sample per texture.
    #[must_use]
    pub fn textures(&self) -> &[ID3D11Texture2D] {
        &self.textures
    }

    /// The fixed encode size.
    #[must_use]
    pub fn output_size(&self) -> (u32, u32) {
        self.output
    }
}

/// Where `src` lands inside `dst` when scaled to fit with its aspect ratio kept:
/// centred, with black bars. Sizes and offsets are even, as NV12 needs.
fn fit_rect(src: (u32, u32), dst: (u32, u32)) -> RECT {
    let (sw, sh) = (u64::from(src.0.max(1)), u64::from(src.1.max(1)));
    let (dw, dh) = (u64::from(dst.0), u64::from(dst.1));
    let (w, h) = if sw * dh >= dw * sh { (dw, sh * dw / sw) } else { (sw * dh / sh, dh) };
    let (w, h) = (w & !1, h & !1);
    let (x, y) = (((dw - w) / 2) & !1, ((dh - h) / 2) & !1);
    // Sizes come from u32 pixel counts well under i32::MAX.
    let i = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    RECT { left: i(x), top: i(y), right: i(x + w), bottom: i(y + h) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(rect: RECT) -> (i32, i32, i32, i32) {
        (rect.left, rect.top, rect.right, rect.bottom)
    }

    #[test]
    fn fit_rect_letterboxes_and_pillarboxes() {
        assert_eq!(r(fit_rect((2560, 1440), (2560, 1440))), (0, 0, 2560, 1440));
        assert_eq!(r(fit_rect((1280, 1024), (1920, 1080))), (284, 0, 1634, 1080));
        assert_eq!(r(fit_rect((2560, 1080), (1920, 1080))), (0, 134, 1920, 944));
    }
}
