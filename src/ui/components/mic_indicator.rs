//! The "Mic live" / "Mic muted" indicator for the status bar, so the owner can see push-to-talk
//! working while playing.

use gpui::{FontWeight, IntoElement, ParentElement, Styled, div, px, rgb};
use zc_core::control::MicState;

use super::status_dot::status_dot;
use crate::ui::theme;

const RED: u32 = theme::DANGER;
const GRAY: u32 = theme::TEXT_DIM;

/// The label and dot colour for a mic state. `None` when the mic is off, so the caller adds no
/// separator for it.
pub fn mic_look(state: MicState) -> Option<(&'static str, u32)> {
    match state {
        MicState::Off => None,
        MicState::Live => Some(("Mic live", RED)),
        MicState::Muted => Some(("Mic muted", GRAY)),
    }
}

/// A dot and a label for the mic state. Returns nothing to draw when the mic is off.
pub fn mic_indicator(state: MicState) -> Option<impl IntoElement> {
    let (label, color) = mic_look(state)?;
    let is_live = state == MicState::Live;
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .text_size(px(theme::TEXT_MICRO))
        .text_color(rgb(if is_live { theme::FG_SOFT } else { theme::MUTED_FG }));
    if is_live {
        row = row.font_weight(FontWeight::MEDIUM);
    }
    Some(row.child(status_dot(rgb(color))).child(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_mic_shows_nothing() {
        assert_eq!(mic_look(MicState::Off), None);
    }

    #[test]
    fn live_and_muted_read_differently() {
        assert_eq!(mic_look(MicState::Live), Some(("Mic live", RED)));
        assert_eq!(mic_look(MicState::Muted), Some(("Mic muted", GRAY)));
    }
}
