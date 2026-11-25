//! The non-modal banner strip under the top bar (focus prompt, rescue prompt). Add children at
//! the call site.

use gpui::{Div, ParentElement, Styled, div, px, rgb};

use crate::ui::theme;

const BANNER_BG: u32 = theme::WARN_SOFT;
const BANNER_EDGE: u32 = theme::WARN_BORDER;

pub fn banner() -> Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .min_h(px(44.))
        .gap_3()
        .px_4()
        .py_2()
        .bg(rgb(BANNER_BG))
        .border_b_1()
        .border_color(rgb(BANNER_EDGE))
        .text_size(px(theme::TEXT_BODY))
        .text_color(rgb(theme::FG))
        .child(div().size(px(6.)).flex_shrink_0().rounded_full().bg(rgb(theme::WARN)))
}
