//! Manual check for audio capture on a real machine:
//! `cargo run -p zc-audio --example audio_smoke --release -- [secs=5] [game pid]`
//!
//! Runs the mic (always on) and, given a pid, process-loopback game audio, and the Master mix
//! of both; logs how many AAC packets each track produced and their timestamp span. Saves
//! nothing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use zc_core::Hns;
use zc_core::media::{PacketRef, TrackId};
use zc_core::mix::{MixBus, MixInput};
use zc_core::ports::{AudioSource, AudioStart};
use zc_core::sink::PacketSink;

/// Packets, bytes, first and last PTS.
type Totals = (u64, usize, Option<Hns>, Hns);

/// Per-track totals.
#[derive(Default)]
struct Count(Mutex<[Totals; TrackId::COUNT]>);

impl PacketSink for Count {
    fn push(&self, p: PacketRef<'_>) {
        if let Ok(mut c) = self.0.lock() {
            let t = &mut c[p.meta.track.index()];
            t.0 += 1;
            t.1 += p.data.len();
            t.2.get_or_insert(p.meta.pts);
            t.3 = p.meta.end();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().map_or(Ok(5), |s| s.parse())?;
    let pid: Option<u32> = args.next().map(|s| s.parse()).transpose()?;
    let config = AudioStart { sample_rate: 48_000, channels: 2, bitrate_bps: 192_000 };
    let sink = Arc::new(Count::default());
    let health = Arc::new(|_| {});

    // The mic is input 0 of the Master mix, the game input 1; Master starts after both.
    let bus = Arc::new(Mutex::new(MixBus::new(2, 2)));
    let mut mic = zc_audio::MicSource::new(true, true, None)
        .with_mix(Some(MixInput::new(Arc::clone(&bus), 0)));
    match mic.start(&config, sink.clone(), health.clone()) {
        Ok(()) => tracing::info!("mic started"),
        Err(e) => tracing::warn!(%e, "mic unavailable"),
    }
    let mut game =
        zc_audio::GameAudioSource::new().with_mix(Some(MixInput::new(Arc::clone(&bus), 1)));
    game.set_target_process(pid);
    game.start(&config, sink.clone(), health.clone())?;
    let mut master = zc_audio::MasterMix::new(bus);
    master.start(&config, sink.clone(), health)?;

    std::thread::sleep(Duration::from_secs(secs));
    mic.stop();
    game.stop();
    master.stop();
    let counts = sink.0.lock().map_err(|_| "poisoned")?;
    for track in [TrackId::Master, TrackId::GameAudio, TrackId::Mic] {
        let (packets, bytes, first, last) = counts[track.index()];
        let span = first.map_or(0.0, |f| (last - f).as_secs_f64());
        tracing::info!(?track, packets, bytes, span_s = span, "track totals");
    }
    Ok(())
}
