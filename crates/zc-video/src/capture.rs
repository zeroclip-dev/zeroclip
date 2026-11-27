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
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{E_ACCESSDENIED, HWND, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_DENIED, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow,
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

/// What WGC gives when Windows HDR is on and we ask for the real picture: 16-bit float scRGB,
/// where 1.0 is 80 nits. [`crate::tonemap`] turns it into SDR.
const FORMAT_HDR: DirectXPixelFormat = DirectXPixelFormat::R16G16B16A16Float;

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
    /// `Some(nits)` when `texture` is float scRGB from an HDR display, with Windows' SDR white
    /// level; `None` for an ordinary BGRA8 frame.
    pub sdr_white: Option<f32>,
}

/// A display Windows reports, for the settings screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The index a [`CaptureTarget::Monitor`] takes (`EnumDisplayMonitors` order).
    pub index: u32,
    /// Label for the UI, such as `Display 1, 2560×1440`.
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
                name: format!("Display {}, {width}×{height}", index + 1),
                primary: info.dwFlags & PRIMARY_FLAG != 0,
            })
        })
        .collect()
}

/// The display `target` is on.
fn target_monitor(target: CaptureTarget) -> HMONITOR {
    match target {
        CaptureTarget::PrimaryMonitor => primary_monitor(),
        CaptureTarget::Monitor(index) => monitor_at(index).unwrap_or_else(primary_monitor),
        // SAFETY: an invalid HWND makes the call return the nearest display; it can't cause UB.
        CaptureTarget::Window(hwnd) => unsafe {
            MonitorFromWindow(HWND(hwnd as *mut c_void), MONITOR_DEFAULTTONEAREST)
        },
    }
}

/// Whether Windows HDR is switched on for `monitor` (its DXGI output is in HDR10 mode).
fn is_hdr(monitor: HMONITOR) -> bool {
    // SAFETY: plain DXGI enumeration; every interface is released when it drops.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return false };
        for adapter in (0..).map_while(|i| factory.EnumAdapters1(i).ok()) {
            for output in (0..).map_while(|i| adapter.EnumOutputs(i).ok()) {
                if output.GetDesc().is_ok_and(|d| d.Monitor == monitor)
                    && let Ok(output) = output.cast::<IDXGIOutput6>()
                    && let Ok(desc) = output.GetDesc1()
                {
                    return desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                }
            }
        }
    }
    false
}

/// The "SDR content brightness" Windows applies to `monitor`, in nits.
/// [DISPLAYCONFIG_SDR_WHITE_LEVEL](https://learn.microsoft.com/windows/win32/api/wingdi/ns-wingdi-displayconfig_sdr_white_level)
/// counts in 1/1000ths of 80 nits.
fn sdr_white_nits(monitor: HMONITOR) -> Option<f32> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).ok()?;
    // SAFETY: MONITORINFOEXW starts with a MONITORINFO and its cbSize says which one it is.
    if !unsafe { GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info).cast()) }.as_bool() {
        return None;
    }
    let (mut paths, mut modes) = (0_u32, 0_u32);
    // SAFETY: valid out-parameters; the vectors are sized by the call before they are filled.
    unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes).ok().ok()?;
        let mut path_buf = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut mode_buf = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths,
            path_buf.as_mut_ptr(),
            &mut modes,
            mode_buf.as_mut_ptr(),
            None,
        )
        .ok()
        .ok()?;
        for path in path_buf.iter().take(paths as usize) {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = u32::try_from(std::mem::size_of_val(&source)).ok()?;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || source.viewGdiDeviceName != info.szDevice
            {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            white.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
            white.header.size = u32::try_from(std::mem::size_of_val(&white)).ok()?;
            white.header.adapterId = path.targetInfo.adapterId;
            white.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut white.header) == 0 {
                return Some(white.SDRWhiteLevel as f32 * 80.0 / 1000.0);
            }
        }
    }
    None
}

