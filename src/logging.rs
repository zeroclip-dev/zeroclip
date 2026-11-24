//! Logging: `tracing` to daily-rotated files, plus stderr in debug builds.
//!
//! Set `ZEROCLIP_LOG` to change the filter, e.g. `ZEROCLIP_LOG=zc_engine=trace`.
//!
//! The "flight recorder" ([`FlightRecorder`]) is an in-memory ring of the last few
//! thousand events (watchdog transitions, encoder results, profile changes) dumped
//! next to crash-rescue files and on panic: capture bugs are often unreproducible,
//! and "it stuttered once last Tuesday" is unfixable without a trace (DESIGN §12).
//!
//! The ring allocates a `String` per event, so hot-path threads (capture, encode,
//! audio) must log only at `trace`, which the default filter drops.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use tracing::field::{Field, Visit};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::util::SubscriberInitExt as _;

const DEFAULT_FILTER: &str = "info,zeroclip=debug,zc_engine=debug";

/// How many recent log lines the flight recorder keeps.
const CAPACITY: usize = 5_000;

/// The newest [`CAPACITY`] log lines, kept in memory.
pub struct FlightRecorder {
    lines: Mutex<VecDeque<String>>,
    started: Instant,
}

impl FlightRecorder {
    fn new() -> Self {
        Self { lines: Mutex::new(VecDeque::with_capacity(CAPACITY)), started: Instant::now() }
    }

    fn push(&self, line: String) {
        // A poisoned ring is still a fine ring: keep recording.
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= CAPACITY {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Writes the ring to `path`, oldest line first. The lock is held only to copy the
    /// lines out, never across the file write.
    pub fn dump(&self, path: &Path) -> io::Result<()> {
        let copy: Vec<String> =
            self.lines.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
        write_lines(path, &copy)
    }

    /// Dumps to `dir/flight-<unix>[suffix].log` and returns the path written.
    pub fn dump_to_dir(&self, dir: &Path, suffix: &str) -> io::Result<PathBuf> {
        let path = flight_path(dir, suffix);
        self.dump(&path)?;
        Ok(path)
    }

    /// Panic-hook variant: never waits. If the panicking thread died inside `push` the
    /// lock is held and a blocking `lock` would deadlock, so give up instead.
    fn dump_on_panic(&self, dir: &Path) -> io::Result<PathBuf> {
        let copy: Vec<String> = match self.lines.try_lock() {
            Ok(lines) => lines.iter().cloned().collect(),
            Err(TryLockError::Poisoned(e)) => e.into_inner().iter().cloned().collect(),
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::other("the flight recorder was busy"));
            }
        };
        let path = flight_path(dir, "-panic");
        write_lines(&path, &copy)?;
        Ok(path)
    }
}

fn flight_path(dir: &Path, suffix: &str) -> PathBuf {
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    dir.join(format!("flight-{unix}{suffix}.log"))
}

fn write_lines(path: &Path, lines: &[String]) -> io::Result<()> {
    std::fs::write(path, lines.join("\n"))
}

/// Feeds every event the subscriber lets through into a [`FlightRecorder`].
struct FlightLayer(Arc<FlightRecorder>);

impl<S: tracing::Subscriber> Layer<S> for FlightLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let meta = event.metadata();
        let mut line = format!(
            "{:>10.3} {:5} {}:",
            self.0.started.elapsed().as_secs_f64(),
            meta.level(),
            meta.target()
        );
        event.record(&mut LineVisitor(&mut line));
        self.0.push(line);
    }
}

/// Appends an event's fields to a line: the message bare, the rest as ` name=value`.
struct LineVisitor<'a>(&'a mut String);

impl Visit for LineVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // Writing to a String cannot fail.
        let _ = if field.name() == "message" {
            write!(self.0, " {value:?}")
        } else {
            write!(self.0, " {}={value:?}", field.name())
        };
    }
}

/// Starts logging. Keep the returned guard alive for the life of the program: it
/// flushes the file writer when dropped. The recorder is for dumping on a rescue.
pub fn init(logs_dir: &Path) -> anyhow::Result<(WorkerGuard, Arc<FlightRecorder>)> {
    std::fs::create_dir_all(logs_dir)
        .with_context(|| format!("couldn't create {}", logs_dir.display()))?;
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("zeroclip")
        .filename_suffix("log")
        .max_log_files(7)
        .build(logs_dir)
        .context("couldn't open the log file")?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let filter =
        EnvFilter::try_from_env("ZEROCLIP_LOG").unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let file_layer = tracing_subscriber::fmt::layer().with_writer(file_writer).with_ansi(false);
    let stderr_layer = cfg!(debug_assertions)
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));

    let recorder = Arc::new(FlightRecorder::new());
    tracing_subscriber::registry()
        .with(filter)
        .with(FlightLayer(recorder.clone()))
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .context("logging was already initialised")?;

    // Panics go to the log file too, not just to a console nobody sees.
    let default_hook = std::panic::take_hook();
    let hook_recorder = recorder.clone();
    let hook_dir = logs_dir.to_path_buf();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "panic");
        // The ring now ends with the panic line itself.
        if let Err(error) = hook_recorder.dump_on_panic(&hook_dir) {
            tracing::error!(%error, "couldn't dump the flight recorder");
        }
        default_hook(info);
    }));

    Ok((guard, recorder))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flight_recorder_keeps_the_newest_events() {
        let rec = Arc::new(FlightRecorder::new());
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(FlightLayer(rec.clone())),
            || {
                for i in 0..CAPACITY + 10 {
                    tracing::info!(i, "tick");
                }
            },
        );
        let lines = rec.lines.lock().unwrap();
        assert_eq!(lines.len(), CAPACITY);
        let last = lines.back().unwrap();
        assert!(last.contains("tick") && last.contains("i=5009"), "{last}");
        assert!(lines.front().unwrap().contains("i=10"), "oldest 10 dropped");
    }
}
