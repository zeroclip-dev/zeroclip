//! Manual check for the GPU video pipeline on a real machine:
//! `cargo run -p zc-video --example smoke`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use zc_core::ports::CaptureTarget;
use zc_video::capture::{WgcCapture, capture_item};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let _apartment = zc_win::com::Apartment::multithreaded()?;
    let (device, _context) = zc_video::device::create_device()?;

    let item = capture_item(CaptureTarget::PrimaryMonitor)?;
    let frames = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&frames);
    let mut last_size = (0, 0);
    let start = Instant::now();
    let capture = WgcCapture::start(&device, &item, false, move |frame| {
        counter.fetch_add(1, Ordering::Relaxed);
        if frame.size != last_size {
            tracing::info!(size = ?frame.size, "capture size");
            last_size = frame.size;
        }
    })?;
    std::thread::sleep(Duration::from_secs(5));
    drop(capture);
    let n = frames.load(Ordering::Relaxed);
    tracing::info!(frames = n, fps = n as f64 / start.elapsed().as_secs_f64(), "capture done");
    Ok(())
}
