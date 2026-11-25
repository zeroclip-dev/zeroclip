//! A switch with a label. `on_change` gets the new value; wire it with
//! `cx.listener(|this, on: &bool, _, cx| ..)`. When `enabled` is false it is dimmed and ignores clicks.

use gpui::{
    App, ElementId, InteractiveElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Toggled, Window, div, px, rgb, transparent_black,
};

use super::toggle::switch_track;
use crate::ui::theme;

pub fn labeled_toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    enabled: bool,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    let row = div()
        .id(id)
        .tab_stop(true)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .role(Role::Switch)
        .aria_label(label.clone())
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .min_h(px(theme::CONTROL_H))
        .px_1()
        .rounded(px(theme::RADIUS_MD))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .text_size(px(theme::TEXT_BODY))
        .text_color(rgb(theme::FG_SOFT))
        .child(switch_track(on))
        .child(div().child(label));
    if enabled {
        row.cursor_pointer()
            .hover(|s| s.text_color(rgb(theme::FG)))
            .active(|s| s.opacity(0.85))
            .on_click(move |_, window, cx| on_change(&!on, window, cx))
    } else {
        row.opacity(0.45)
    }
}
