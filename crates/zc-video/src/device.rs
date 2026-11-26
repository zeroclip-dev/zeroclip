//! The shared D3D11 device.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{DXGI_ADAPTER_DESC, IDXGIDevice};
use windows::core::Interface;
use zc_core::ports::SourceError;
use zc_win::error::WinResultExt;

/// Creates the hardware D3D11 device every stage of the pipeline shares
/// ([D3D11CreateDevice](https://learn.microsoft.com/windows/win32/api/d3d11/nf-d3d11-d3d11createdevice)).
///
/// - `BGRA_SUPPORT` for WGC's frames, `VIDEO_SUPPORT` for the video processor and
///   the encoder.
/// - Multithread protection on, so the capture callback and the encoder thread
///   can't collide on the immediate context (DESIGN §3).
/// - The default adapter. On a hybrid laptop that's usually the discrete GPU; the
///   adapter is logged so bug reports say which GPU they came from.
///
/// # Errors
/// No hardware device (e.g. Remote Desktop without GPU passthrough).
pub fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), SourceError> {
    let mut device = None;
    let mut context = None;
    // SAFETY: the out-parameters are valid `Option`s that outlive the call, and the
    // feature-level slice is a valid array.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    let (Some(device), Some(context)) = (device, context) else {
        return Err(SourceError::Unsupported("no hardware Direct3D 11 device".into()));
    };
    let multithread = device.cast::<ID3D11Multithread>().context("ID3D11Multithread")?;
    // SAFETY: plain COM call on a live device; the result is the previous state.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let desc = adapter_desc(&device)?;
    let name = String::from_utf16_lossy(&desc.Description);
    tracing::info!(
        gpu = name.trim_end_matches('\0'),
        vendor = format_args!("{:#06x}", desc.VendorId),
        vram_mib = desc.DedicatedVideoMemory / (1 << 20),
        "D3D11 device created"
    );
    Ok((device, context))
}

/// The adapter (GPU) a device lives on. The encoder probe uses its LUID to pick
/// the encoder on the same GPU.
///
/// # Errors
/// A DXGI call failed.
pub fn adapter_desc(device: &ID3D11Device) -> Result<DXGI_ADAPTER_DESC, SourceError> {
    let dxgi = device.cast::<IDXGIDevice>().context("IDXGIDevice")?;
    // SAFETY: plain COM calls on a live device.
    unsafe { dxgi.GetAdapter().and_then(|a| a.GetDesc()) }.context("IDXGIAdapter::GetDesc")
}
