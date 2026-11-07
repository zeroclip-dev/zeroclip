//! The "Mic live" / "Mic muted" indicator for the status bar, so the owner can see push-to-talk
//! working while playing.

use gpui::{IntoElement, ParentElement, Styled, div, rgb};
use zc_core::control::MicState;

use super::status_dot::status_dot;

const RED: u32 = 0xf08080;
const GRAY: u32 = 0x909090;

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
    Some(div().flex().flex_row().items_center().gap_1().child(status_dot(rgb(color))).child(label))
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
