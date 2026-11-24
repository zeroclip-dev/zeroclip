//! The Home screen: the replay buffer with its actions, the hotkeys they mirror, and the activity
//! feed. Ported from the egui version (`git show 1dad236:src/ui/home.rs`).

use gpui::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px, relative, rgb,
};
use zc_core::control::Command;
use zc_core::hotkey::Chord;
use zc_core::settings::MicMode;

use super::components::button::{button, button_ghost, button_primary};
use super::components::card::{card, kbd};
use super::components::status_dot::status_dot;
use super::{human_duration, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

pub struct HomeView {
    app: WeakEntity<ZeroClipApp>,
}

impl HomeView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app }
    }
}

impl Render for HomeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entity) = self.app.upgrade() else {
            return div().into_any_element();
        };
        let app = entity.read(cx);
        let window = app.settings.profiles.default.buffer();
        let hotkeys = &app.settings.hotkeys;
        // The push-to-talk row only appears when push-to-talk is the mic mode.
        let push_to_talk =
            hotkeys.push_to_talk.as_ref().filter(|_| app.settings.audio.mic == MicMode::PushToTalk);

        let mut rows = vec![
            hotkey_row(&hotkeys.save_clip, "Save the buffer as a clip"),
            hotkey_row(&hotkeys.toggle_recording, "Start or stop recording"),
            hotkey_row(&hotkeys.screenshot, "Screenshot"),
        ];
        rows.extend(push_to_talk.map(|ptt| hotkey_row(ptt, "Push-to-talk (hold)")));

        let record_label = if app.status.recording { "Stop recording" } else { "Start recording" };
        let fill =
            fill_ratio(app.status.buffered.as_millis(), app.status.buffer_window.as_millis());
        let feed: Vec<gpui::Div> = if app.notices.is_empty() {
            vec![div().text_color(rgb(theme::MUTED_FG)).child("Nothing yet.")]
        } else {
            app.notices
                .iter()
                .map(|notice| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER))
                        .child(status_dot(rgb(notice_color(notice.level))))
                        .child(div().flex_1().child(notice.text.clone()))
                })
                .collect()
        };

        let buffer_card =
            card("Replay buffer")
                .flex_1()
                .child(
                    div()
                        .text_size(px(34.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(human_duration(app.status.buffered)),
                )
                .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                    "of {} kept in memory",
                    human_duration(app.status.buffer_window)
                )))
                .child(
                    div().h(px(6.)).w_full().rounded_full().bg(rgb(theme::MUTED)).child(
                        div().h_full().w(relative(fill)).rounded_full().bg(rgb(theme::BRAND)),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_2()
                        .pt_2()
                        .child(
                            button_primary("home-save", "Save clip")
                                .child(kbd(hotkeys.save_clip.to_string()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    send(&this.app, cx, Command::SaveClip)
                                })),
                        )
                        .child(button("home-record", record_label).on_click(cx.listener(
                            |this, _, _, cx| send(&this.app, cx, Command::ToggleRecording),
                        )))
                        .child(button_ghost("home-shot", "Screenshot").on_click(
                            cx.listener(|this, _, _, cx| send(&this.app, cx, Command::Screenshot)),
                        )),
                );

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Clip what just happened"),
                    )
                    .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                        "ZeroClip keeps the last {} of gameplay in memory. Press a hotkey to save it.",
                        human_duration(window)
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .child(buffer_card)
                    .child(card("Hotkeys").flex_1().children(rows)),
            )
            .child(
                card("Activity").flex_1().min_h_0().child(
                    div()
                        .id("home-activity")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .text_sm()
                        .children(feed),
                ),
            )
            .into_any_element()
    }
}

/// Share of the buffer window that holds footage, `0.0..=1.0`. An unknown (zero) window reads empty.
fn fill_ratio(buffered_ms: i64, window_ms: i64) -> f32 {
    if window_ms <= 0 {
        return 0.0;
    }
    (buffered_ms.max(0) as f64 / window_ms as f64).clamp(0.0, 1.0) as f32
}

/// One line of the hotkey card: what it does, then the chord as a chip.
fn hotkey_row(chord: &Chord, what: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .py_1()
        .text_sm()
        .child(what)
        .child(kbd(chord.to_string()))
}

/// Sends `command` to the engine through the app. A failed send lands in the activity feed.
fn send(app: &WeakEntity<ZeroClipApp>, cx: &mut App, command: Command) {
    let _ = app.update(cx, |app, cx| {
        app.send(command);
        cx.notify();
    });
}

fn notice_color(level: NoticeLevel) -> u32 {
    match level {
        NoticeLevel::Info => theme::MUTED_FG,
        NoticeLevel::Warning => theme::WARN,
        NoticeLevel::Error => theme::DANGER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_is_clamped_and_safe_on_zero_window() {
        assert_eq!(fill_ratio(30_000, 60_000), 0.5);
        assert_eq!(fill_ratio(90_000, 60_000), 1.0);
        assert_eq!(fill_ratio(-5, 60_000), 0.0);
        assert_eq!(fill_ratio(10, 0), 0.0);
    }
}
