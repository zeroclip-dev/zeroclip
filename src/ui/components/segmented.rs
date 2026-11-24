//! Pick one of a few options, shown as a muted pill group (stands in for a dropdown); the chosen
//! segment sits on `BG`. `on_select` gets the option's index; wire it with
//! `cx.listener(|this, ix: &usize, _, cx| ..)`. `selected` is `None` when the current value isn't
//! in the list.

use gpui::{
    App, ElementId, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px, rgb,
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
            .h(px(26.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(theme::RADIUS_SM))
            .text_sm()
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(label);
        let chip = if active {
            chip.bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::INPUT))
                .text_color(rgb(theme::FG))
                .aria_selected(true)
        } else {
            // Same border width as the active chip, so choosing one doesn't shift the row.
            chip.border_1().border_color(gpui::transparent_black()).text_color(rgb(theme::MUTED_FG))
        };
        if !enabled {
            return chip;
        }
        let chip = chip.cursor_pointer();
        let chip = if active { chip } else { chip.hover(|h| h.text_color(rgb(theme::FG))) };
        chip.on_click(move |_, window, cx| on_select(&i, window, cx))
    });
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::MUTED))
        .opacity(if enabled { 1.0 } else { 0.4 })
        .children(chips)
}
