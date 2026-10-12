//! Windows.Graphics.Capture (WGC): external, no injection, anti-cheat safe.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_ACCESSDENIED, HWND, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_ACCESS_DENIED, IDXGIDevice};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, IInspectable, Interface, factory};
use zc_core::Hns;
use zc_core::ports::{CaptureTarget, SourceError};
use zc_win::error::{WinResultExt, platform_error};

/// WGC's BGRA format. The video processor converts it to NV12 (see [`crate::convert`]).
const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

/// One captured frame, still in VRAM.
#[derive(Debug)]
pub struct CapturedFrame {
    /// The frame. Owned by the frame pool: copy it (on the GPU) before the callback
    /// returns if it must outlive it.
    pub texture: ID3D11Texture2D,
    /// `Direct3D11CaptureFrame::SystemRelativeTime`: QPC-based, 100 ns units.
    pub time: Hns,
    /// Content size in pixels; changes when the window or display mode does.
    pub size: (u32, u32),
}

/// A display Windows reports, for the settings screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The index a [`CaptureTarget::Monitor`] takes (`EnumDisplayMonitors` order).
    pub index: u32,
    /// Label for the UI, such as `Display 1 – 2560×1440`.
    pub name: String,
    /// Whether this is the primary display.
    pub primary: bool,
}

