//! Manual check for the GPU video pipeline on a real machine:
//! `cargo run -p zc-video --example smoke`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use zc_core::ports::CaptureTarget;
use zc_video::capture::{WgcCapture, capture_item};
use zc_video::convert::NvConverter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let _apartment = zc_win::com::Apartment::multithreaded()?;
    let (device, _context) = zc_video::device::create_device()?;

    let item = capture_item(CaptureTarget::PrimaryMonitor)?;
    let size = item.Size()?;
    let input = (u32::try_from(size.Width)?, u32::try_from(size.Height)?);
    let mut converter = NvConverter::new(&device, input, (input.0 & !1, input.1 & !1))?;

    let [frames, converted, failed] = [(); 3].map(|()| Arc::new(AtomicU64::new(0)));
    let counters = (Arc::clone(&frames), Arc::clone(&converted), Arc::clone(&failed));
    let mut last_size = (0, 0);
    let start = Instant::now();
    let capture = WgcCapture::start(&device, &item, false, move |frame| {
        counters.0.fetch_add(1, Ordering::Relaxed);
        if frame.size != last_size {
            tracing::info!(size = ?frame.size, "capture size");
            last_size = frame.size;
        }
        match converter.convert(&frame.texture, frame.size) {
            Ok(_) => counters.1.fetch_add(1, Ordering::Relaxed),
            Err(e) => {
                tracing::warn!(%e, "convert failed");
                counters.2.fetch_add(1, Ordering::Relaxed)
            }
        };
    })?;
    std::thread::sleep(Duration::from_secs(5));
    drop(capture);
    let n = frames.load(Ordering::Relaxed);
    tracing::info!(
        frames = n,
        converted = converted.load(Ordering::Relaxed),
        failed = failed.load(Ordering::Relaxed),
        fps = n as f64 / start.elapsed().as_secs_f64(),
        "capture done"
    );
    Ok(())
}
