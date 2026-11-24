//! A settings-style row: a fixed-width label, then the control, over a hairline. Used by Settings
//! and the Editor's per-track controls.

use gpui::{IntoElement, ParentElement, Styled, div, px, rgb};

use crate::ui::theme;

pub fn field_row(label: &'static str, control: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .min_h(px(30.))
        .pb_2()
        .border_b_1()
        .border_color(rgb(theme::BORDER))
        .child(div().w(px(190.)).flex_shrink_0().text_color(rgb(theme::FG)).child(label))
        .child(control)
}
