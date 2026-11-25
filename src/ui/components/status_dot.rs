//! A small filled circle, for recording and microphone state.

use gpui::{Hsla, Styled, div, px};

pub fn status_dot(color: impl Into<Hsla>) -> gpui::Div {
    div().size(px(8.)).flex_shrink_0().rounded_full().bg(color.into())
}
