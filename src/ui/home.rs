//! The Home screen. One focal point (the replay buffer and the save action), then the latest
//! clips as large tiles, then the shortcuts and the activity feed. Right-click, or the "..." that
//! appears on hover, opens the same clip menu as the Clips screen (`ui/clip_menu.rs`).

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use gpui::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ObjectFit, ParentElement,
    Render, Role, StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window,
    div, img, px, relative, rgb, rgba,
};
use zc_core::Hns;
use zc_core::control::Command;
use zc_core::hotkey::Chord;
use zc_core::settings::MicMode;

use super::clip_menu::{more_button, on_right_click};
use super::clips::{ClipFile, Thumbs, age, display_name};
use super::components::button::{button, button_ghost, button_primary};
use super::components::card::{card, kbd};
use super::components::status_dot::status_dot;
use super::{human_duration, icons, theme};
use crate::app::{NoticeLevel, Tab, ZeroClipApp};

/// Most clips Home asks for; it shows two rows of whatever fits the width.
const RECENT: usize = 8;
/// Widest the page gets, so lines stay readable on a big monitor.
const PAGE_MAX_W: f32 = 1120.0;
/// Page padding on each side, and the gap between tiles.
const PAGE_PAD: f32 = 24.0;
const TILE_GAP: f32 = 12.0;
/// Narrowest a tile gets before a column is dropped.
const TILE_MIN_W: f32 = 200.0;
/// Space a tile spends around its picture: padding 8 + border 1, on both sides.
const TILE_CHROME: f32 = 18.0;
/// The dark chip behind the play button and the "..." on a picture.
const SCRIM: u32 = 0x000000a6;

pub struct HomeView {
    app: WeakEntity<ZeroClipApp>,
    thumbs: Thumbs,
    /// The game being clipped and when it was first seen, for the hero's game chip.
    session: Option<(String, Instant, SystemTime)>,
}

