//! The shared D3D11 device.

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use zc_core::ports::SourceError;

/// Creates the hardware D3D11 device every stage of the pipeline shares.
///
/// Phase 1 plan:
/// 1. `D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, …,
///    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, …)`
///    — BGRA for WGC, VIDEO for the video processor and the encoder.
/// 2. `device.cast::<ID3D10Multithread>()?.SetMultithreadProtected(true)` so the
///    capture callback and the encoder can't collide on the immediate context
///    (DESIGN §3). `ID3D11Multithread` is the same interface, newer name.
/// 3. Log the adapter (`IDXGIDevice::GetAdapter` → `GetDesc1`) so bug reports say
///    which GPU they came from.
///
/// # Errors
/// No hardware device (e.g. Remote Desktop without GPU passthrough).
pub fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), SourceError> {
    todo!("Phase 1: create the device (move it from the Phase 0 spike)")
}
