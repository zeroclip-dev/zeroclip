//! An on/off switch. Add `.on_click(cx.listener(..))` at the call site, and only when `enabled`:
//! a disabled switch is drawn dimmed and has no pointer cursor.

use gpui::{
    ElementId, InteractiveElement, ParentElement, Role, StatefulInteractiveElement, Styled,
    Toggled, div, px, rgb,
};

use crate::ui::theme;

pub fn toggle(id: impl Into<ElementId>, on: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    let track = div()
        .id(id)
        .tab_stop(true)
        .focus_visible(|s| s.border_1().border_color(rgb(theme::RING)))
        .role(Role::Switch)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .w(px(34.0))
        .h(px(20.0))
        .p(px(2.0))
        .rounded_full()
        .flex()
        .flex_row()
        .bg(rgb(if on { theme::BRAND } else { theme::INPUT }))
        .child(div().size(px(16.0)).rounded_full().bg(rgb(theme::FG)));
    let track = if on { track.justify_end() } else { track };
    if enabled { track.cursor_pointer().hover(|s| s.opacity(0.85)) } else { track.opacity(0.4) }
}
