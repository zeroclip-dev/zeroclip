//! A small filled circle, for recording and microphone state.

use gpui::{Hsla, Styled, div};

pub fn status_dot(color: impl Into<Hsla>) -> gpui::Div {
    div().size_2p5().rounded_full().bg(color.into())
}
