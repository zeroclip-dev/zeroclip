//! The Home screen. One focal point (the replay buffer and the save action), then the latest
//! clips as large tiles, then the shortcuts and the activity feed. Right-click, or the "..." that
//! appears on hover, opens the same clip menu as the Clips screen (`ui/clip_menu.rs`).

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ObjectFit, ParentElement,
    Render, Role, StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window,
    div, img, px, relative, rgb, rgba, transparent_black,
};
use zc_core::Hns;
use zc_core::control::Command;
use zc_core::hotkey::Chord;
use zc_core::settings::MicMode;

use super::art::{ArtCache, ArtRequest, clean_game_stem};
use super::clip_menu::{more_button, on_right_click};
use super::clips::{ClipFile, Kind, Thumbs, age, display_name};
use super::components::button::{button, button_danger, button_ghost, button_hero};
use super::components::card::{card, kbd};
use super::components::game_banner::{BannerStatus, GameBannerProps, game_banner};
use super::components::page_bg::page_background;
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
/// Rolling 24-hour window used for the banner's "today" clip count.
const TODAY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

pub struct HomeView {
    app: WeakEntity<ZeroClipApp>,
    thumbs: Thumbs,
    art: ArtCache,
    /// The game being clipped and when it was first seen, for the banner's session state.
    session: Option<(String, Instant, SystemTime)>,
    /// When the current manual/session recording started, for the banner's live elapsed counter.
    recording_since: Option<Instant>,
}

