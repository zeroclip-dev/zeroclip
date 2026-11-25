//! Card surface and keyboard-chip helpers shared by the screens.

use gpui::{FontWeight, IntoElement, ParentElement, SharedString, Styled, div, px, rgb};

use crate::ui::theme;

/// A layered card surface with a section heading. Add the body with `.child(..)`.
pub fn card(title: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(theme::section_heading(title))
}

/// A tactile hotkey chip such as `Alt+F9`.
pub fn kbd(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px(px(6.))
        .py(px(2.))
        .rounded(px(theme::RADIUS_SM))
        .border_1()
        .border_color(rgb(theme::INPUT))
        .bg(rgb(theme::SURFACE_RAISED))
        .text_size(px(theme::TEXT_MICRO))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::FG_SOFT))
        .child(text.into())
}
