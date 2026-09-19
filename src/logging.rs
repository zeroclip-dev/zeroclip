//! Logging: `tracing` to daily-rotated files, plus stderr in debug builds.
//!
//! Set `ZEROCLIP_LOG` to change the filter, e.g. `ZEROCLIP_LOG=zc_engine=trace`.
//!
//! Phase 4 adds a "flight recorder": an in-memory ring of the last few thousand
//! events (frame timings, encoder results, watchdog transitions) dumped next to
//! crash-rescue files — capture bugs are often unreproducible, and "it stuttered
//! once last Tuesday" is unfixable without a trace (DESIGN §12).

use std::path::Path;

use anyhow::Context as _;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

const DEFAULT_FILTER: &str = "info,zeroclip=debug,zc_engine=debug";

/// Starts logging. Keep the returned guard alive for the life of the program: it
/// flushes the file writer when dropped.
pub fn init(logs_dir: &Path) -> anyhow::Result<WorkerGuard> {
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

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .context("logging was already initialised")?;

    // Panics go to the log file too, not just to a console nobody sees.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "panic");
        default_hook(info);
    }));

    Ok(guard)
}