/// Every display, in `EnumDisplayMonitors` order. An empty list if the enumeration fails.
fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<HMONITOR>` that `monitors` passed to
        // EnumDisplayMonitors, and it stays alive and unaliased until that call returns.
        let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        list.push(monitor);
        TRUE
    }
    let mut list: Vec<HMONITOR> = Vec::new();
    // SAFETY: `collect` only runs during this call, while `list` is alive and not
    // otherwise borrowed. A null HDC and clip rect mean "every display". The result is
    // ignored: a partial list is still usable, and an empty one falls back to primary.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(std::ptr::from_mut(&mut list) as isize),
        )
    };
    list
}

/// The display Windows calls primary.
fn primary_monitor() -> HMONITOR {
    // SAFETY: plain FFI call; MONITOR_DEFAULTTOPRIMARY always returns a monitor.
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

/// Geometry and flags of one display.
fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `info` is a live MONITORINFO whose cbSize is set as the call requires.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool();
    ok.then_some(info)
}

/// The display at `index`, if one is connected.
fn monitor_at(index: u32) -> Option<HMONITOR> {
    usize::try_from(index).ok().and_then(|i| monitors().get(i).copied())
}

/// Every connected display, for a monitor picker. Indices match [`CaptureTarget::Monitor`].
#[must_use]
pub fn list_monitors() -> Vec<MonitorInfo> {
    // `MONITORINFOF_PRIMARY` (WindowsAndMessaging) is 1.
    const PRIMARY_FLAG: u32 = 1;
    (0_u32..)
        .zip(monitors())
        .filter_map(|(index, monitor)| {
            let info = monitor_info(monitor)?;
            let width = info.rcMonitor.right.saturating_sub(info.rcMonitor.left);
            let height = info.rcMonitor.bottom.saturating_sub(info.rcMonitor.top);
            Some(MonitorInfo {
                index,
                name: format!("Display {} – {width}×{height}", index + 1),
                primary: info.dwFlags & PRIMARY_FLAG != 0,
            })
        })
        .collect()
}

/// Resolves a [`CaptureTarget`] into a WGC capture item
/// ([IGraphicsCaptureItemInterop](https://learn.microsoft.com/windows/win32/api/windows.graphics.capture.interop/nn-windows-graphics-capture-interop-igraphicscaptureiteminterop)).
///
/// A monitor index that isn't connected falls back to the primary display, so an
/// unplugged screen never stops a recording.
///
/// # Errors
/// WGC unavailable (Windows 10 before 1903) or the target is gone.
pub fn capture_item(target: CaptureTarget) -> Result<GraphicsCaptureItem, SourceError> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(SourceError::Unsupported(
            "Windows.Graphics.Capture needs Windows 10 1903 or later".into(),
        ));
    }
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .context("IGraphicsCaptureItemInterop")?;
    match target {
        CaptureTarget::PrimaryMonitor => {
            // SAFETY: `primary_monitor` returns a valid HMONITOR.
            unsafe { interop.CreateForMonitor(primary_monitor()) }.context("CreateForMonitor")
        }
        CaptureTarget::Monitor(index) => {
            let monitor = monitor_at(index).unwrap_or_else(|| {
                tracing::warn!(index, "no monitor at that index; recording the primary display");
                primary_monitor()
            });
            // SAFETY: `monitor` came from EnumDisplayMonitors or MonitorFromPoint just
            // now. If it has been unplugged since, the call fails cleanly and the caller retries.
            unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")
        }
        CaptureTarget::Window(hwnd) => {
            // SAFETY: an invalid HWND makes the call fail; it can't cause UB.
            unsafe { interop.CreateForWindow(HWND(hwnd as *mut c_void)) }.context("CreateForWindow")
        }
    }
}

/// Wraps the D3D11 device for WinRT. `IDirect3DDevice` isn't `Send`, so the frame
/// handler keeps the (`Send`) `ID3D11Device` and calls this again when it needs it.
fn winrt_device(device: &ID3D11Device) -> windows::core::Result<IDirect3DDevice> {
    let dxgi = device.cast::<IDXGIDevice>()?;
    // SAFETY: plain FFI call on a live device.
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()
}

/// Access denied on the capture path means protected (HDCP/DRM) content. Kept here,
/// not in zc-win, because `E_ACCESSDENIED` means other things elsewhere.
fn capture_error(context: &str, e: &windows::core::Error) -> SourceError {
    if e.code() == DXGI_ERROR_ACCESS_DENIED || e.code() == E_ACCESSDENIED {
        SourceError::Protected
    } else {
        platform_error(context, e)
    }
}

/// A running capture session. Dropping it stops capture.
///
/// - The frame pool is free-threaded, so frames arrive on a thread-pool thread
///   instead of needing a dispatcher; the callback must stay cheap, because it runs
///   on every display refresh (up to 200 times a second).
/// - When the content size changes, the pool is recreated (Microsoft's documented
///   pattern); the converter letterboxes the new size into the fixed encode size.
/// - Protected content: WGC keeps delivering frames (D23). Errors that do say
///   "access denied" become [`SourceError::Protected`], and `GpuVideoSource`
///   retries with backoff.
/// - The item can close under us: a monitor switched off or re-assigned (e.g.
///   Win+P "second screen only") ends capture for good. [`is_closed`](Self::is_closed)
///   reports it, and `GpuVideoSource` reopens the target.
#[derive(Debug)]
pub struct WgcCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    token: i64,
    closed_token: i64,
    closed: Arc<AtomicBool>,
}

impl WgcCapture {
    /// Starts capturing `item`, calling `on_frame` from the frame pool's thread.
    ///
    /// # Errors
    /// Session creation failures; [`SourceError::Protected`] for protected content.
    pub fn start(
        device: &ID3D11Device,
        item: &GraphicsCaptureItem,
        capture_cursor: bool,
        on_frame: impl FnMut(CapturedFrame) + Send + 'static,
    ) -> Result<Self, SourceError> {
        let err = |context: &'static str| move |e| capture_error(context, &e);
        let d3d = winrt_device(device).map_err(err("CreateDirect3D11DeviceFromDXGIDevice"))?;
        let size = item.Size().map_err(err("GraphicsCaptureItem::Size"))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, FORMAT, 2, size)
            .map_err(err("Direct3D11CaptureFramePool::CreateFreeThreaded"))?;

        // `TypedEventHandler` takes `Fn`, so the mutable state lives in a mutex. A
        // poisoned lock skips frames rather than panicking on the capture thread.
        let state = Mutex::new((on_frame, size, device.clone()));
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let Some(pool) = pool.as_ref() else {
                    return Ok(());
                };
                let Ok(mut state) = state.lock() else {
                    return Ok(());
                };
                let (on_frame, last_size, device) = &mut *state;
                let frame = pool.TryGetNextFrame()?;
                let content = frame.ContentSize()?;
                if content != *last_size {
                    pool.Recreate(&winrt_device(device)?, FORMAT, 2, content)?;
                    *last_size = content;
                }
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                // SAFETY: WGC surfaces are D3D11 textures.
                let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: `desc` is a valid out-parameter.
                unsafe { texture.GetDesc(&mut desc) };
                // The frame in hand can predate the Recreate, so clamp to its buffer.
                let width = u32::try_from(content.Width).unwrap_or(0).min(desc.Width);
                let height = u32::try_from(content.Height).unwrap_or(0).min(desc.Height);
                let time = Hns(frame.SystemRelativeTime()?.Duration);
                on_frame(CapturedFrame { texture, time, size: (width, height) });
                frame.Close()
            });
        let token = pool.FrameArrived(&handler).map_err(err("FrameArrived"))?;
        let session = pool.CreateCaptureSession(item).map_err(err("CreateCaptureSession"))?;
        // Windows 11 only; older systems keep the yellow border.
        if let Err(e) = session.SetIsBorderRequired(false) {
            tracing::debug!(%e, "can't hide the capture border");
        }
        if let Err(e) = session.SetIsCursorCaptureEnabled(capture_cursor) {
            tracing::debug!(%e, "can't change cursor capture");
        }
        // WGC's default minimum gap between frames is ~16.7 ms, which on a 200 Hz
        // display gave 50 fps in the Phase 0 spike. Ask for every refresh and let the
        // `FramePacer` decimate. Windows 11 22H2+; older systems keep the default.
        if let Err(e) = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 }) {
            tracing::debug!(%e, "can't lower WGC's minimum update interval");
        }
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let closed_token = item
            .Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                flag.store(true, Ordering::Release);
                Ok(())
            }))
            .map_err(err("GraphicsCaptureItem::Closed"))?;
        session.StartCapture().map_err(err("StartCapture"))?;
        Ok(Self { pool, session, item: item.clone(), token, closed_token, closed })
    }

    /// Whether Windows closed the capture item (its monitor or window went away).
    /// No more frames will come; open a new item.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        // Nothing useful to do if these fail while tearing down.
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
