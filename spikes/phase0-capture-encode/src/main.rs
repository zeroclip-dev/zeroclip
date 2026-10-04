//! # Phase 0 spike — prove the load-bearing assumption (DESIGN §11)
//!
//! **WGC frame → Media Foundation hardware encoder → MP4 on disk, with no
//! system-memory round trip for the frame.** Everything else in ZeroClip is
//! conventional engineering; this is the part that has to work, so it gets proven
//! first, in the smallest possible program.
//!
//! Done when: `cargo run -p spike-phase0 --release` writes a 10-second
//! hardware-encoded `spike-phase0.mp4` that plays in VLC and a browser, and Task
//! Manager shows the GPU's "Video Encode" engine busy while CPU stays near idle.
//!
//! This is throwaway code — direct and quick is fine. When it works, Phase 1 moves
//! the pieces into `crates/zc-video` properly (with the colour conversion and the
//! encoder driven directly instead of through the sink writer).
//!
//! Useful references: the `windows` crate docs (search a function name at
//! <https://microsoft.github.io/windows-docs-rs/>), and Microsoft's docs for
//! Windows.Graphics.Capture and the Media Foundation Sink Writer.

#![allow(clippy::print_stdout, reason = "the spike reports its results on the console")]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_POINTER, HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFSinkWriter, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_SINK_WRITER_DISABLE_THROTTLING, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Video, MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoInterlace_Progressive,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HSTRING, IInspectable, Interface, factory};
use zc_win::com::Apartment;
use zc_win::mf::MediaFoundation;

const OUTPUT: &str = "spike-phase0.mp4";
const DURATION: Duration = Duration::from_secs(10);
const FPS: u32 = 60;
const BITRATE_BPS: u32 = 20_000_000;

fn main() -> Result<()> {
    // COM/WinRT and Media Foundation must be initialised on this thread first.
    let _apartment = Apartment::multithreaded()?;
    let _media_foundation = MediaFoundation::start()?;

    let (device, context) = create_device()?;
    let item = capture_item()?;
    let size = item.Size()?;
    let width = u32::try_from(size.Width)?;
    let height = u32::try_from(size.Height)?;
    println!("Capturing the primary monitor at {width}x{height} for {DURATION:?}…");

    let (writer, stream) = sink_writer(&device, width, height)?;
    let stats = record(&device, &context, &item, &writer, stream)?;
    // SAFETY: the writer is valid and every sample has been written.
    unsafe { writer.Finalize()? };

    let bytes = std::fs::metadata(OUTPUT)?.len();
    println!(
        "Wrote {OUTPUT}: {} frames, {:.1} MB, frame interval min/median/average/max =          {:.2}/{:.2}/{:.2}/{:.2} ms, {:.0}% of frames under 8 ms after the previous one",
        stats.frames,
        bytes as f64 / 1_000_000.0,
        stats.min_interval_ms,
        stats.median_interval_ms,
        stats.average_interval_ms,
        stats.max_interval_ms,
        stats.fast_share * 100.0
    );
    Ok(())
}

/// What happened during the recording.
struct Stats {
    frames: u64,
    average_interval_ms: f64,
    min_interval_ms: f64,
    median_interval_ms: f64,
    max_interval_ms: f64,
    /// Fraction of intervals shorter than 8 ms, i.e. frames arriving at 125+ fps.
    fast_share: f64,
}

/// **Step 1** — a hardware D3D11 device that both WGC and Media Foundation can use.
///
/// The immediate context is shared by the capture callback and the encoder, so the
/// device is made multithread-protected (DESIGN §3). `ID3D11Multithread` has the same
/// IID as the `ID3D10Multithread` this used to name.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
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
        )?;
    }
    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no context")?;
    // SAFETY: plain COM call on a live device. The return value is the previous
    // protection state, which we don't need.
    let _ = unsafe { device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };
    Ok((device, context))
}

/// **Step 2** — what to capture: the primary monitor.
fn capture_item() -> Result<GraphicsCaptureItem> {
    // SAFETY: plain FFI call; the primary monitor always exists.
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: `monitor` is a valid HMONITOR obtained just above.
    Ok(unsafe { interop.CreateForMonitor(monitor)? })
}

