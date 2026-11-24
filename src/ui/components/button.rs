//! Buttons in three looks, after the design preview: `button` (outline, the default),
//! `button_primary` and `button_ghost`. Add `.on_click(cx.listener(..))` at the call site.

use gpui::{
    ElementId, InteractiveElement, ParentElement, Role, SharedString, StatefulInteractiveElement,
    Styled, div, px, rgb,
};

use crate::ui::theme;

fn base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        // Keyboard: reachable with Tab, ringed when focused that way; screen readers: a button.
        .tab_stop(true)
        .focus_visible(|s| s.border_1().border_color(rgb(theme::RING)))
        .role(Role::Button)
        .aria_label(label.clone())
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
        .child(label)
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
        .text_color(rgb(theme::FG_SOFT))
        .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)))
}

/// The primary button, or a quiet grey one while there is nothing to do (a dimmed white button
/// shouts louder than the active one beside it).
pub fn button_primary_if(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    if enabled {
        button_primary(id, label)
    } else {
        base(id, label).bg(rgb(theme::MUTED)).text_color(rgb(theme::MUTED_FG)).cursor_default()
    }
}
