//! The update strip under the top bar: "Update to vX available". Non-modal, never takes focus.
//! Installing is always a click; the app never restarts by itself.

use gpui::{
    Context, FontWeight, IntoElement, ParentElement, StatefulInteractiveElement as _, Styled, div,
    px, rgb,
};

use super::components::banner::banner as strip;
use super::components::button::{button_ghost, button_primary};
use super::theme;
use crate::app::ZeroClipApp;
use crate::update::UpdateState;

/// The strip for the current update state. Nothing to draw when there is no update.
pub fn banner(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> Option<impl IntoElement> {
    let (text, actions) = match &app.update {
        UpdateState::None => return None,
        UpdateState::Available(r) => (
            format!("Update to v{} available", r.version),
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    button_primary("update-go", "Update")
                        .on_click(cx.listener(|app, _, _, cx| app.start_update(cx))),
                )
                .child(button_ghost("update-later", "Later").on_click(cx.listener(
                    |app, _, _, cx| {
                        app.dismiss_update();
                        cx.notify();
                    },
                ))),
        ),
        UpdateState::Downloading(r) => {
            (format!("Downloading v{}...", r.version), div().flex().flex_row())
        }
        UpdateState::Ready { release, .. } => (
            format!("v{} is ready", release.version),
            div().flex().flex_row().child(
                button_primary("update-restart", "Restart to update")
                    .on_click(cx.listener(|app, _, _, cx| app.install_update(cx))),
            ),
        ),
    };
    Some(
        strip()
            .bg(rgb(theme::BRAND_SOFT))
            .border_color(rgb(theme::BRAND_BORDER))
            .child(
                div()
                    .flex_1()
                    .min_w(px(220.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::FG))
                    .child(text),
            )
            .child(actions),
    )
}