/// **Step 3** — a sink writer that encodes on the GPU. Returns the writer and its
/// video stream index.
///
/// The writer is told to use our D3D11 device and hardware transforms, so with an
/// ARGB32 input it inserts a GPU colour converter and a hardware H.264 encoder.
fn sink_writer(device: &ID3D11Device, width: u32, height: u32) -> Result<(IMFSinkWriter, u32)> {
    let frame_size = (u64::from(width) << 32) | u64::from(height);
    let frame_rate = (u64::from(FPS) << 32) | 1;
    let square_pixels = (1u64 << 32) | 1;

    // SAFETY: every call below is plain COM/FFI on objects created in this function;
    // out-parameters are valid `Option`s, and the device manager and attributes stay
    // alive until the writer holds its own references.
    unsafe {
        // Lets the sink writer's transforms (colour converter, encoder) share our device.
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.context("MFCreateDXGIDeviceManager returned no manager")?;
        manager.ResetDevice(device, token)?;

        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 3)?;
        let attributes = attributes.context("MFCreateAttributes returned no attributes")?;
        attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(OUTPUT), None, &attributes)?;

        // What goes into the file: H.264 (try HEVC in P0.4).
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE_BPS)?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        output.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square_pixels)?;
        let stream = writer.AddStream(&output)?;

        // What we feed it: WGC's B8G8R8A8, which Media Foundation calls ARGB32.
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square_pixels)?;
        writer.SetInputMediaType(stream, &input, None)?;
        writer.BeginWriting()?;
        Ok((writer, stream))
    }
}

/// **Step 4** — capture for [`DURATION`], handing every frame to the writer.
///
/// The sink writer isn't `Send`, so the capture callback only copies each frame on
/// the GPU and sends the copy (plus its timestamp) to this thread, which writes it.
/// That's the Phase 1 design too.
fn record(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    item: &GraphicsCaptureItem,
    writer: &IMFSinkWriter,
    stream: u32,
) -> Result<Stats> {
    // SAFETY: plain FFI call on a live device.
    let d3d: IDirect3DDevice =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)? }.cast()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &d3d,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        item.Size()?,
    )?;

    let (tx, rx) = mpsc::channel::<(ID3D11Texture2D, i64)>();
    let (handler_device, handler_context) = (device.clone(), context.clone());
    let handler =
        TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
            let Some(pool) = pool.as_ref() else {
                return Ok(());
            };
            let frame = pool.TryGetNextFrame()?;
            let time = frame.SystemRelativeTime()?.Duration;
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            // SAFETY: the surface is a D3D11 texture, which is what WGC hands out.
            let source: ID3D11Texture2D = unsafe { access.GetInterface()? };

            // The pool recycles its two textures, so copy on the GPU into one we own
            // before the frame goes back (DESIGN §3). A texture per frame is fine for a
            // spike; Phase 1 uses a ring.
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: `desc` is a valid out-parameter; the copy is a GPU-to-GPU command
            // on a multithread-protected device.
            let copy = unsafe {
                source.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_DEFAULT;
                desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                desc.CPUAccessFlags = 0;
                desc.MiscFlags = 0;
                let mut copy = None;
                handler_device.CreateTexture2D(&desc, None, Some(&mut copy))?;
                let copy: ID3D11Texture2D =
                    copy.ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;
                handler_context.CopyResource(&copy, &source);
                copy
            };
            frame.Close()?;
            // The receiver is gone once recording has finished; that's fine.
            let _ = tx.send((copy, time));
            Ok(())
        });
    pool.FrameArrived(&handler)?;
    let session = pool.CreateCaptureSession(item)?;
    session.StartCapture()?;

    let start = Instant::now();
    let mut first_time = None;
    let mut last_time = 0i64;
    let mut frames = 0u64;
    let mut intervals = Vec::<i64>::new();
    while start.elapsed() < DURATION {
        let Ok((texture, time)) = rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let first = *first_time.get_or_insert(time);
        if frames > 0 {
            intervals.push(time - last_time);
        }
        last_time = time;

        // SAFETY: plain COM calls on objects created here; the texture is a valid
        // `ID3D11Texture2D` and the buffer wraps it without copying.
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)?;
            let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(length)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time - first)?;
            sample.SetSampleDuration(10_000_000 / i64::from(FPS))?;
            writer.WriteSample(stream, &sample)?;
        }
        frames += 1;
    }
    session.Close()?;
    pool.Close()?;

    // Spike-only statistics: how steady is the frame cadence? (100 ns ticks to ms.)
    intervals.sort_unstable();
    let ms = |ticks: i64| ticks as f64 / 10_000.0;
    let at = |i: usize| intervals.get(i).copied().map_or(0.0, ms);
    let average_interval_ms = if intervals.is_empty() {
        0.0
    } else {
        ms(intervals.iter().sum::<i64>()) / intervals.len() as f64
    };
    let fast = intervals.iter().filter(|&&t| ms(t) < 8.0).count();
    Ok(Stats {
        frames,
        average_interval_ms,
        min_interval_ms: at(0),
        median_interval_ms: at(intervals.len() / 2),
        max_interval_ms: at(intervals.len().saturating_sub(1)),
        fast_share: fast as f64 / intervals.len().max(1) as f64,
    })
}
