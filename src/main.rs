//! ZeroClip — the desktop app.
//!
//! This crate is the composition root: it resolves paths, starts logging, loads
//! settings, builds the platform adapters ([`wiring`]) and hands them to the
//! engine, then runs the GPUI UI. All recording logic lives in `crates/`; see
//! docs/ARCHITECTURE.md.

// Release builds are GUI apps: no console window. Debug builds keep the console
// so logs are visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod logging;
mod paths;
mod settings_store;
mod tray;
mod ui;
mod wiring;

use gpui::{AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

fn main() -> anyhow::Result<()> {
    let paths = paths::AppPaths::resolve()?;
    let (_log_guard, recorder) = logging::init(&paths.logs_dir)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ZeroClip starting");

    // Two copies would share the disk-ring DVR file and one would truncate the other's buffer.
    // Held until `main` returns. A failing guard must not block startup.
    let _instance = match zc_win::single_instance::acquire(zc_win::single_instance::INSTANCE_MUTEX)
    {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            tracing::warn!("ZeroClip is already running; exiting this copy");
            zc_win::single_instance::show_already_running();
            return Ok(());
        }
        Err(error) => {
            tracing::warn!(%error, "single-instance guard failed; continuing without it");
            None
        }
    };

    let settings = settings_store::load(&paths.settings_file);
    // Windows starts ZeroClip with `--tray` at sign-in (P4.16): the window stays hidden.
    let start_hidden = std::env::args().any(|arg| arg == "--tray");

    gpui_platform::application().with_assets(ui::icons::Icons).run(move |cx| {
        cx.bind_keys([gpui::KeyBinding::new("f11", ui::ToggleFullscreen, None)]);
        let bounds = Bounds::centered(None, size(px(980.0), px(660.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(760.0), px(500.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("ZeroClip".into()),
                    // No Windows title bar: the top bar draws its own (ui::top_bar).
                    appears_transparent: true,
                    ..Default::default()
                }),
                show: !start_hidden,
                ..Default::default()
            },
            |window, cx| {
                let app = cx.new(|cx| match app::ZeroClipApp::new(paths, settings, recorder, cx) {
                    Ok(app) => app,
                    Err(error) => {
                        tracing::error!(%error, "couldn't start ZeroClip");
                        // ponytail: a failed engine start has no error screen yet; exit.
                        std::process::exit(1);
                    }
                });
                // Close-to-tray (P4.15): the app decides whether this close hides or quits.
                let weak = app.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    weak.update(cx, |app, cx| app.close_requested(cx)).unwrap_or(true)
                });
                app
            },
        );
        if let Err(error) = opened {
            tracing::error!(%error, "couldn't open the window");
            cx.quit();
        }
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
    Ok(())
}
