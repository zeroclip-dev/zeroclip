//! Pick one of a few options, shown as a recessed pill group (stands in for a dropdown); the chosen
//! segment sits raised on `SURFACE_RAISED`. `on_select` gets the option's index; wire it with
//! `cx.listener(|this, ix: &usize, _, cx| ..)`. `selected` is `None` when the current value isn't
//! in the list.

use gpui::{
    App, ElementId, FontWeight, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, rgb, transparent_black,
};

use crate::ui::theme;

pub fn segmented(
    id: impl Into<SharedString>,
    options: Vec<SharedString>,
    selected: Option<usize>,
    enabled: bool,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let id: SharedString = id.into();
    let on_select = std::rc::Rc::new(on_select);
    let chips = options.into_iter().enumerate().map(|(i, label)| {
        let on_select = on_select.clone();
        let active = selected == Some(i);
        let chip = div()
            .id(ElementId::Name(format!("{id}-{i}").into()))
            .tab_stop(true)
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .role(gpui::Role::RadioButton)
            .h(px(24.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(theme::RADIUS_SM))
            .text_size(px(theme::TEXT_BODY))
            .font_weight(FontWeight::MEDIUM)
            .border_1()
            .child(label);
        let chip = if active {
            chip.bg(rgb(theme::SURFACE_RAISED))
                .border_color(rgb(theme::BORDER_STRONG))
                .text_color(rgb(theme::FG))
                .aria_selected(true)
        } else {
            // Same border width as the active chip, so choosing one doesn't shift the row.
            chip.border_color(transparent_black()).text_color(rgb(theme::MUTED_FG))
        };
        if !enabled {
            return chip;
        }
        let chip = chip.cursor_pointer();
        let chip = if active {
            chip
        } else {
            chip.hover(|h| h.bg(rgb(theme::CARD)).text_color(rgb(theme::FG)))
                .active(|a| a.bg(rgb(theme::ACCENT)))
        };
        chip.on_click(move |_, window, cx| on_select(&i, window, cx))
    });
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .min_h(px(theme::CONTROL_H))
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::SURFACE_INSET))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .opacity(if enabled { 1.0 } else { 0.45 })
        .children(chips)
}
