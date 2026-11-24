//! The pure parts of the toast overlay: what a toast says, where it sits and how long it lives.
//! No GPUI and no Win32 here, so these run in CI.

use std::time::Duration;

use zc_core::Hns;

use crate::ui::human_duration;
use crate::ui::theme;

/// Card size in logical pixels.
pub const CARD_W: f32 = 300.0;
pub const CARD_H: f32 = 60.0;
/// Gap from the screen's left edge.
pub const SIDE_MARGIN: f32 = 24.0;
/// Gap from the screen's top edge.
pub const TOP_MARGIN: f32 = 24.0;
/// Time at full opacity, after the fade-in and before the fade-out.
pub const HOLD: Duration = Duration::from_millis(2_600);
/// Fade-in and fade-out each. Hold + two fades is about 3 s on screen.
pub const FADE: Duration = Duration::from_millis(200);
/// The shimmer sweep, once, when the card appears.
pub const SHIMMER: Duration = Duration::from_millis(600);
/// Width of the light band that sweeps across the card.
pub const SHIMMER_BAND: f32 = 80.0;

/// The three events the overlay reports. Each variant carries only what its text needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    /// The save hotkey was pressed; the clip is being written.
    Saving,
    /// A DVR clip was written; `length` is the real length of that clip.
    ClipSaved { length: Hns },
    /// A manual or session recording started.
    RecordingStarted,
    /// A manual or session recording finished; `length` is how long it ran.
    RecordingStopped { length: Hns },
    /// A screenshot was written.
    ScreenshotTaken,
}

/// The two lines on a card. `detail` is optional.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastText {
    pub title: String,
    pub detail: Option<String>,
}

/// What the card says.
pub fn toast_text(kind: ToastKind) -> ToastText {
    match kind {
        ToastKind::Saving => ToastText { title: "Saving clip…".into(), detail: None },
        ToastKind::ClipSaved { length } => ToastText {
            title: format!("Clipped last {}", human_duration(length)),
            detail: Some("Saved to Clips".into()),
        },
        ToastKind::RecordingStarted => {
            ToastText { title: "Recording started".into(), detail: None }
        }
        ToastKind::RecordingStopped { length } => ToastText {
            title: "Recording stopped".into(),
            detail: Some(format!("{} recorded", human_duration(length))),
        },
        ToastKind::ScreenshotTaken => ToastText { title: "Screenshot saved".into(), detail: None },
    }
}

/// The colour of the status dot. Red marks recording, as it does in the top bar.
pub fn accent(kind: ToastKind) -> u32 {
    match kind {
        ToastKind::Saving | ToastKind::ClipSaved { .. } => theme::BRAND,
        ToastKind::RecordingStarted => theme::DANGER,
        ToastKind::RecordingStopped { .. } => theme::MUTED_FG,
        ToastKind::ScreenshotTaken => theme::FG,
    }
}

/// A rectangle in logical pixels, for the display's bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Top-left corner of the card when it sits in the top-left of `area`.
pub fn top_left_origin(area: Rect) -> (f32, f32) {
    (area.x + SIDE_MARGIN, area.y + TOP_MARGIN)
}

/// Left edge of the shimmer band for `progress` in 0..=1. It starts just off the left edge
/// and ends just past the right edge, so it is never visible at either end.
pub fn shimmer_left(progress: f32, card_w: f32, band_w: f32) -> f32 {
    -band_w + (card_w + band_w) * progress.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_text_names_the_real_length() {
        let text = toast_text(ToastKind::ClipSaved { length: Hns::from_secs(20 * 60) });
        assert_eq!(text.title, "Clipped last 20 min");
        let short = toast_text(ToastKind::ClipSaved { length: Hns::from_secs(45) });
        assert_eq!(short.title, "Clipped last 45 s");
    }

    #[test]
    fn each_event_has_its_own_text() {
        assert_eq!(toast_text(ToastKind::Saving).title, "Saving clip…");
        assert_eq!(toast_text(ToastKind::RecordingStarted).title, "Recording started");
        let stopped = toast_text(ToastKind::RecordingStopped { length: Hns::from_secs(90) });
        assert_eq!(stopped.title, "Recording stopped");
        assert_eq!(stopped.detail.as_deref(), Some("1 min 30 s recorded"));
        assert_eq!(toast_text(ToastKind::ScreenshotTaken).title, "Screenshot saved");
    }

    #[test]
    fn card_sits_top_left_inside_the_margins() {
        let screen = Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
        let (x, y) = top_left_origin(screen);
        assert_eq!(x, SIDE_MARGIN);
        assert_eq!(y, TOP_MARGIN);
        // A second monitor that starts to the right moves the card with it.
        let right = Rect { x: 1920.0, ..screen };
        assert_eq!(top_left_origin(right).0, 1920.0 + SIDE_MARGIN);
    }

    #[test]
    fn shimmer_sweeps_from_hidden_left_to_hidden_right() {
        assert_eq!(shimmer_left(0.0, CARD_W, SHIMMER_BAND), -SHIMMER_BAND);
        assert_eq!(shimmer_left(1.0, CARD_W, SHIMMER_BAND), CARD_W);
        assert!(shimmer_left(0.5, CARD_W, SHIMMER_BAND) > -SHIMMER_BAND);
        assert_eq!(shimmer_left(2.0, CARD_W, SHIMMER_BAND), CARD_W, "clamped past the end");
    }

    #[test]
    fn toast_lives_about_three_seconds() {
        assert_eq!(HOLD + FADE * 2, Duration::from_secs(3));
    }
}
