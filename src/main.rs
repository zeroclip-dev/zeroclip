//! ZeroClip — the desktop app.
//!
//! This crate is the composition root: it resolves paths, starts logging, loads
//! settings, builds the platform adapters ([`wiring`]) and hands them to the
//! engine, then runs the egui UI. All recording logic lives in `crates/`; see
//! docs/ARCHITECTURE.md.

// Release builds are GUI apps: no console window. Debug builds keep the console
// so logs are visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod logging;
mod paths;
mod settings_store;
mod ui;
mod wiring;

use eframe::egui;

fn main() -> anyhow::Result<()> {
    let paths = paths::AppPaths::resolve()?;
    let _log_guard = logging::init(&paths.logs_dir)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ZeroClip starting");

    let settings = settings_store::load(&paths.settings_file);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZeroClip")
            .with_inner_size([980.0, 660.0])
            .with_min_inner_size([760.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ZeroClip",
        options,
        Box::new(move |cc| Ok(Box::new(app::ZeroClipApp::new(cc, paths, settings)?))),
    )
    .map_err(|e| anyhow::anyhow!("the UI failed: {e}"))
}
