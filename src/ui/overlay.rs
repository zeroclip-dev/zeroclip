//! The toast overlay: a small card in the top-left corner that says what just happened
//! ("Clipped last 20 min"). `zc_win::toast` draws it as a plain Win32 layered window that is
//! click-through, non-activating, always on top and excluded from capture, so it never shows in a
//! recording or a screenshot.
//!
//! Limit: borderless and windowed games only. An exclusive-fullscreen game draws over the
//! overlay, so the card is not visible there.

mod alert;
mod model;

use gpui::App;

pub use alert::{AlertLevel, Coalescer, split as split_alert};
pub use model::ToastKind;
use model::{FADE, HOLD};

use crate::ui::theme;

/// Shows a toast for `kind`, replacing the one that is up. Nothing is shown if the window can't
/// be made or can't be excluded from capture.
pub fn show(cx: &App, kind: ToastKind) {
    let text = model::toast_text(kind);
    // No fade when the user has turned animations off.
    let animate = !cx.reduce_motion() && zc_win::overlay::system_animations_enabled();
    let toast = zc_win::toast::Toast {
        title: text.title,
        detail: text.detail,
        accent: model::accent(kind),
        background: theme::SURFACE_RAISED,
        border: model::border(kind),
        text: theme::FG,
        muted: theme::MUTED_FG,
        hold: HOLD,
        fade: if animate { FADE } else { std::time::Duration::ZERO },
        alert: false,
    };
    if let Err(error) = zc_win::toast::show(toast) {
        tracing::warn!(%error, "toast not shown");
    }
}

/// Shows the failure popup: larger than a toast, an alert icon, a close button, and it stays for
/// [`alert::lifetime`]. `count` above 1 marks a repeat. Shown even with toasts turned off, because
/// a player in a game has no other way to learn that recording stopped.
pub fn show_alert(cx: &App, level: AlertLevel, message: &str, count: u32) {
    let (mut title, detail) = alert::split(level, message);
    if count > 1 {
        title = format!("{title} (x{count})");
    }
    let animate = !cx.reduce_motion() && zc_win::overlay::system_animations_enabled();
    let fade = if animate { FADE } else { std::time::Duration::ZERO };
    let (accent, border) = match level {
        AlertLevel::Error => (theme::DANGER, theme::DANGER_BORDER),
        AlertLevel::Warning => (theme::WARN, theme::WARN_BORDER),
    };
    let toast = zc_win::toast::Toast {
        title,
        detail: Some(detail),
        accent,
        background: theme::SURFACE_RAISED,
        border,
        text: theme::FG,
        muted: theme::MUTED_FG,
        hold: alert::lifetime(level).saturating_sub(fade * 2),
        fade,
        alert: true,
    };
    if let Err(error) = zc_win::toast::show(toast) {
        tracing::warn!(%error, "alert popup not shown");
    }
}
