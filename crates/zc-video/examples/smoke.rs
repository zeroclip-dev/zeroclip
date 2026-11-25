//! Manual check for the GPU video pipeline on a real machine (the Phase 1
//! done-when harness):
//!
//! `cargo run -p zc-video --example smoke --release -- [secs=10] [h264|hevc|av1] [fps=60]`
//!
//! Writes the raw encoded stream to `capture.h264` / `capture.hevc` / `capture.obu`
//! and a screenshot to `smoke.png`, then logs packets, keyframe spacing, drops and
//! faults. Play the stream with `ffplay capture.hevc` or VLC.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use zc_core::Hns;
use zc_core::media::{Codec, PacketRef};
use zc_core::ports::{CaptureTarget, VideoSource, VideoStart};
use zc_core::sink::PacketSink;
use zc_core::watchdog::HealthEvent;
use zc_video::GpuVideoSource;

/// Sends every packet to the main thread (allocating is fine in an example).
struct ChannelSink(mpsc::Sender<(Vec<u8>, bool, Hns)>);

impl PacketSink for ChannelSink {
    fn push(&self, packet: PacketRef<'_>) {
        let _ = self.0.send((packet.data.to_vec(), packet.meta.keyframe, packet.meta.pts));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().map_or(Ok(10), |s| s.parse())?;
    let (codec, file) = match args.next().as_deref() {
        None | Some("hevc") => (Codec::Hevc, "capture.hevc"),
        Some("h264") => (Codec::H264, "capture.h264"),
        Some("av1") => (Codec::Av1, "capture.obu"),
        Some(other) => return Err(format!("unknown codec {other}").into()),
    };
    let fps: u32 = args.next().map_or(Ok(60), |s| s.parse())?;

    let (tx, rx) = mpsc::channel();
    let [drops, faults] = [(); 2].map(|()| Arc::new(AtomicU64::new(0)));
    let (d, f) = (Arc::clone(&drops), Arc::clone(&faults));
    let health = Arc::new(move |event: HealthEvent| match event {
        HealthEvent::FrameDropped => _ = d.fetch_add(1, Ordering::Relaxed),
        HealthEvent::EncoderFault { hresult } => {
            tracing::error!(hresult = format_args!("{hresult:#010x}"), "encoder fault");
            f.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    });
    let config = VideoStart {
        target: CaptureTarget::PrimaryMonitor,
        codec,
        fps,
        min_bitrate_bps: 5_000_000,
        max_bitrate_bps: 20_000_000,
        keyframe_interval: Hns::from_secs(1),
        capture_cursor: true,
        resolution: Default::default(),
    };

    let mut source = GpuVideoSource::new();
    let used = source.start(&config, Arc::new(ChannelSink(tx)), health)?;
    tracing::info!(codec = used.name(), secs, fps, "recording");
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(secs / 2));
    match source.screenshot(Path::new("smoke.png")) {
        Ok(()) => tracing::info!("screenshot saved to smoke.png"),
        Err(e) => tracing::warn!(%e, "screenshot failed"),
    }
    std::thread::sleep(Duration::from_secs(secs - secs / 2));
    source.stop();
    let elapsed = start.elapsed().as_secs_f64();

    let mut out = std::io::BufWriter::new(std::fs::File::create(file)?);
    let (mut packets, mut bytes, mut keyframes) = (0u64, 0usize, Vec::new());
    for (data, keyframe, pts) in rx.try_iter() {
        out.write_all(&data)?;
        packets += 1;
        bytes += data.len();
        if keyframe {
            keyframes.push(pts);
        }
    }
    out.flush()?;
    let gaps: Vec<f64> = keyframes.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
    let mean_gap = gaps.iter().sum::<f64>() / gaps.len().max(1) as f64;
    let max_gap = gaps.iter().copied().fold(0.0, f64::max);
    tracing::info!(
        file,
        packets,
        fps = packets as f64 / elapsed,
        mib = bytes as f64 / f64::from(1 << 20),
        keyframes = keyframes.len(),
        mean_keyframe_gap_s = mean_gap,
        max_keyframe_gap_s = max_gap,
        drops = drops.load(Ordering::Relaxed),
        faults = faults.load(Ordering::Relaxed),
        "done"
    );

    // Players (VLC especially) often won't open a raw stream with no timestamps, so
    // wrap it in an MP4 when ffmpeg is installed. Phase 3 writes real MP4s itself.
    let mp4 = format!("{file}.mp4");
    let format = match codec {
        Codec::Hevc => "hevc",
        Codec::H264 => "h264",
        Codec::Av1 => "obu",
    };
    let fps = fps.to_string();
    let mut args = vec!["-v", "error", "-y", "-r", &fps, "-f", format, "-i", file, "-c", "copy"];
    if codec == Codec::Hevc {
        args.extend(["-tag:v", "hvc1"]); // Windows players and browsers want hvc1
    }
    args.push(&mp4);
    match std::process::Command::new("ffmpeg").args(&args).status() {
        Ok(status) if status.success() => tracing::info!(file = mp4, "wrapped for playback"),
        Ok(status) => tracing::warn!(%status, "ffmpeg couldn't wrap the stream"),
        Err(_) => tracing::warn!("ffmpeg isn't on PATH; play the raw stream with ffplay"),
    }
    Ok(())
}
