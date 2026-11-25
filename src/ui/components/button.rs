//! Buttons in consistent 32 px height (36 px for `button_hero`) and 8 px radius: `button`
//! (secondary outline, the default), `button_primary`, `button_hero`, `button_ghost`, and
//! `button_danger`. Add `.on_click(cx.listener(..))` at the call site.

use gpui::{
    ElementId, FontWeight, InteractiveElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, div, px, rgb, transparent_black,
};

use crate::ui::theme;

fn base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        // Keyboard: reachable with Tab, ringed when focused that way; screen readers: a button.
        .tab_stop(true)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .role(Role::Button)
        .aria_label(label.clone())
        .h(px(theme::CONTROL_H))
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(theme::RADIUS_MD))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label)
}

/// Secondary / outline: lifted surface with a crisp 1 px border and clear hover/press states.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .bg(rgb(theme::SURFACE_RAISED))
        .border_color(rgb(theme::INPUT))
        .text_color(rgb(theme::FG))
        .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
        .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)).border_color(rgb(theme::BORDER_STRONG)))
}

/// The one main high-contrast action on a card or bar.
pub fn button_primary(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .bg(rgb(theme::PRIMARY))
        .text_color(rgb(theme::PRIMARY_FG))
        .font_weight(FontWeight::SEMIBOLD)
        .hover(|s| s.bg(rgb(theme::PRIMARY_HOVER)))
        .active(|s| s.bg(rgb(theme::PRIMARY_ACTIVE)))
}

/// Larger 36 px primary CTA reserved for hero-level actions.
pub fn button_hero(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    button_primary(id, label).h(px(theme::CONTROL_H_LG)).px_4().text_size(px(theme::TEXT_HEADING))
}

/// Quiet action: no border or fill until hovered.
pub fn button_ghost(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .text_color(rgb(theme::FG_SOFT))
        .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)))
        .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)).text_color(rgb(theme::FG)))
}

/// Destructive action (delete, discard, stop-and-erase). Red is reserved for recording and danger.
pub fn button_danger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    base(id, label)
        .bg(rgb(theme::DANGER_SOFT))
        .border_color(rgb(theme::DANGER_BORDER))
        .text_color(rgb(theme::DANGER))
        .hover(|s| {
            s.bg(rgb(theme::DANGER))
                .border_color(rgb(theme::DANGER))
                .text_color(rgb(theme::PRIMARY_FG))
        })
        .active(|s| s.opacity(0.88))
}

/// The primary button, or a quiet muted one while there is nothing to do (a dimmed white button
/// shouts louder than the active one beside it).
pub fn button_primary_if(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    if enabled {
        button_primary(id, label)
    } else {
        base(id, label)
            .bg(rgb(theme::MUTED))
            .border_color(rgb(theme::BORDER_SUBTLE))
            .text_color(rgb(theme::TEXT_DIM))
            .cursor_default()
    }
}
