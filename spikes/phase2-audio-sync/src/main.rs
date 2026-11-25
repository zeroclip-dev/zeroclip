//! # Phase 2 spike: does audio stay in sync over a long session?
//!
//! Phase 2's done-when is "a 30-minute recording has no measurable A/V drift", but
//! the muxer arrives in Phase 3. So this probe runs video, game audio (process
//! loopback of `--pid`) and the mic, and every minute logs each track's *skew*:
//! where the track's timeline has got to, minus the QPC clock now. All three
//! share the QPC timeline, so if every skew stays flat, nothing drifts.
//!
//! `cargo run -p spike-phase2 --release -- --pid <game pid> [--minutes 30] [--write]`
//!
//! `--write` also saves `game.aac` and `mic.aac` (ADTS, playable in VLC). Without it
//! nothing is stored: only timestamps are measured.

#![allow(clippy::print_stdout, reason = "throwaway spike")]

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zc_core::Hns;
use zc_core::media::{Codec, PacketRef, TrackId};
use zc_core::ports::{AudioSource, AudioStart, CaptureTarget, VideoSource, VideoStart};
use zc_core::sink::PacketSink;
use zc_win::clock;

#[derive(Default)]
struct Probe {
    /// Per track: where its timeline has got to (pts + duration of the last packet).
    last_end: [Option<Hns>; TrackId::COUNT],
    files: Option<[BufWriter<File>; 2]>,
}

/// A 7-byte ADTS header for one raw AAC-LC frame, 48 kHz stereo, so players can
/// read the file.
fn adts_header(payload: usize) -> [u8; 7] {
    let len = payload + 7;
    let (profile, freq_index, channels) = (1u8, 3u8, 2u8); // LC, 48 kHz, stereo
    [
        0xFF,
        0xF1,
        (profile << 6) | (freq_index << 2) | (channels >> 2),
        ((channels & 3) << 6) | ((len >> 11) as u8 & 0x03),
        (len >> 3) as u8,
        ((len as u8 & 7) << 5) | 0x1F,
        0xFC,
    ]
}

struct ProbeSink(Mutex<Probe>);

impl PacketSink for ProbeSink {
    fn push(&self, p: PacketRef<'_>) {
        let Ok(mut probe) = self.0.lock() else { return };
        probe.last_end[p.meta.track.index()] = Some(p.meta.end());
        // Blocking file I/O here is acceptable only in a spike: the real recorder
        // never touches the disk on the hot path.
        let slot = match p.meta.track {
            TrackId::GameAudio => Some(0),
            TrackId::Mic => Some(1),
            _ => None,
        };
        if let (Some(i), Some(files)) = (slot, probe.files.as_mut()) {
            let _ = files[i].write_all(&adts_header(p.data.len()));
            let _ = files[i].write_all(p.data);
        }
    }
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let pid: u32 = arg("--pid").context("--pid <game pid> is required")?.parse()?;
    let minutes: u64 = arg("--minutes").map_or(Ok(30), |m| m.parse())?;
    let write = std::env::args().any(|a| a == "--write");
    let _apartment = zc_win::com::Apartment::multithreaded()?;

    let files = write
        .then(|| -> Result<_> {
            Ok([
                BufWriter::new(File::create("game.aac")?),
                BufWriter::new(File::create("mic.aac")?),
            ])
        })
        .transpose()?;
    let sink = Arc::new(ProbeSink(Mutex::new(Probe { files, ..Probe::default() })));
    let health = Arc::new(|_| {});
    let audio = AudioStart { sample_rate: 48_000, channels: 2, bitrate_bps: 192_000 };

    let mut game = zc_audio::GameAudioSource::new();
    game.set_target_process(Some(pid));
    game.start(&audio, sink.clone(), health.clone())?;
    let mut mic = zc_audio::MicSource::new(true, true, None);
    if let Err(e) = mic.start(&audio, sink.clone(), health.clone()) {
        tracing::warn!(%e, "no microphone; measuring game audio and video only");
    }
    let mut video = zc_video::GpuVideoSource::new();
    let codec = video.start(
        &VideoStart {
            target: CaptureTarget::PrimaryMonitor,
            codec: Codec::Hevc,
            fps: 60,
            min_bitrate_bps: 5_000_000,
            max_bitrate_bps: 20_000_000,
            keyframe_interval: Hns::from_secs(1),
            capture_cursor: false,
            resolution: Default::default(),
        },
        sink.clone(),
        health,
    )?;
    tracing::info!(pid, minutes, write, codec = codec.name(), "probing");

    let skews = || -> [Option<f64>; TrackId::COUNT] {
        let now = clock::now();
        let probe = sink.0.lock().map(|p| p.last_end).unwrap_or_default();
        probe.map(|end| end.map(|e| (e - now).as_secs_f64() * 1000.0))
    };
    let start = Instant::now();
    let mut first: Option<[Option<f64>; TrackId::COUNT]> = None;
    for minute in 1..=minutes {
        std::thread::sleep(
            (start + Duration::from_secs(60 * minute)).saturating_duration_since(Instant::now()),
        );
        let now = skews();
        let base = *first.get_or_insert(now);
        let drift = |t: TrackId| match (now[t.index()], base[t.index()]) {
            (Some(n), Some(b)) => format!("{:+.1} ms", n - b),
            _ => "no data".into(),
        };
        tracing::info!(
            minute,
            video = drift(TrackId::Video),
            game = drift(TrackId::GameAudio),
            mic = drift(TrackId::Mic),
            "drift since minute 1"
        );
    }
    game.stop();
    mic.stop();
    video.stop();
    if let Some(files) = sink.0.lock().ok().and_then(|mut p| p.files.take()) {
        for mut f in files {
            f.flush()?;
        }
    }
    Ok(())
}
