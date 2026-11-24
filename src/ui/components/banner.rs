//! The non-modal banner strip under the top bar (focus prompt, rescue prompt). Add children at
//! the call site.

use gpui::{Div, Styled, div, rgb};

const BANNER_BG: u32 = 0x3a3320;
const BANNER_EDGE: u32 = 0x5c4f20;

pub fn banner() -> Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .bg(rgb(BANNER_BG))
        .border_b_1()
        .border_color(rgb(BANNER_EDGE))
}
