//! Buttons in three looks, after the design preview: `button` (outline, the default),
//! `button_primary` and `button_ghost`. Add `.on_click(cx.listener(..))` at the call site.

use gpui::{ElementId, InteractiveElement, ParentElement, SharedString, Styled, div, px, rgb};

use crate::ui::theme;

fn base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded(px(theme::RADIUS_MD))
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label.into())
}

/// Outline: a border, hover fills with `ACCENT`.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .bg(rgb(theme::BG))
        .border_1()
        .border_color(rgb(theme::INPUT))
        .text_color(rgb(theme::FG))
        .hover(|s| s.bg(rgb(theme::ACCENT)))
}

/// The one main action on a card.
pub fn button_primary(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .bg(rgb(theme::PRIMARY))
        .text_color(rgb(theme::PRIMARY_FG))
        .hover(|s| s.opacity(0.9))
}

/// No border or fill until hovered.
pub fn button_ghost(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .text_color(rgb(theme::MUTED_FG))
        .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)))
}