impl HomeView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            thumbs: Thumbs::default(),
            art: ArtCache::default(),
            session: None,
            recording_since: None,
        }
    }

    /// One recent clip: a 16:9 thumbnail that shows a play button and "..." on hover (and keeps
    /// "..." visible under keyboard focus), plus a title and quiet metadata line. Click watches
    /// it; right-click or "..." opens the clip menu.
    fn tile(
        &self,
        i: usize,
        file: &ClipFile,
        thumb: Option<PathBuf>,
        tile_w: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pic_w = (tile_w - TILE_CHROME).max(1.0);
        let pic_h = (pic_w * 9. / 16.).floor().max(1.0);
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
            .rounded(px(theme::RADIUS_SM))
            .overflow_hidden()
            .bg(rgb(theme::SURFACE_INSET));
        if let Some(src) = thumb {
            picture =
                picture.child(img(src).w(px(pic_w)).h(px(pic_h)).object_fit(ObjectFit::Cover));
        } else {
            picture = picture.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icons::icon("film", theme::TEXT_DIM)),
            );
        }
        let picture = picture
            .child(
                // The play button appears on hover so resting thumbnails stay uncluttered.
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
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgba(theme::SCRIM))
                            .border_1()
                            .border_color(rgb(theme::BORDER_STRONG))
                            .child(icons::icon("play", theme::FG)),
                    ),
            )
            .child(
                // Placed directly without an opacity-0 wrapper so keyboard focus on the button
                // itself reveals it cleanly with its focus ring and zero layout shift.
                div().absolute().top(px(6.)).right(px(6.)).child(
                    more_button(("home-more", i), &self.app, &file.path, 28.)
                        .bg(rgba(theme::SCRIM))
                        .border_1()
                        .border_color(transparent_black())
                        .opacity(0.)
                        .group_hover(group.clone(), |s| s.opacity(1.))
                        .hover(|s| {
                            s.opacity(1.)
                                .bg(rgb(theme::SURFACE_RAISED))
                                .border_color(rgb(theme::BORDER_STRONG))
                        })
                        .active(|s| s.opacity(1.).bg(rgb(theme::SURFACE_ACTIVE)))
                        .focus_visible(|s| {
                            s.opacity(1.)
                                .bg(rgb(theme::SURFACE_RAISED))
                                .border_color(rgb(theme::RING))
                        }),
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
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)).border_color(rgb(theme::BORDER_STRONG)))
            .active(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
            .child(picture)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px_1()
                    .pb(px(2.))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .line_height(px(18.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .line_height(px(16.))
                            .text_color(rgb(theme::MUTED_FG))
                            .truncate()
                            .child(sub),
                    ),
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
        let (
            game,
            recording,
            capturing,
            recent_game,
            last_clip_game,
            recent,
            thumb_dir,
            icons_dir,
            ffmpeg_path,
        ) = {
            let app = entity.read(cx);
            (
                app.status.game.clone(),
                app.status.recording,
                app.status.capturing,
                app.status.recent_games.first().cloned(),
                app.clips.files.iter().find_map(|f| f.game.clone()),
                app.clips.recent_videos(RECENT).cloned().collect::<Vec<_>>(),
                app.paths.db_file.parent().map(|data| data.join("thumbs")),
                app.paths.icons_dir.clone(),
                app.settings.editor.ffmpeg_path.clone(),
            )
        };
        // The session starts when a game is first seen and ends when it goes.
        match (&game, &self.session) {
            (Some(game), Some((now, ..))) if game == now => {}
            (Some(game), _) => {
                self.session = Some((game.clone(), Instant::now(), SystemTime::now()));
            }
            (None, _) => self.session = None,
        }
        let recording_elapsed = if recording {
            let since = *self.recording_since.get_or_insert_with(Instant::now);
            Some(Hns::from_secs(since.elapsed().as_secs() as i64))
        } else {
            self.recording_since = None;
            None
        };

        // Banner target: active game, else most recently detected game, else last recorded game.
        let banner_target: Option<(String, bool)> = game
            .clone()
            .map(|g| (g, true))
            .or_else(|| recent_game.map(|g| (g, false)))
            .or_else(|| last_clip_game.map(|g| (g, false)));

        let banner_clip: Option<ClipFile> = banner_target.as_ref().and_then(|(target, _)| {
            let app = entity.read(cx);
            let stem = clean_game_stem(target);
            app.clips
                .files
                .iter()
                .find(|f| {
                    f.game.as_deref().is_some_and(|g| clean_game_stem(g).eq_ignore_ascii_case(stem))
                })
                .cloned()
        });

        if let Some(thumb_dir) = &thumb_dir {
            self.thumbs.ensure(
                recent.iter().chain(banner_clip.iter()),
                thumb_dir,
                ffmpeg_path,
                |this: &mut Self| &mut this.thumbs,
                cx,
            );
        }

        let banner_clip_thumb: Option<PathBuf> = banner_clip.as_ref().and_then(|clip| {
            if Kind::Videos.accepts(&clip.name) {
                thumb_dir.as_ref().and_then(|dir| self.thumbs.get(dir, clip))
            } else {
                Some(clip.path.clone())
            }
        });
        let resolved_art = banner_target.as_ref().and_then(|(target, _)| {
            self.art
                .get_or_load(
                    ArtRequest {
                        game: target.clone(),
                        clip_thumb: banner_clip_thumb.clone(),
                        icons_dir: icons_dir.clone(),
                    },
                    |this: &mut Self| &mut this.art,
                    cx,
                )
                .cloned()
        });

        // Tiles share the width evenly: as many columns as fit, two rows of them.
        let page_w = (f32::from(window.viewport_size().width).min(PAGE_MAX_W) - 2. * PAGE_PAD)
            .max(TILE_MIN_W);
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

        let now = SystemTime::now();
        let clips_today = count_clips_since(&app.clips.files, now, TODAY_WINDOW);
        let banner_status = if let Some(elapsed) = recording_elapsed {
            BannerStatus::Recording { elapsed }
        } else if capturing {
            let elapsed = self
                .session
                .as_ref()
                .map(|(_, since, _)| Hns::from_secs(since.elapsed().as_secs() as i64));
            BannerStatus::Armed { elapsed }
        } else {
            BannerStatus::Idle
        };

        // No game known (clips from before games were tagged): show the newest clip's frame.
        let latest_art = thumb_dir
            .as_ref()
            .zip(recent.first())
            .and_then(|(dir, file)| self.thumbs.get(dir, file))
            .map(super::art::ArtSource::ClipThumb);
        let banner_el = {
            let today_label = match clips_today {
                0 => "No clips today".to_owned(),
                1 => "1 clip today".to_owned(),
                n => format!("{n} clips today"),
            };
            match &banner_target {
                Some((target, is_active)) => {
                    let stem = clean_game_stem(target);
                    let title = resolved_art
                        .as_ref()
                        .and_then(|a| a.display_name.clone())
                        .unwrap_or_else(|| stem.to_owned());
                    let game_clips = app
                        .clips
                        .files
                        .iter()
                        .filter(|f| {
                            f.game
                                .as_deref()
                                .is_some_and(|g| clean_game_stem(g).eq_ignore_ascii_case(stem))
                        })
                        .count();
                    let eyebrow = if recording && *is_active {
                        "Recording now"
                    } else if *is_active {
                        "Now playing"
                    } else {
                        "Last played"
                    };
                    let meta =
                        if let Some((_, _, wall)) = self.session.as_ref().filter(|_| *is_active) {
                            let session_saved = app
                                .clips
                                .files
                                .iter()
                                .filter(|f| f.modified.is_some_and(|m| m >= *wall))
                                .count();
                            if session_saved > 0 {
                                format!("{session_saved} this session \u{b7} {today_label}")
                            } else {
                                today_label
                            }
                        } else if let Some(when) =
                            banner_clip.as_ref().and_then(|c| c.modified).map(age)
                        {
                            format!("{today_label} \u{b7} Last clip {when}")
                        } else {
                            today_label
                        };
                    let badge_right = (game_clips > 0).then(|| plural_clips(game_clips));
                    game_banner(GameBannerProps {
                        title,
                        eyebrow,
                        status: banner_status,
                        meta,
                        badge_right,
                        art: resolved_art.map(|a| a.source),
                        empty_state: false,
                    })
                }
                None => {
                    let meta = if clips_today > 0 {
                        format!("{today_label} \u{b7} Launch a game to start automatic clipping")
                    } else {
                        "Launch a game to start automatic clipping".to_owned()
                    };
                    game_banner(GameBannerProps {
                        title: "ZeroClip".to_owned(),
                        eyebrow: "Ready to clip",
                        status: banner_status,
                        meta,
                        badge_right: None,
                        art: latest_art,
                        empty_state: true,
                    })
                }
            }
        };

        // The hero: what the buffer holds, and the one action that matters.
        let fill =
            fill_ratio(app.status.buffered.as_millis(), app.status.buffer_window.as_millis());
        let record_button = if app.status.recording {
            button_danger("home-record", "Stop recording")
        } else {
            button("home-record", "Start recording")
        }
        .on_click(cx.listener(|this, _, _, cx| send(&this.app, cx, Command::ToggleRecording)));

        let hero =
            div()
                .flex()
                .flex_col()
                .gap_4()
                .p_5()
                .rounded(px(theme::RADIUS_LG))
                .bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    div().flex().flex_row().flex_wrap().items_center().gap_2().child(
                        div()
                            .flex_1()
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::MUTED_FG))
                            .child("Replay buffer"),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .items_baseline()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(36.))
                                        .line_height(px(40.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(theme::FG))
                                        .child(human_duration(app.status.buffered)),
                                )
                                .child(
                                    div()
                                        .text_size(px(theme::TEXT_BODY))
                                        .text_color(rgb(theme::MUTED_FG))
                                        .child(format!(
                                            "of {} kept in memory",
                                            human_duration(app.status.buffer_window)
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .h(px(6.))
                                .w_full()
                                .rounded_full()
                                .overflow_hidden()
                                .bg(rgb(theme::SURFACE_INSET))
                                .border_1()
                                .border_color(rgb(theme::BORDER_SUBTLE))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(fill))
                                        .rounded_full()
                                        .bg(rgb(theme::BRAND)),
                                ),
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
                            button_hero("home-save", "Save clip")
                                .child(kbd(hotkeys.save_clip.to_string()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    send(&this.app, cx, Command::SaveClip)
                                })),
                        )
                        .child(record_button)
                        .child(button_ghost("home-shot", "Take screenshot").on_click(
                            cx.listener(|this, _, _, cx| send(&this.app, cx, Command::Screenshot)),
                        ))
                        .child(div().flex_1())
                        .child(theme::caption(format!(
                            "Saves the last {}",
                            human_duration(buffer_window)
                        ))),
                );

        // Latest clips: rows of equal tiles, or an empty state that invites the next action.
        let recent_body =
            if tiles.is_empty() {
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .py_8()
                    .px_6()
                    .rounded(px(theme::RADIUS))
                    .bg(rgb(theme::CARD))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        div()
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(theme::RADIUS_MD))
                            .bg(rgb(theme::SURFACE_RAISED))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .child(icons::icon("film", theme::MUTED_FG)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(theme::TEXT_HEADING))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::FG))
                                    .child("No clips yet"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(6.))
                                    .text_size(px(theme::TEXT_CAPTION))
                                    .text_color(rgb(theme::MUTED_FG))
                                    .child("Press")
                                    .child(kbd(hotkeys.save_clip.to_string()))
                                    .child("or save the buffer to capture your first clip."),
                            ),
                    )
                    .child(button("home-empty-save", "Save clip").on_click(
                        cx.listener(|this, _, _, cx| send(&this.app, cx, Command::SaveClip)),
                    ))
            } else {
                div().flex().flex_row().flex_wrap().gap(px(TILE_GAP)).children(tiles)
            };
        let recent_section = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(theme::section_heading("Recent clips").flex_1())
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

        let mut raw_rows = vec![
            (&hotkeys.save_clip, "Save clip"),
            (&hotkeys.toggle_recording, "Start or stop recording"),
            (&hotkeys.screenshot, "Take screenshot"),
            (&hotkeys.bookmark, "Bookmark moment while recording"),
        ];
        if let Some(ptt) = push_to_talk {
            raw_rows.push((ptt, "Push-to-talk (hold)"));
        }
        let rows: Vec<_> = raw_rows
            .into_iter()
            .enumerate()
            .map(|(i, (chord, what))| {
                hotkey_row(chord, what)
                    .when(i > 0, |d| d.border_t_1().border_color(rgb(theme::BORDER_SUBTLE)))
            })
            .collect();

        let feed: Vec<gpui::Div> = if app.notices.is_empty() {
            vec![theme::caption("Saved clips, screenshots, and notices appear here.")]
        } else {
            app.notices
                .iter()
                .enumerate()
                .map(|(i, notice)| {
                    let text_color = match notice.level {
                        NoticeLevel::Info => theme::FG_SOFT,
                        NoticeLevel::Warning | NoticeLevel::Error => theme::FG,
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .py(px(6.))
                        .when(i > 0, |d| d.border_t_1().border_color(rgb(theme::BORDER_SUBTLE)))
                        .child(status_dot(rgb(notice_color(notice.level))))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::TEXT_BODY))
                                .text_color(rgb(text_color))
                                .child(notice.text.clone()),
                        )
                })
                .collect()
        };
        let lower = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_4()
            .child(
                card("Shortcuts")
                    .flex_1()
                    .min_w(px(320.))
                    .child(div().flex().flex_col().children(rows)),
            )
            .child(
                card("Activity").flex_1().min_w(px(320.)).child(
                    div()
                        .id("home-activity")
                        .max_h(px(200.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
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
            .gap_5()
            .px(px(PAGE_PAD))
            .py(px(20.))
            .child(banner_el)
            .child(hero)
            .child(recent_section)
            .child(lower);
        div()
            .relative()
            .size_full()
            .bg(rgb(theme::BG))
            .overflow_hidden()
            .child(page_background())
            .child(div().id("home").relative().size_full().overflow_y_scroll().child(content))
            .into_any_element()
    }
}

/// Counts clips whose modification time falls within `window` before `now` (future timestamps are
/// treated as within the window).
fn count_clips_since(files: &[ClipFile], now: SystemTime, window: Duration) -> usize {
    files
        .iter()
        .filter(|f| {
            f.modified.is_some_and(|m| match now.duration_since(m) {
                Ok(elapsed) => elapsed <= window,
                Err(_) => true,
            })
        })
        .count()
}

fn plural_clips(count: usize) -> String {
    match count {
        1 => "1 clip".to_owned(),
        n => format!("{n} clips"),
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
        .gap_3()
        .py(px(6.))
        .text_size(px(theme::TEXT_BODY))
        .text_color(rgb(theme::FG_SOFT))
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

    #[test]
    fn count_clips_since_filters_by_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let mk = |modified| ClipFile {
            name: "clip.mp4".into(),
            path: PathBuf::from("clip.mp4"),
            size_bytes: 1,
            modified,
            game: Some("Deadlock".into()),
        };
        let files = vec![
            mk(Some(now - Duration::from_secs(60))),
            mk(Some(now - Duration::from_secs(23 * 3_600))),
            mk(Some(now - Duration::from_secs(25 * 3_600))),
            mk(Some(now + Duration::from_secs(10))),
            mk(None),
        ];
        assert_eq!(count_clips_since(&files, now, TODAY_WINDOW), 3);
        assert_eq!(plural_clips(1), "1 clip");
        assert_eq!(plural_clips(4), "4 clips");
    }
}