/// `Some(SDR white in nits)` when Windows HDR is on for the display `target` is on, so capture
/// should take the float scRGB frame and tone-map it; `None` for an ordinary SDR display.
#[must_use]
pub fn hdr_sdr_white(target: CaptureTarget) -> Option<f32> {
    let monitor = target_monitor(target);
    // 200 nits is Windows' usual default if the level can't be read.
    is_hdr(monitor).then(|| sdr_white_nits(monitor).unwrap_or(200.0))
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

/// WGC's minimum gap between frames for a target of `fps`, in 100 ns ticks: half a frame period.
/// WGC rounds the gap up to whole refreshes, so anything near a full period would skip a refresh
/// it needs (12.5 ms on a 100 Hz display means every 20 ms: 50 fps). Half a period always leaves
/// at least one refresh per frame slot.
fn min_update_ticks(fps: u32) -> i64 {
    Hns::PER_SECOND / i64::from(fps.max(1)) / 2
}

/// A running capture session. Dropping it stops capture.
///
/// - The frame pool is free-threaded, so frames arrive on a thread-pool thread
///   instead of needing a dispatcher; the callback must stay cheap, because it runs
///   on every display refresh (up to 200 times a second).
/// - When the content size changes, the pool is recreated (Microsoft's documented
///   pattern); the converter letterboxes the new size into the fixed encode size.
/// - Protected content: WGC keeps delivering frames. Errors that do say
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
    /// Starts capturing `item` for a target of `fps`, calling `on_frame` from the frame pool's thread.
    ///
    /// # Errors
    /// Session creation failures; [`SourceError::Protected`] for protected content.
    pub fn start(
        device: &ID3D11Device,
        item: &GraphicsCaptureItem,
        capture_cursor: bool,
        hdr_white: Option<f32>,
        fps: u32,
        on_frame: impl FnMut(CapturedFrame) + Send + 'static,
    ) -> Result<Self, SourceError> {
        let err = |context: &'static str| move |e| capture_error(context, &e);
        let d3d = winrt_device(device).map_err(err("CreateDirect3D11DeviceFromDXGIDevice"))?;
        let size = item.Size().map_err(err("GraphicsCaptureItem::Size"))?;
        let format = if hdr_white.is_some() { FORMAT_HDR } else { FORMAT };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, format, 2, size)
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
                // Every path closes the frame, or the pool (2 buffers) runs dry and capture
                // stalls; a failed step only skips this frame.
                let handled: windows::core::Result<()> = (|| {
                    let content = frame.ContentSize()?;
                    // A minimised window reports 0 (or 1) pixels; the pool can't be resized
                    // to that, so wait for it to come back.
                    if content.Width < 2 || content.Height < 2 {
                        return Ok(());
                    }
                    if content != *last_size {
                        pool.Recreate(&winrt_device(device)?, format, 2, content)?;
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
                    on_frame(CapturedFrame {
                        texture,
                        time,
                        size: (width, height),
                        sdr_white: hdr_white,
                    });
                    Ok(())
                })();
                let closed = frame.Close();
                handled.and(closed)
            });
        let token = pool.FrameArrived(&handler).map_err(|e| {
            let _ = pool.Close();
            capture_error("FrameArrived", &e)
        })?;
        // Past this point a failure must undo what exists so far, or the pool and its
        // FrameArrived handler (which owns the frame callback) outlive the failed start.
        let undo = |session: Option<&GraphicsCaptureSession>, closed_token: Option<i64>| {
            if let Some(closed_token) = closed_token {
                let _ = item.RemoveClosed(closed_token);
            }
            let _ = pool.RemoveFrameArrived(token);
            if let Some(session) = session {
                let _ = session.Close();
            }
            let _ = pool.Close();
        };
        let session = match pool.CreateCaptureSession(item) {
            Ok(session) => session,
            Err(e) => {
                undo(None, None);
                return Err(capture_error("CreateCaptureSession", &e));
            }
        };
        // Windows 11 only; older systems keep the yellow border.
        if let Err(e) = session.SetIsBorderRequired(false) {
            tracing::debug!(%e, "can't hide the capture border");
        }
        if let Err(e) = session.SetIsCursorCaptureEnabled(capture_cursor) {
            tracing::debug!(%e, "can't change cursor capture");
        }
        // WGC's default minimum gap between frames is ~16.7 ms, which on a 200 Hz display gave 50 fps
        // in the Phase 0 spike. Ask for half a frame period: the `FramePacer` still picks the frames
        // on its grid, but WGC no longer copies every refresh (200+ GPU copies a second next to a
        // game). Windows 11 22H2+; older systems keep the default. See
        // <https://learn.microsoft.com/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval>.
        if let Err(e) = session.SetMinUpdateInterval(TimeSpan { Duration: min_update_ticks(fps) }) {
            tracing::debug!(%e, "can't lower WGC's minimum update interval");
        }
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let closed_token = match item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                flag.store(true, Ordering::Release);
                Ok(())
            }),
        ) {
            Ok(token) => token,
            Err(e) => {
                undo(Some(&session), None);
                return Err(capture_error("GraphicsCaptureItem::Closed", &e));
            }
        };
        if let Err(e) = session.StartCapture() {
            undo(Some(&session), Some(closed_token));
            return Err(capture_error("StartCapture", &e));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_gap_is_half_a_frame() {
        assert_eq!(min_update_ticks(60), 83_333);
        assert_eq!(min_update_ticks(30), 166_666);
        // A zero fps setting can't divide by zero.
        assert_eq!(min_update_ticks(0), Hns::PER_SECOND / 2);
    }
}
