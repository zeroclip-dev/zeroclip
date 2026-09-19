//! Screenshots (DESIGN §5): one frame, uncompressed, independent of recording.

use std::path::Path;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use zc_core::ports::SourceError;

/// Saves `frame` as a PNG at `path`.
///
/// Phase 1 plan: create a `D3D11_USAGE_STAGING` texture with `D3D11_CPU_ACCESS_READ`
/// → `CopyResource` → `Map` → copy rows (mind `RowPitch`), swapping BGRA → RGBA →
/// encode with the `image` crate on a background thread. This is the one place a
/// frame is deliberately copied to system memory.
///
/// # Errors
/// Mapping or file errors.
pub fn save_png(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &ID3D11Texture2D,
    path: &Path,
) -> Result<(), SourceError> {
    let _ = (device, context, frame, path);
    todo!("Phase 1: staging copy → PNG")
}
