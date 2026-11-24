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
    div()
        .id(id)
        .track_focus(focus)
        .w(px(width))
        .h(px(32.))
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::BG))
        .border_1()
        .border_color(rgb(if focused { theme::RING } else { theme::INPUT }))
        .cursor_text()
        .text_color(rgb(if value.is_empty() { theme::MUTED_FG } else { theme::FG }))
        .on_click(move |_, window, _| handle.focus(window))
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
