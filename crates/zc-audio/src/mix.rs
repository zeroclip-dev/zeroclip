//! Track 1, Master: a live mix of every enabled source, encoded once more as AAC (D30).
//!
//! The sources write their PCM into a shared [`MixBus`]. One mixer thread wakes every 20 ms,
//! mixes everything up to 200 ms behind real time, and encodes the result, so the first audio
//! track is the mix players and Discord play. The per-source tracks are unchanged, for the editor.
//!
//! The mixer thread joins the multithreaded apartment ([`Apartment`]) and starts Media
//! Foundation ([`MediaFoundation`]), because the AAC encoder is a Media Foundation transform
//! and needs both running on the thread that calls it. See the
//! [Media Foundation API](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/) reference.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zc_core::Hns;
use zc_core::media::TrackId;
use zc_core::mix::{MIX_LAG, MixBus, lock_bus, sample_index, sample_time};
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::{PacketSink, SharedSink};
use zc_core::watchdog::HealthEvent;
use zc_win::com::Apartment;
use zc_win::mf::MediaFoundation;

use crate::capture::{Encode, Mmcss, Worker};

/// How often the mixer wakes up. Its output lags real time by [`MIX_LAG`], which is far more
/// than one tick, so a late wake-up only moves the mix forward a little later.
const TICK: Duration = Duration::from_millis(20);

/// The most frames one encode takes from the bus: one tick of audio.
const CHUNK_FRAMES: usize = 960;

/// The Master track: a mixer thread over a shared bus.
///
/// Sources write into the bus through their own [`zc_core::mix::MixInput`]. This source only
/// reads it and encodes what comes out. Start it after the sources, so the mix point is in
/// place before any source writes (see [`start`](AudioSource::start)).
#[derive(Debug)]
pub struct MasterMix {
    bus: Arc<Mutex<MixBus>>,
    worker: Option<Worker>,
}

impl MasterMix {
    /// A stopped Master track over `bus`, which the sources write into.
    #[must_use]
    pub fn new(bus: Arc<Mutex<MixBus>>) -> Self {
        Self { bus, worker: None }
    }
}

impl AudioSource for MasterMix {
    fn track(&self) -> TrackId {
        TrackId::Master
    }

    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        _health: Emit<HealthEvent>,
    ) -> Result<(), SourceError> {
        self.stop();
        // The mix starts `MIX_LAG` behind now, and before any block a source writes. Sources
        // start first, so their first real blocks come after this point, and the track has
        // no hole at its start. A bus the engine reuses after a stop is already anchored where
        // the last run ended, so `skip_to` also drops the dead time in between (otherwise it is
        // mixed as silence on Track 1 and the sources' writes land beyond the ring).
        let mut scratch = vec![0i16; CHUNK_FRAMES * usize::from(config.channels)];
        skip_to(&self.bus, sample_index(zc_win::clock::now() - MIX_LAG), &mut scratch);
        let (config, bus) = (*config, Arc::clone(&self.bus));
        self.worker = Some(Worker::spawn("zc-master-mix", move |stop, ready| {
            let setup = (|| {
                let apartment = Apartment::multithreaded()?;
                let mf = MediaFoundation::start()?;
                let encode = Encode::new(TrackId::Master, &config)?;
                Ok((apartment, mf, encode))
            })();
            let (_apartment, _mf, mut encode) = match setup {
                Ok(parts) => {
                    ready(Ok(()));
                    parts
                }
                Err(e) => return ready(Err(e)),
            };
            let _mmcss = Mmcss::join();
            let channels = usize::from(config.channels);
            // One tick's worth of mix, allocated once.
            let mut scratch = vec![0i16; CHUNK_FRAMES * channels];
            while !stop.load(Ordering::Acquire) {
                let end = zc_win::clock::now() - MIX_LAG;
                if let Err(e) = mix_pass(&bus, end, channels, &mut scratch, &mut encode, &*sink) {
                    tracing::warn!(%e, "master mix encode failed");
                }
                std::thread::sleep(TICK);
            }
            encode.finish(&*sink);
            tracing::info!(stats = ?lock_bus(&bus).stats(), "master mix stopped");
        })?);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }
}

/// Anchors a fresh bus at sample `to`, or moves a bus that ran before up to it, discarding
/// what the mix would have produced on the way. Runs on `start`, not the hot path.
// ponytail: costs a little CPU per second of gap (hours of gap = seconds here); add a
// `MixBus::reanchor` in zc-core if long idle restarts matter.
fn skip_to(bus: &Mutex<MixBus>, to: i64, scratch: &mut [i16]) {
    lock_bus(bus).anchor(to);
    while lock_bus(bus).mix_until(to, scratch) > 0 {}
}

