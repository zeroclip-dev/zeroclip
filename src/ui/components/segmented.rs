//! Pick one of a few options, shown as a wrapping row of chips (stands in for a dropdown).
//! `on_select` gets the option's index; wire it with `cx.listener(|this, ix: &usize, _, cx| ..)`.
//! `selected` is `None` when the current value isn't in the list.

use gpui::{
    App, ElementId, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, rgb,
};

pub fn segmented(
    id: &'static str,
    options: Vec<SharedString>,
    selected: Option<usize>,
    enabled: bool,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let on_select = std::rc::Rc::new(on_select);
    let chips = options.into_iter().enumerate().map(|(i, label)| {
        let on_select = on_select.clone();
        let active = selected == Some(i);
        let chip = div()
            .id(ElementId::Name(format!("{id}-{i}").into()))
            .px_3()
            .py_1()
            .rounded_md()
            .bg(rgb(if active { 0x4f8cff } else { 0x34343c }))
            .child(label);
        if !enabled {
            return chip;
        }
        let chip = chip.cursor_pointer();
        let chip = if active { chip } else { chip.hover(|h| h.bg(rgb(0x44444e))) };
        chip.on_click(move |_, window, cx| on_select(&i, window, cx))
    });
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_1()
        .opacity(if enabled { 1.0 } else { 0.4 })
        .children(chips)
}
