//! A switch with a label. `on_change` gets the new value; wire it with
//! `cx.listener(|this, on: &bool, _, cx| ..)`. When `enabled` is false it is dimmed and ignores clicks.

use gpui::{
    App, ElementId, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px, rgb,
};

pub fn labeled_toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    enabled: bool,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let knob = div().size(px(14.)).rounded_full().bg(rgb(0xffffff));
    let mut track = div()
        .w(px(34.))
        .h(px(18.))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .px(px(2.))
        .bg(rgb(if on { 0x4f8cff } else { 0x4a4a55 }));
    if on {
        track = track.justify_end();
    }
    let row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(track.child(knob))
        .child(div().child(label.into()));
    if enabled {
        row.cursor_pointer().on_click(move |_, window, cx| on_change(&!on, window, cx))
    } else {
        row.opacity(0.4)
    }
}
