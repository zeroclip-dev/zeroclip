//! An on/off switch. Add `.on_click(cx.listener(..))` at the call site, and only when `enabled`:
//! a disabled switch is drawn dimmed and has no pointer cursor.

use gpui::{
    ElementId, InteractiveElement, ParentElement, Role, StatefulInteractiveElement, Styled,
    Toggled, div, px, rgb,
};

use crate::ui::theme;

/// Shared switch track visual used by both [`toggle`] and `labeled_toggle`.
pub(super) fn switch_track(on: bool) -> gpui::Div {
    let knob =
        div().size(px(14.)).rounded_full().bg(rgb(if on { theme::FG } else { theme::FG_SOFT }));
    let track = div()
        .w(px(36.))
        .h(px(20.))
        .flex_shrink_0()
        .p(px(2.))
        .rounded_full()
        .border_1()
        .border_color(rgb(if on { theme::BRAND } else { theme::INPUT }))
        .bg(rgb(if on { theme::BRAND } else { theme::SURFACE_INSET }))
        .flex()
        .flex_row()
        .items_center()
        .child(knob);
    if on { track.justify_end() } else { track }
}

pub fn toggle(id: impl Into<ElementId>, on: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    let track = switch_track(on)
        .id(id)
        .tab_stop(true)
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .role(Role::Switch)
        .aria_toggled(if on { Toggled::True } else { Toggled::False });
    if enabled {
        track
            .cursor_pointer()
            .hover(move |s| {
                if on {
                    s.opacity(0.9)
                } else {
                    s.bg(rgb(theme::SURFACE_RAISED)).border_color(rgb(theme::BORDER_STRONG))
                }
            })
            .active(|s| s.opacity(0.8))
    } else {
        track.opacity(0.45)
    }
}
