//! The Home screen: the hotkeys, the three actions they mirror, and the activity feed. Ported from
//! the egui version (`git show 1dad236:src/ui/home.rs`).

use gpui::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px, rgb,
};
use zc_core::control::Command;
use zc_core::hotkey::Chord;
use zc_core::settings::MicMode;

use super::components::button::button;
use super::{GRAY, RED, human_duration};
use crate::app::{NoticeLevel, ZeroClipApp};

/// Warm yellow for warnings. Info uses the grey the rest of the UI uses.
const WARNING: u32 = 0xe6be5a;

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
        let feed: Vec<gpui::Div> = if app.notices.is_empty() {
            vec![div().text_color(rgb(GRAY)).child("Nothing yet.")]
        } else {
            app.notices
                .iter()
                .map(|notice| {
                    div().text_color(rgb(notice_color(notice.level))).child(notice.text.clone())
                })
                .collect()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Clip what just happened"))
            .child(format!(
                "ZeroClip keeps the last {} of gameplay in memory. Press a hotkey to save it.",
                human_duration(window)
            ))
            .child(div().flex().flex_col().gap_1().children(rows))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(button("home-save", "Save clip").on_click(
                        cx.listener(|this, _, _, cx| send(&this.app, cx, Command::SaveClip)),
                    ))
                    .child(button("home-record", record_label).on_click(
                        cx.listener(|this, _, _, cx| send(&this.app, cx, Command::ToggleRecording)),
                    ))
                    .child(button("home-shot", "Screenshot").on_click(
                        cx.listener(|this, _, _, cx| send(&this.app, cx, Command::Screenshot)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(0x3a3a42))
                    .child(div().font_weight(FontWeight::BOLD).child("Activity"))
                    .child(
                        div()
                            .id("home-activity")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(feed),
                    ),
            )
            .into_any_element()
    }
}

/// One line of the hotkey table: the chord, then what it does.
fn hotkey_row(chord: &Chord, what: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap_6()
        .child(div().w(px(120.)).font_weight(FontWeight::BOLD).child(chord.to_string()))
        .child(what)
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
        NoticeLevel::Info => GRAY,
        NoticeLevel::Warning => WARNING,
        NoticeLevel::Error => RED,
    }
}
