//! The Home screen: the replay buffer with its actions, the hotkeys they mirror, and the activity
//! feed. Ported from the egui version (`git show 1dad236:src/ui/home.rs`).

use std::time::{Instant, SystemTime};

use gpui::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px, relative, rgb,
};
use zc_core::Hns;
use zc_core::control::Command;
use zc_core::hotkey::Chord;
use zc_core::settings::MicMode;

use super::components::button::{button, button_ghost, button_primary};
use super::components::card::{card, kbd};
use super::components::status_dot::status_dot;
use super::clips::{Thumbs, age, thumb_box};
use super::{human_duration, theme};
use crate::app::{NoticeLevel, Tab, ZeroClipApp};

/// How many recent clips Home shows.
const RECENT: usize = 4;
/// Width of a recent-clip tile.
const TILE_W: f32 = 200.0;

pub struct HomeView {
    app: WeakEntity<ZeroClipApp>,
    thumbs: Thumbs,
    /// The game being clipped and when it was first seen, for the banner.
    session: Option<(String, Instant, SystemTime)>,
}

impl HomeView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app, thumbs: Thumbs::default(), session: None }
    }
}

impl Render for HomeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entity) = self.app.upgrade() else {
            return div().into_any_element();
        };
        let dir = entity.read(cx).output_dir();
        entity.update(cx, |app, _| app.clips.ensure_scanned(&dir));
        let (game, recent, thumb_dir, ffmpeg_path) = {
            let app = entity.read(cx);
            (
                app.status.game.clone(),
                app.clips.recent_videos(RECENT).cloned().collect::<Vec<_>>(),
                app.paths.db_file.parent().map(|data| data.join("thumbs")),
                app.settings.editor.ffmpeg_path.clone(),
            )
        };
        // The banner's session starts when a game is first seen and ends when it goes.
        match (&game, &self.session) {
            (Some(game), Some((now, ..))) if game == now => {}
            (Some(game), _) => {
                self.session = Some((game.clone(), Instant::now(), SystemTime::now()));
            }
            (None, _) => self.session = None,
        }
        if let Some(thumb_dir) = &thumb_dir {
            self.thumbs.ensure(
                recent.iter(),
                thumb_dir,
                ffmpeg_path,
                |this: &mut Self| &mut this.thumbs,
                cx,
            );
        }
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
            vec![div().text_color(rgb(theme::MUTED_FG)).child("Nothing yet. Saved clips, screenshots and problems will be listed here.")]
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

        // Banner: the game being clipped, for how long, and the clips saved since it started.
        let banner = self.session.as_ref().map(|(name, since, wall)| {
            let saved = recent
                .iter()
                .chain(app.clips.files.iter().filter(|f| !recent.iter().any(|r| r.path == f.path)))
                .filter(|f| f.modified.is_some_and(|m| m >= *wall))
                .count();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_4()
                .py_3()
                .rounded(px(theme::RADIUS))
                .bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::BRAND))
                .child(status_dot(rgb(theme::BRAND)))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                            "Clipping {}",
                            name.trim_end_matches(".exe")
                        )))
                        .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                            "In game for {} \u{b7} {} saved this session",
                            human_duration(Hns::from_secs(since.elapsed().as_secs() as i64)),
                            match saved {
                                1 => "1 clip".to_owned(),
                                n => format!("{n} clips"),
                            }
                        ))),
                )
        });

        let tiles: Vec<gpui::Stateful<gpui::Div>> = recent
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let path = file.path.clone();
                let picture = thumb_box(
                    thumb_dir.as_ref().and_then(|dir| self.thumbs.get(dir, file)),
                    TILE_W - 18.,
                    (TILE_W - 18.) * 9. / 16.,
                );
                div()
                    .id(("home-clip", i))
                    .w(px(TILE_W))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded(px(theme::RADIUS))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ACCENT)))
                    .child(
                        div().relative().child(picture).child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_2xl()
                                .text_color(rgb(0xffffff))
                                .child("\u{25b6}"),
                        ),
                    )
                    .child(div().text_sm().truncate().child(file.name.clone()))
                    .child(div().text_xs().text_color(rgb(theme::MUTED_FG)).child(
                        file.modified.map_or_else(|| "\u{2014}".to_owned(), age),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = path.clone();
                        let _ = this.app.update(cx, |app, cx| app.open_in_editor(path, true, cx));
                    }))
            })
            .collect();
        let recent_section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Recent clips"),
                    )
                    .child(button_ghost("home-all-clips", "View all").on_click(cx.listener(
                        |this, _, _, cx| {
                            let _ = this.app.update(cx, |app, cx| {
                                app.tab = Tab::Clips;
                                cx.notify();
                            });
                        },
                    ))),
            )
            .child(if tiles.is_empty() {
                div()
                    .p_4()
                    .rounded(px(theme::RADIUS))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .text_sm()
                    .text_color(rgb(theme::MUTED_FG))
                    .child("No clips yet. Press the save hotkey to keep what just happened.")
            } else {
                div().flex().flex_row().flex_wrap().gap_3().children(tiles)
            });

        div()
            .id("home")
            .size_full()
            .overflow_y_scroll()
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
            .children(banner)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .child(buffer_card)
                    .child(card("Hotkeys").flex_1().children(rows)),
            )
            .child(recent_section)
            .child(
                card("Activity").child(
                    div()
                        .id("home-activity")
                        .max_h(px(220.))
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
