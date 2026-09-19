//! Windows.Graphics.Capture (WGC): external, no injection, anti-cheat safe.

use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use zc_core::Hns;
use zc_core::ports::{CaptureTarget, SourceError};

/// One captured frame, still in VRAM.
#[derive(Debug)]
pub struct CapturedFrame {
    /// The frame. Owned by the frame pool: copy it (on the GPU) before the next
    /// frame arrives if it must outlive the callback.
    pub texture: ID3D11Texture2D,
    /// `Direct3D11CaptureFrame::SystemRelativeTime` — QPC-based, 100 ns units.
    pub time: Hns,
    /// Content size in pixels; changes when the window or display mode does.
    pub size: (u32, u32),
}

/// Resolves a [`CaptureTarget`] into a WGC capture item.
///
/// Phase 1 plan: `windows::core::factory::<GraphicsCaptureItem,
/// IGraphicsCaptureItemInterop>()`, then `CreateForMonitor(hmonitor)` (from
/// `MonitorFromPoint`/`EnumDisplayMonitors`) or `CreateForWindow(hwnd)`.
///
/// # Errors
/// WGC unavailable (Windows 10 before 1903) or the target is gone.
pub fn capture_item(target: CaptureTarget) -> Result<GraphicsCaptureItem, SourceError> {
    let _ = target;
    todo!("Phase 1: IGraphicsCaptureItemInterop::CreateForMonitor / CreateForWindow")
}

/// A running capture session.
///
/// Phase 1 plan:
/// 1. Wrap the device for WinRT: `CreateDirect3D11DeviceFromDXGIDevice` →
///    `IDirect3DDevice`.
/// 2. `Direct3D11CaptureFramePool::CreateFreeThreaded(&device,
///    DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size()?)` — free-threaded,
///    so frames arrive on a pool thread instead of needing a dispatcher.
/// 3. `pool.FrameArrived(...)`: `TryGetNextFrame` → `Surface` →
///    `IDirect3DDxgiInterfaceAccess::GetInterface::<ID3D11Texture2D>()`, plus
///    `SystemRelativeTime` → [`CapturedFrame`].
/// 4. `CreateCaptureSession(&item)`; turn off the yellow border
///    (`SetIsBorderRequired(false)`, Windows 11) and the cursor unless enabled;
///    `StartCapture()`.
/// 5. On `ContentSize` changes, `Recreate` the pool; the encoder is re-created
///    for the new size.
/// 6. `DXGI_ERROR_ACCESS_DENIED` = protected content: back off with
///    `zc_core::backoff::Backoff::hdcp()` and emit placeholder frames (DESIGN §3).
#[derive(Debug)]
pub struct WgcCapture {
    #[expect(dead_code, reason = "scaffold: Phase 1")]
    pool: Direct3D11CaptureFramePool,
    #[expect(dead_code, reason = "scaffold: Phase 1")]
    session: GraphicsCaptureSession,
}

impl WgcCapture {
    /// Starts capturing `item`, calling `on_frame` from the frame pool's thread.
    ///
    /// # Errors
    /// Session creation failures.
    pub fn start(
        device: &ID3D11Device,
        item: &GraphicsCaptureItem,
        on_frame: impl FnMut(CapturedFrame) + Send + 'static,
    ) -> Result<Self, SourceError> {
        let _ = (device, item, on_frame);
        todo!("Phase 1: frame pool + session (see the plan above)")
    }
}
