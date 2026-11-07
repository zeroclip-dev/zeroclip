//! A top-bar tab. Highlighted when `active`; add `.on_click(cx.listener(..))` at the call site.

use gpui::{InteractiveElement, ParentElement, SharedString, Styled, div, rgb};

pub fn tab_button(label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    let text: SharedString = label.into();
    div()
        .id(text.clone())
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .when_active(active)
        .hover(|s| s.bg(rgb(0x2c2c34)))
        .child(text)
}

trait WhenActive: Styled + Sized {
    fn when_active(self, active: bool) -> Self {
        if active { self.bg(rgb(0x3b3b46)) } else { self }
    }
}
impl<T: Styled> WhenActive for T {}
