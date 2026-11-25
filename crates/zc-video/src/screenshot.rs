//! Screenshots (DESIGN §5): one frame, uncompressed, independent of recording.
//!
//! The capture callback only queues a GPU copy into a staging texture
//! ([`staging_copy`]); the caller (the engine thread, off the hot path) maps it and
//! encodes the PNG ([`save_png`]). This is the one place a frame is deliberately
//! copied to system memory.
//!
//! Direct3D 11 GPU memory is not CPU-readable, so the frame goes through a
//! [`D3D11_USAGE_STAGING`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_usage)
//! texture: a GPU-side copy, then [`ID3D11DeviceContext::Map`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11devicecontext-map)
//! for CPU reads.

use std::path::Path;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use zc_core::ports::SourceError;
use zc_win::error::WinResultExt;

/// Copies `texture` into a new CPU-readable staging texture, on the GPU. The copy
/// is queued on the device context and runs in GPU order, so the frame cannot be
/// read before the copy finishes. See
/// [`ID3D11Device::CreateTexture2D`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11device-createtexture2d)
/// and [`ID3D11DeviceContext::CopyResource`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11devicecontext-copyresource).
///
/// # Errors
/// The texture couldn't be created.
pub fn staging_copy(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11Texture2D, SourceError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `desc` is a valid out-parameter; the copy is a GPU command between two
    // textures of the same description on a multithread-protected device.
    unsafe {
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .context("CreateTexture2D (staging)")?;
        let staging =
            staging.ok_or_else(|| SourceError::Other("CreateTexture2D returned nothing".into()))?;
        context.CopyResource(&staging, texture);
        Ok(staging)
    }
}

/// Saves the top-left `size` pixels of a BGRA staging texture as a PNG at `path`.
/// `Map` blocks until the GPU copy is done, and every `Map` is paired with an
/// [`Unmap`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11devicecontext-unmap).
/// The frame is read as BGRA because that is the format WGC delivers.
///
/// # Errors
/// Mapping or file errors.
pub fn save_png(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    size: (u32, u32),
    path: &Path,
) -> Result<(), SourceError> {
    if size.0 == 0 || size.1 == 0 {
        return Err(SourceError::Other("no frame to save: the capture has no size".into()));
    }
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` is CPU-readable; Map waits for the GPU copy to finish.
    unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.context("Map")?;
    let pitch = mapped.RowPitch as usize;
    if mapped.pData.is_null() || pitch < size.0 as usize * 4 {
        // SAFETY: balances the successful Map above.
        unsafe { context.Unmap(staging, 0) };
        return Err(SourceError::Other("the screenshot texture mapped to nothing usable".into()));
    }
    // SAFETY: while mapped, `pData` points at `RowPitch` bytes for each of the
    // texture's rows, and `size` never exceeds the texture.
    let bytes =
        unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * size.1 as usize) };
    let rgba = bgra_to_rgba(bytes, pitch, size.0, size.1);
    // SAFETY: balances the successful Map above; `bytes` isn't used after this.
    unsafe { context.Unmap(staging, 0) };
    image::save_buffer(path, &rgba, size.0, size.1, image::ExtendedColorType::Rgba8)
        .map_err(|e| SourceError::Other(format!("saving {}: {e}", path.display())))
}

/// BGRA rows (with `row_pitch` bytes per row) to tightly packed RGBA. Alpha is
/// forced opaque: DWM's alpha channel isn't reliably 255.
fn bgra_to_rgba(src: &[u8], row_pitch: usize, width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for row in src.chunks(row_pitch).take(height as usize) {
        let Some(row) = row.get(..row_bytes) else { break };
        for px in row.as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_rgba_swaps_channels_and_skips_row_padding() {
        // 2×2 pixels, 12-byte rows (4 bytes of padding each).
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, //
            7, 8, 9, 0, 10, 11, 12, 0, 9, 9, 9, 9,
        ];
        assert_eq!(
            bgra_to_rgba(&src, 12, 2, 2),
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }
}