/// Mixes everything up to `end` (a QPC time) and encodes it as the Master track. The mix is
/// taken `scratch`-sized at a time, and the bus lock is held only for each take, never across
/// the encode or the push to the sink. Allocates nothing.
fn mix_pass(
    bus: &Mutex<MixBus>,
    end: Hns,
    channels: usize,
    scratch: &mut [i16],
    encode: &mut Encode,
    sink: &dyn PacketSink,
) -> Result<(), SourceError> {
    let end = sample_index(end);
    loop {
        let (start, frames) = {
            let mut bus = lock_bus(bus);
            (bus.mixed_until(), bus.mix_until(end, scratch))
        };
        if frames == 0 {
            return Ok(());
        }
        let Some(start) = start else {
            return Ok(());
        };
        let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
        // The timeline sees each block exactly where the previous one ended, so it never pads
        // or skips: the track is continuous by construction.
        encode.push(
            Some(&scratch[..frames * channels]),
            frames_u32,
            Some(sample_time(start)),
            sink,
        )?;
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use zc_core::Packet;
    use zc_core::media::PacketRef;
    use zc_core::mix::MixInput;
    use zc_win::com::Apartment;
    use zc_win::mf::MediaFoundation;

    use super::*;

    #[derive(Default)]
    struct Collect(Mutex<Vec<Packet>>);

    impl PacketSink for Collect {
        fn push(&self, packet: PacketRef<'_>) {
            self.0.lock().unwrap().push(packet.to_packet());
        }
    }

    #[test]
    fn a_restarted_mix_skips_the_gap() {
        let bus = Mutex::new(MixBus::new(1, 2));
        let mut scratch = vec![0i16; 8];
        skip_to(&bus, 100, &mut scratch);
        assert_eq!(lock_bus(&bus).mixed_until(), Some(100), "a fresh bus anchors");
        skip_to(&bus, 5_000, &mut scratch);
        assert_eq!(lock_bus(&bus).mixed_until(), Some(5_000), "a used bus moves up");
        skip_to(&bus, 10, &mut scratch);
        assert_eq!(lock_bus(&bus).mixed_until(), Some(5_000), "never backwards");
    }

    #[test]
    fn master_mix_encodes_a_continuous_track() {
        let (Ok(_apartment), Ok(_mf)) = (Apartment::multithreaded(), MediaFoundation::start())
        else {
            return; // no Media Foundation on this image
        };
        let config = AudioStart { sample_rate: 48_000, channels: 2, bitrate_bps: 192_000 };
        let bus = Arc::new(Mutex::new(MixBus::new(1, 2)));
        lock_bus(&bus).anchor(0);
        // One second of a 440 Hz tone, written in 10 ms blocks as a source writes it.
        let source = MixInput::new(Arc::clone(&bus), 0);
        let mut phase = 0.0f32;
        for k in 0..100 {
            let pcm: Vec<i16> = (0..480)
                .flat_map(|_| {
                    phase += 440.0 * TAU / 48_000.0;
                    let s = (phase.sin() * 8_000.0) as i16;
                    [s, s]
                })
                .collect();
            source.write(480 * k, &pcm);
        }
        let mut encode = Encode::new(TrackId::Master, &config).expect("encoder");
        let sink = Collect::default();
        let mut scratch = vec![0i16; CHUNK_FRAMES * 2];
        mix_pass(&bus, sample_time(48_000), 2, &mut scratch, &mut encode, &sink).expect("mix");
        encode.finish(&sink);

        assert_eq!(lock_bus(&bus).mixed_until(), Some(48_000), "the mix reached the end");
        let packets = sink.0.into_inner().unwrap();
        assert!(packets.len() > 40, "{} packets", packets.len());
        assert!(packets.iter().all(|p| p.meta.track == TrackId::Master));
        let first = packets[0].meta.pts;
        assert!(first < Hns::from_millis(50), "the track starts at {first}");
        // Neighbouring packets touch: no gap longer than 25 ms, and no overlap.
        for pair in packets.windows(2) {
            let gap = pair[1].meta.pts - (pair[0].meta.pts + pair[0].meta.duration);
            assert!(gap.ticks().abs() <= Hns::from_millis(25).ticks(), "gap {gap}");
        }
    }
}