impl HomeView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app, thumbs: Thumbs::default(), session: None }
    }

    /// One recent clip: a picture that shows a play button on hover, and two quiet lines. Click
    /// watches it; right-click or the "..." opens the clip menu.
    fn tile(
        &self,
        i: usize,
        file: &ClipFile,
        thumb: Option<PathBuf>,
        tile_w: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pic_w = tile_w - TILE_CHROME;
        let pic_h = (pic_w * 9. / 16.).floor();
        let group = format!("home-tile-{i}");
        let path = file.path.clone();
        let name = display_name(&file.name);
        let when = file.modified.map_or_else(|| "Unknown date".to_owned(), age);
        let sub = match &file.game {
            Some(game) => format!("{} \u{b7} {when}", game.trim_end_matches(".exe")),
            None => when,
        };
        let mut picture = div()
            .relative()
            .w(px(pic_w))
            .h(px(pic_h))
            .rounded(px(theme::RADIUS_MD))
            .overflow_hidden()
            .bg(rgb(theme::ACCENT));
        if let Some(src) = thumb {
            picture =
                picture.child(img(src).w(px(pic_w)).h(px(pic_h)).object_fit(ObjectFit::Cover));
        }
        let picture = picture
            .child(
                // The play button fades in on hover, so a resting tile is just the picture.
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(0.)
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .child(
                        div()
                            .size(px(44.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgba(SCRIM))
                            .child(icons::icon("play", 0xffffff)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .opacity(0.)
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .child(
                        more_button(("home-more", i), &self.app, &file.path, 28.).bg(rgba(SCRIM)),
                    ),
            );
        let tile = div()
            .id(("home-clip", i))
            .group(group)
            .tab_stop(true)
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .role(Role::Button)
            .aria_label(format!("Watch {name}"))
            .w(px(tile_w))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(theme::RADIUS))
            .border_1()
            .border_color(rgb(theme::BG))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::CARD)).border_color(rgb(theme::BORDER)))
            .child(picture)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px_1()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(name))
                    .child(div().text_xs().text_color(rgb(theme::MUTED_FG)).truncate().child(sub)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let path = path.clone();
                let _ = this.app.update(cx, |app, cx| app.play_clip(path, cx));
            }));
        on_right_click(tile, &self.app, &file.path)
    }
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        // The game chip's session starts when a game is first seen and ends when it goes.
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
        // Tiles share the width evenly: as many columns as fit, two rows of them.
        let page_w = f32::from(window.viewport_size().width).min(PAGE_MAX_W) - 2. * PAGE_PAD;
        let cols = (((page_w + TILE_GAP) / (TILE_MIN_W + TILE_GAP)) as usize).clamp(2, 4);
        let tile_w = ((page_w - TILE_GAP * (cols - 1) as f32) / cols as f32).floor();

        let tiles: Vec<_> = recent
            .iter()
            .take(cols * 2)
            .enumerate()
            .map(|(i, file)| {
                let thumb = thumb_dir.as_ref().and_then(|dir| self.thumbs.get(dir, file));
                self.tile(i, file, thumb, tile_w, cx)
            })
            .collect();

        let app = entity.read(cx);
        let hotkeys = &app.settings.hotkeys;
        let buffer_window = app.settings.profiles.default.buffer();
        // The push-to-talk row only appears when push-to-talk is the mic mode.
        let push_to_talk =
            hotkeys.push_to_talk.as_ref().filter(|_| app.settings.audio.mic == MicMode::PushToTalk);

        // The hero: what the buffer holds, and the one action that matters.
        let fill =
            fill_ratio(app.status.buffered.as_millis(), app.status.buffer_window.as_millis());
        let record_label = if app.status.recording { "Stop recording" } else { "Start recording" };
        let game_chip = self.session.as_ref().map(|(name, since, wall)| {
            let saved =
                app.clips.files.iter().filter(|f| f.modified.is_some_and(|m| m >= *wall)).count();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .rounded_full()
                .bg(rgb(theme::BRAND_SOFT))
                .text_sm()
                .text_color(rgb(theme::BRAND))
                .child(icons::icon("gamepad-2", theme::BRAND))
                .child(format!(
                    "{} for {}, {} saved",
                    name.trim_end_matches(".exe"),
                    human_duration(Hns::from_secs(since.elapsed().as_secs() as i64)),
                    match saved {
                        1 => "1 clip".to_owned(),
                        n => format!("{n} clips"),
                    }
                ))
        });
        let recording_chip = app.status.recording.then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(rgb(theme::DANGER))
                .child(status_dot(rgb(theme::DANGER)))
                .child("Recording")
        });
        let hero =
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_5()
                .rounded(px(theme::RADIUS))
                .bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(rgb(theme::MUTED_FG))
                                .child("Replay buffer"),
                        )
                        .children(recording_chip)
                        .children(game_chip),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(44.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(human_duration(app.status.buffered)),
                        )
                        .child(div().text_color(rgb(theme::MUTED_FG)).child(format!(
                            "of {} kept in memory",
                            human_duration(app.status.buffer_window)
                        ))),
                )
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
                        .items_center()
                        .gap_2()
                        .pt_1()
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
                        ))
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::MUTED_FG))
                                .child(format!("Saves the last {}", human_duration(buffer_window))),
                        ),
                );

        // Latest clips: rows of equal tiles, or an empty state that says how to make the first.
        let recent_body = if tiles.is_empty() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .py_8()
                .rounded(px(theme::RADIUS))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(icons::icon("film", theme::MUTED_FG))
                .child(div().font_weight(FontWeight::MEDIUM).child("No clips yet"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .text_color(rgb(theme::MUTED_FG))
                        .child("Press")
                        .child(kbd(hotkeys.save_clip.to_string()))
                        .child("to keep what just happened."),
                )
        } else {
            div().flex().flex_row().flex_wrap().gap(px(TILE_GAP)).children(tiles)
        };
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
                            .text_lg()
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
            .child(recent_body);

        let mut rows = vec![
            hotkey_row(&hotkeys.save_clip, "Save the buffer as a clip"),
            hotkey_row(&hotkeys.toggle_recording, "Start or stop recording"),
            hotkey_row(&hotkeys.screenshot, "Screenshot"),
            hotkey_row(&hotkeys.bookmark, "Bookmark this moment while recording"),
        ];
        rows.extend(push_to_talk.map(|ptt| hotkey_row(ptt, "Push-to-talk (hold)")));
        let feed: Vec<gpui::Div> =
            if app.notices.is_empty() {
                vec![div().text_color(rgb(theme::MUTED_FG)).child(
                    "Nothing yet. Saved clips, screenshots and problems will be listed here.",
                )]
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
        let lower = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_4()
            .child(card("Shortcuts").flex_1().min_w(px(320.)).children(rows))
            .child(
                card("Activity").flex_1().min_w(px(320.)).child(
                    div()
                        .id("home-activity")
                        .max_h(px(200.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .text_sm()
                        .children(feed),
                ),
            );

        // The scroll box has no padding of its own (bottom padding of a scroll box is dropped), so
        // the padded column sits inside it.
        let content = div()
            .w_full()
            .max_w(px(PAGE_MAX_W))
            .mx_auto()
            .flex()
            .flex_col()
            .gap_6()
            .p(px(PAGE_PAD))
            .child(hero)
            .child(recent_section)
            .child(lower);
        div().id("home").size_full().overflow_y_scroll().child(content).into_any_element()
    }
}

/// Share of the buffer window that holds footage, `0.0..=1.0`. An unknown (zero) window reads empty.
fn fill_ratio(buffered_ms: i64, window_ms: i64) -> f32 {
    if window_ms <= 0 {
        return 0.0;
    }
    (buffered_ms.max(0) as f64 / window_ms as f64).clamp(0.0, 1.0) as f32
}

/// One line of the shortcuts card: what it does, then the chord as a chip.
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
