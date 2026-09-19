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
//! This is throwaway code — direct and quick is fine. Fill in the four steps in
//! order; each doc comment lists the calls you need. When it works, Phase 1 moves
//! the pieces into `crates/zc-video` properly (with the colour conversion and the
//! encoder driven directly instead of through the sink writer).
//!
//! Useful references: the `windows` crate docs (search a function name at
//! <https://microsoft.github.io/windows-docs-rs/>), and Microsoft's docs for
//! Windows.Graphics.Capture and the Media Foundation Sink Writer.

#![allow(clippy::print_stdout, reason = "the spike reports its results on the console")]

use std::time::Duration;

use anyhow::Result;
use windows::Graphics::Capture::GraphicsCaptureItem;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Media::MediaFoundation::IMFSinkWriter;
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
        "Wrote {OUTPUT}: {} frames, {:.1} MB, average frame interval {:.2} ms",
        stats.frames,
        bytes as f64 / 1_000_000.0,
        stats.average_interval_ms
    );
    Ok(())
}

/// What happened during the recording.
struct Stats {
    frames: u64,
    average_interval_ms: f64,
}

/// **Step 1** — a hardware D3D11 device that both WGC and Media Foundation can use.
///
/// - `D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
///   D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
///   Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]), D3D11_SDK_VERSION,
///   Some(&mut device), None, Some(&mut context))`
/// - `device.cast::<ID3D10Multithread>()?.SetMultithreadProtected(true)`: the capture
///   callback and the encoder both use the immediate context (DESIGN §3).
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    todo!("Step 1: create the D3D11 device (see the doc comment)")
}

/// **Step 2** — what to capture: the primary monitor.
///
/// - `MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)`
/// - `windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?`
/// - `interop.CreateForMonitor(monitor)`
fn capture_item() -> Result<GraphicsCaptureItem> {
    todo!("Step 2: GraphicsCaptureItem for the primary monitor")
}

/// **Step 3** — a sink writer that encodes on the GPU. Returns the writer and its
/// video stream index.
///
/// - `MFCreateDXGIDeviceManager(&mut token, &mut manager)`, then
///   `manager.ResetDevice(device, token)`.
/// - `MFCreateAttributes`, then set `MF_SINK_WRITER_D3D_MANAGER` = the manager,
///   `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` = 1, `MF_SINK_WRITER_DISABLE_THROTTLING` = 1.
/// - `MFCreateSinkWriterFromURL(OUTPUT, None, &attributes)`.
/// - Output type (`MFCreateMediaType`): `MF_MT_MAJOR_TYPE` = `MFMediaType_Video`,
///   `MF_MT_SUBTYPE` = `MFVideoFormat_H264` (try HEVC once it works),
///   `MF_MT_AVG_BITRATE` = [`BITRATE_BPS`], `MF_MT_INTERLACE_MODE` = progressive,
///   `MF_MT_FRAME_SIZE` = `(width << 32) | height`, `MF_MT_FRAME_RATE` = `(FPS << 32) | 1`.
///   `AddStream` gives the stream index.
/// - Input type: the same, but `MF_MT_SUBTYPE` = `MFVideoFormat_ARGB32` (WGC's BGRA).
///   With hardware transforms on, the sink writer adds a GPU colour converter.
///   `SetInputMediaType`, then `BeginWriting`.
fn sink_writer(device: &ID3D11Device, width: u32, height: u32) -> Result<(IMFSinkWriter, u32)> {
    let _ = (device, width, height, OUTPUT, BITRATE_BPS);
    todo!("Step 3: configure the sink writer")
}

/// **Step 4** — capture for [`DURATION`], handing every frame to the writer.
///
/// - Wrap the device for WinRT: `CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?
///   .cast::<IDirect3DDevice>()?`.
/// - `Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized,
///   2, item.Size()?)?`.
/// - `pool.FrameArrived(&TypedEventHandler::new(move |pool, _| { … }))`. In the handler:
///   - `TryGetNextFrame()`, then `frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface::<ID3D11Texture2D>()?`;
///   - copy it into a texture you own (`CopyResource`) — the pool recycles its two
///     textures while the writer may still be reading;
///   - `MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &copy, 0, false)` → `MFCreateSample` →
///     `AddBuffer`; `SetSampleTime(frame.SystemRelativeTime()?.Duration - first_frame_time)`;
///     `SetSampleDuration(10_000_000 / FPS)`; `writer.WriteSample(stream, &sample)`.
///   - If the compiler says the writer isn't `Send`, send the textures over a channel
///     to the main thread and write there — that's the Phase 1 design anyway.
/// - `pool.CreateCaptureSession(item)?.StartCapture()?`, sleep for [`DURATION`], then
///   `Close()` the session and the pool.
/// - Count frames and time between them: that's your first performance number.
fn record(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    item: &GraphicsCaptureItem,
    writer: &IMFSinkWriter,
    stream: u32,
) -> Result<Stats> {
    let _ = (device, context, item, writer, stream, FPS, DURATION);
    todo!("Step 4: capture and write frames")
}
