//! A plain text button. Add `.on_click(cx.listener(..))` at the call site.

use gpui::{ElementId, InteractiveElement, ParentElement, SharedString, Styled, div, rgb};

pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(0x34343c))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x44444e)))
        .child(label.into())
}
