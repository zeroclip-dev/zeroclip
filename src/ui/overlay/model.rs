//! The pure parts of the toast overlay: what a toast says, where it sits and how long it lives.
//! No GPUI and no Win32 here, so these run in CI.

use std::time::Duration;

use zc_core::Hns;

use crate::ui::human_duration;
use crate::ui::theme;

/// Time at full opacity, after the fade-in and before the fade-out.
pub const HOLD: Duration = Duration::from_millis(2_600);
/// Fade-in and fade-out each. Hold + two fades is about 3 s on screen.
pub const FADE: Duration = Duration::from_millis(200);

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
    /// A moment of the running recording was marked, `at` into it.
    Bookmarked { at: Hns },
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
        ToastKind::Bookmarked { at } => ToastText {
            title: "Bookmarked".into(),
            detail: Some(format!("{} into the recording", human_duration(at))),
        },
    }
}

/// The colour of the status dot. Red marks recording, as it does in the top bar.
pub fn accent(kind: ToastKind) -> u32 {
    match kind {
        ToastKind::Saving | ToastKind::ClipSaved { .. } => theme::BRAND,
        ToastKind::RecordingStarted => theme::DANGER,
        ToastKind::RecordingStopped { .. } => theme::MUTED_FG,
        ToastKind::ScreenshotTaken => theme::FG,
        ToastKind::Bookmarked { .. } => theme::WARN,
    }
}

/// The 1 px hairline border around the floating toast card, subtly tinted for active states.
pub fn border(kind: ToastKind) -> u32 {
    match kind {
        ToastKind::Saving | ToastKind::ClipSaved { .. } => theme::BRAND_BORDER,
        ToastKind::RecordingStarted => theme::DANGER_BORDER,
        ToastKind::Bookmarked { .. } => theme::WARN_BORDER,
        ToastKind::RecordingStopped { .. } | ToastKind::ScreenshotTaken => theme::BORDER_STRONG,
    }
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
    fn toast_lives_about_three_seconds() {
        assert_eq!(HOLD + FADE * 2, Duration::from_secs(3));
    }

    #[test]
    fn border_matches_semantic_state() {
        assert_eq!(border(ToastKind::Saving), theme::BRAND_BORDER);
        assert_eq!(
            border(ToastKind::ClipSaved { length: Hns::from_secs(30) }),
            theme::BRAND_BORDER
        );
        assert_eq!(border(ToastKind::RecordingStarted), theme::DANGER_BORDER);
        assert_eq!(border(ToastKind::Bookmarked { at: Hns::from_secs(10) }), theme::WARN_BORDER);
        assert_eq!(
            border(ToastKind::RecordingStopped { length: Hns::from_secs(30) }),
            theme::BORDER_STRONG
        );
        assert_eq!(border(ToastKind::ScreenshotTaken), theme::BORDER_STRONG);
    }
}
