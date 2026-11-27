//! The system tray (notification area) icon and its menu.
//!
//! The icon and menu come from the `tray-icon` crate, which calls
//! [`Shell_NotifyIconW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw)
//! underneath. The icon must be created on the main thread, inside the running event loop; the
//! app does that in `ZeroClipApp::new`, and keeps the [`Tray`] alive for as long as the app runs.
//!
//! Menu and icon events arrive on the main thread too. Their handlers only queue a
//! [`TrayCommand`] and call the wake callback, so the UI handles them on its own schedule.

use std::sync::{Arc, mpsc};

use anyhow::Context as _;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// What the user asked for from the tray.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// Bring the window back (menu item, or a left click on the icon).
    Show,
    /// Save the last clip now, as the save hotkey does.
    SaveClip,
    /// Quit ZeroClip, even when close-to-tray is on.
    Quit,
}

/// The tray icon, its menu, and the queue of commands from them.
pub struct Tray {
    /// Kept alive: dropping the icon removes it from the notification area.
    _icon: TrayIcon,
    pub commands: mpsc::Receiver<TrayCommand>,
}

/// Creates the tray icon. `wake` runs on the main thread after each command is queued, so the UI
/// can drain [`Tray::commands`] promptly even while its window is hidden.
///
/// # Errors
/// The menu or the icon could not be created.
pub fn create(wake: Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<Tray> {
    let show = MenuItem::new("Show ZeroClip", true, None);
    let save = MenuItem::new("Save clip", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append(&show).context("adding the Show menu item")?;
    menu.append(&save).context("adding the Save clip menu item")?;
    menu.append(&PredefinedMenuItem::separator()).context("adding the separator")?;
    menu.append(&quit).context("adding the Quit menu item")?;

    let (tx, rx) = mpsc::channel();

    let (show_id, save_id, quit_id) = (show.id().clone(), save.id().clone(), quit.id().clone());
    let menu_tx = tx.clone();
    let menu_wake = Arc::clone(&wake);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = if event.id == show_id {
            TrayCommand::Show
        } else if event.id == save_id {
            TrayCommand::SaveClip
        } else if event.id == quit_id {
            TrayCommand::Quit
        } else {
            return;
        };
        let _ = menu_tx.send(command);
        menu_wake();
    }));

    // A left click opens the window; the right click shows the menu (`with_menu_on_left_click`).
    let click_tx = tx;
    let click_wake = wake;
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            let _ = click_tx.send(TrayCommand::Show);
            click_wake();
        }
    }));

    let icon = TrayIconBuilder::new()
        .with_tooltip("ZeroClip")
        .with_icon(circle_icon()?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .context("building the tray icon")?;
    Ok(Tray { _icon: icon, commands: rx })
}

/// A filled red disc, drawn in code so the app needs no image asset or decoder.
fn circle_icon() -> anyhow::Result<Icon> {
    const SIZE: u32 = 32;
    const RADIUS: f32 = 13.0;
    const RED: [u8; 4] = [0xE5, 0x48, 0x4D, 0xFF];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];
    let centre = (SIZE as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let inside = dx * dx + dy * dy <= RADIUS * RADIUS;
            rgba.extend_from_slice(if inside { &RED } else { &CLEAR });
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("making the tray icon image")
}
