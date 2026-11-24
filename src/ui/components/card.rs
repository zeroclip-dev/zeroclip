//! Card surface and keyboard-chip helpers shared by the screens (design preview `.card`, `kbd`).

use gpui::{IntoElement, ParentElement, SharedString, Styled, div, px, rgb};

use crate::ui::theme;

/// A bordered surface with a small heading. Add the body with `.child(..)`.
pub fn card(title: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child(title.into()))
}

/// A hotkey chip such as `Alt+F9`.
pub fn kbd(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(theme::RADIUS_SM))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::MUTED))
        .text_xs()
        .text_color(rgb(theme::MUTED_FG))
        .child(text.into())
}
