//! A single-line text box. Stateless: the caller owns the string and a `FocusHandle` (made with
//! `cx.focus_handle()`), and `on_change` gets the whole new text. Typing appends, Backspace deletes
//! the last character, Ctrl+V pastes. ponytail: no caret movement, selection or IME; add when needed.

use gpui::{
    App, ElementId, FocusHandle, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, rgb,
};

use crate::ui::theme;

pub fn text_field(
    id: impl Into<ElementId>,
    focus: &FocusHandle,
    window: &Window,
    value: &str,
    placeholder: &str,
    width: f32,
    on_change: impl Fn(&String, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let focused = focus.is_focused(window);
    let shown: SharedString = if value.is_empty() { placeholder } else { value }.to_owned().into();
    let current = value.to_owned();
    let handle = focus.clone();
    let field = div()
        .id(id)
        .track_focus(focus)
        .w(px(width))
        .h(px(theme::CONTROL_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::SURFACE_INSET))
        .border_1()
        .border_color(rgb(if focused { theme::RING } else { theme::INPUT }))
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .text_size(px(theme::TEXT_BODY))
        .cursor_text()
        .text_color(rgb(if value.is_empty() { theme::MUTED_FG } else { theme::FG }));
    let field = if focused {
        field
    } else {
        field
            .hover(|s| s.border_color(rgb(theme::BORDER_STRONG)))
            .active(|s| s.border_color(rgb(theme::RING)))
    };
    field
        .on_click(move |_, window, cx| handle.focus(window, cx))
        .on_key_down(move |ev, window, cx| {
            let k = &ev.keystroke;
            let mut text = current.clone();
            if k.modifiers.control && k.key == "v" {
                if let Some(paste) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    text.push_str(paste.lines().next().unwrap_or(""));
                }
            } else if k.key == "backspace" {
                text.pop();
            } else if let (false, Some(ch)) = (k.modifiers.control || k.modifiers.alt, &k.key_char)
            {
                text.push_str(ch);
            } else {
                return;
            }
            on_change(&text, window, cx);
        })
        .child(shown)
}
