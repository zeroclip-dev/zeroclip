//! The toast overlay: a small card in the top-left corner that says what just happened
//! ("Clipped last 20 min"). `zc_win::toast` draws it as a plain Win32 layered window that is
//! click-through, non-activating, always on top and excluded from capture, so it never shows in a
//! recording or a screenshot.
//!
//! Limit: borderless and windowed games only. An exclusive-fullscreen game draws over the
//! overlay, so the card is not visible there.

mod model;

use gpui::App;

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
        background: theme::CARD,
        border: theme::BORDER,
        text: theme::FG,
        muted: theme::MUTED_FG,
        hold: HOLD,
        fade: if animate { FADE } else { std::time::Duration::ZERO },
    };
    if let Err(error) = zc_win::toast::show(toast) {
        tracing::warn!(%error, "toast not shown");
    }
}
