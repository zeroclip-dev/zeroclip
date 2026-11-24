//! Per-app audio (D26): one app's process tree on its own track, via WASAPI process loopback.
//!
//! The Windows calls are in `loopback`: `loopback::activate` asks for process loopback
//! with [`ActivateAudioInterfaceAsync`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync),
//! and `loopback::setup` joins the MTA and starts Media Foundation for the track's encoder.
//! The exe-to-pid lookup is the injected resolver, so this module does no process queries itself.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use zc_core::Hns;
use zc_core::media::TrackId;
use zc_core::mix::MixInput;
use zc_core::ports::{AudioSource, AudioStart, Emit, SourceError};
use zc_core::sink::SharedSink;
use zc_core::watchdog::HealthEvent;

use crate::capture::{Mmcss, Stream, Worker, pcm16_format};
use crate::loopback;

/// How often the capture thread asks which process to follow: a relaunched app is picked up
/// within this long.
const RESOLVE_EVERY: Hns = Hns::from_secs(2);

/// Paces the capture thread's process lookups. Pure: the caller passes the time, so the
/// cadence is testable without a clock.
#[derive(Debug)]
struct ResolveTimer {
    next: Hns,
    every: Hns,
}

impl ResolveTimer {
    fn new(every: Hns) -> Self {
        Self { next: Hns::ZERO, every }
    }

    /// `true` once per `every`; the first call is always due. After a long pause it fires once
    /// and restarts the interval from `now`, without catching up.
    fn due(&mut self, now: Hns) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.every;
        true
    }
}

/// Maps an exe name (`Discord.exe`) to the root pid of its process tree, or `None` if the app
/// isn't running.
type PidResolver = Box<dyn Fn(&str) -> Option<u32> + Send + 'static>;

/// Per-app audio (D26): the process tree of one app (for example `Discord.exe`), captured with
/// WASAPI process loopback in include mode onto its own track. The track has no holes: while
/// the app isn't running, the thread writes silence, as the game track does.
///
/// Unlike [`GameAudioSource`](crate::GameAudioSource) there is no whole-system fallback. Before
/// Windows 11 build 20348 this source logs once and stays silent, because an app track must
/// never record everything.
pub struct AppAudioSource {
    track: TrackId,
    exe: String,
    /// Maps the exe name to the root pid of its process tree, or `None` if it isn't running.
    /// Behind a mutex so the capture thread can call it and `start` can run again after `stop`.
    resolve: Arc<Mutex<PidResolver>>,
    /// This source's input in the Master mix (D30), if it has one.
    mix: Option<MixInput>,
    worker: Option<Worker>,
    /// When the app was last audible (clock ticks), or `i64::MIN` for never.
    last_audible: Arc<AtomicI64>,
}

/// A sample louder than this (about -50 dBFS) counts as sound; anything quieter is idle noise.
const AUDIBLE: i16 = 100;

impl AppAudioSource {
    /// A stopped source for `exe` on `track`. `resolve` maps the exe name to the root pid of
    /// its process tree, or `None` when the app isn't running.
    #[must_use]
    pub fn new(track: TrackId, exe: String, resolve: PidResolver) -> Self {
        Self { track, exe, resolve: Arc::new(Mutex::new(resolve)), mix: None, worker: None, last_audible: Arc::new(AtomicI64::new(i64::MIN)) }
    }

    /// Puts this source in the Master mix as `mix`. Set it before `start`. `None` keeps the
    /// source out of the mix.
    #[must_use]
    pub fn with_mix(mut self, mix: Option<MixInput>) -> Self {
        self.mix = mix;
        self
    }
}

/// Runs the injected resolver. A poisoned lock counts as "not running".
fn resolve_pid(resolve: &Mutex<PidResolver>, exe: &str) -> Option<u32> {
    let guard = resolve.lock().ok()?;
    (**guard)(exe)
}

impl AudioSource for AppAudioSource {
    fn track(&self) -> TrackId {
        self.track
    }

    fn start(
        &mut self,
        config: &AudioStart,
        sink: SharedSink,
        _health: Emit<HealthEvent>,
    ) -> Result<(), SourceError> {
        self.stop();
        let (config, track, exe, resolve, mix) =
            (*config, self.track, self.exe.clone(), Arc::clone(&self.resolve), self.mix.clone());
        let heard = Arc::clone(&self.last_audible);
        self.worker = Some(Worker::spawn("zc-app-audio", move |stop, ready| {
            let (_apartment, _mf, mut encode) = match loopback::setup(track, &config, mix) {
                Ok(parts) => {
                    ready(Ok(()));
                    parts
                }
                Err(e) => return ready(Err(e)),
            };
            let _mmcss = Mmcss::join();
            let format = pcm16_format(config.sample_rate, config.channels);
            let mut timer = ResolveTimer::new(RESOLVE_EVERY);
            // The pid being captured (0: none) and its stream.
            let (mut current, mut stream) = (0u32, None::<Stream>);
            // Set once process loopback turns out to be unsupported: nothing more to try.
            let mut unsupported = false;
            while !stop.load(Ordering::Acquire) {
                if !unsupported && timer.due(zc_win::clock::now()) {
                    let pid = resolve_pid(&resolve, &exe).unwrap_or(0);
                    if pid != current {
                        stream = None;
                        current = pid;
                        if pid != 0 {
                            match loopback::activate(pid)
                                .and_then(|c| Stream::start(c, true, &format))
                            {
                                Ok(s) => {
                                    tracing::info!(%exe, pid, "capturing app audio");
                                    stream = Some(s);
                                }
                                Err(SourceError::Unsupported(e)) => {
                                    tracing::info!(%e, "per-app audio not recorded on this system");
                                    unsupported = true;
                                }
                                // `current` back to 0, so the next tick tries again.
                                Err(e) => {
                                    tracing::warn!(%exe, pid, %e, "can't capture this app's audio");
                                    current = 0;
                                }
                            }
                        }
                    }
                }
                // Silence up to 100 ms ago before any read, as in the game track.
                if let Err(e) = encode.pad_silence(zc_win::clock::now(), &*sink) {
                    tracing::warn!(%e, "app audio silence failed");
                }
                match &stream {
                    Some(s) => {
                        if let Err(e) =
                            s.read(|pcm, frames, qpc| {
                                if pcm.is_some_and(|p| p.iter().any(|v| v.unsigned_abs() > AUDIBLE as u16)) {
                                    heard.store(zc_win::clock::now().ticks(), Ordering::Relaxed);
                                }
                                encode.push(pcm, frames, qpc, &*sink)
                            })
                        {
                            // Drop the stream and retry on the next tick.
                            tracing::warn!(%exe, %e, "app audio capture stopped");
                            stream = None;
                            current = 0;
                        }
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
            encode.finish(&*sink);
        })?);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }

    fn audible_within(&self, window: Hns) -> bool {
        let heard = self.last_audible.load(Ordering::Relaxed);
        heard != i64::MIN && zc_win::clock::now().ticks() - heard <= window.ticks()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_timer_fires_once_per_interval() {
        let mut timer = ResolveTimer::new(Hns::from_secs(2));
        let s = Hns::from_secs;
        assert!(timer.due(s(10)), "the first call is due");
        assert!(!timer.due(s(11)), "1 s later is too soon");
        assert!(!timer.due(s(11) + Hns::from_millis(999)));
        assert!(timer.due(s(12)), "2 s after the first");
        assert!(timer.due(s(30)), "a long pause fires once");
        assert!(!timer.due(s(30)), "and does not catch up");
    }
}
