//! The Profiles screen (DESIGN §7): how much footage to keep, and whether to record the whole
//! session, per game. Edits go to `app.draft`; the save bar at the bottom writes them.

use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    Render, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px, rgb,
};
use zc_core::Hns;
use zc_core::profile::{GameProfile, ProfileSet, exe_name};
use zc_core::settings::MAX_APP_TRACKS;

use super::components::button::{button, button_primary};
use super::components::open_apps::{
    has_live_track, same_exe, set_audio_track, set_game_profile, track_slot_free,
};
use super::components::toggle::toggle;
use super::settings::Draft;
use super::theme;
use super::{dvr_megabytes, effective_ceiling, human_duration};
use crate::app::ZeroClipApp;

const SURFACE: u32 = theme::CARD;
const FIELD: u32 = theme::BG;
const MUTED: u32 = theme::MUTED_FG;
const WARN: u32 = theme::WARN;
const MIN_SECS: u32 = 10;
const MAX_SECS: u32 = 1_800;

pub struct ProfilesView {
    app: WeakEntity<ZeroClipApp>,
    /// One focus handle per game row, for its executable field.
    fields: Vec<FocusHandle>,
}

impl ProfilesView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app, fields: Vec::new() }
    }

    /// Runs `f` on the app (where the draft lives) and repaints it and this screen.
    fn edit(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut ZeroClipApp)) {
        self.app
            .update(cx, |app, cx| {
                f(app);
                cx.notify();
            })
            .ok();
        cx.notify();
    }
}

/// +10 s below a minute, +30 s above; the reverse going down. 60 s is the pivot both ways.
fn step_secs(secs: u32, up: bool) -> u32 {
    let next = match (up, secs < 60) {
        (true, true) => secs + 10,
        (true, false) => secs + 30,
        (false, true) => secs.saturating_sub(10),
        (false, false) if secs == 60 => 50,
        (false, false) => secs - 30,
    };
    next.clamp(MIN_SECS, MAX_SECS)
}

fn secs_label(secs: u32) -> String {
    human_duration(Hns::from_secs(i64::from(secs)))
}

fn label(text: &'static str) -> gpui::Div {
    div().text_xs().text_color(rgb(MUTED)).child(text)
}

fn stepper(
    i: usize,
    secs: u32,
    cx: &mut Context<ProfilesView>,
    set: fn(&mut ZeroClipApp, usize, u32),
) -> impl IntoElement {
    let step = |name: &'static str, glyph: &'static str, up: bool, cx: &mut Context<_>| {
        button((name, i), glyph).w(px(28.0)).flex().justify_center().on_click(cx.listener(
            move |this: &mut ProfilesView, _, _, cx| {
                this.edit(cx, |app| set(app, i, step_secs(current(app, i), up)));
            },
        ))
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(step("dec", "-", false, cx))
        .child(div().w(px(76.0)).flex().justify_center().child(secs_label(secs)))
        .child(step("inc", "+", true, cx))
}

/// Row 0 is the default profile; rows 1.. are `games[i - 1]`.
fn profile_mut(profiles: &mut ProfileSet, i: usize) -> &mut GameProfile {
    if i == 0 { &mut profiles.default } else { &mut profiles.games[i - 1] }
}

fn current(app: &ZeroClipApp, i: usize) -> u32 {
    let p = &app.draft.settings.profiles;
    if i == 0 { p.default.buffer_secs } else { p.games[i - 1].buffer_secs }
}

fn set_secs(app: &mut ZeroClipApp, i: usize, secs: u32) {
    profile_mut(&mut app.draft.settings.profiles, i).buffer_secs = secs;
}

fn memory_note(secs: u32, max_kbps: u32, ceiling_mb: u64) -> AnyElement {
    let mb = dvr_megabytes(secs, max_kbps);
    if mb > ceiling_mb {
        div()
            .text_color(rgb(WARN))
            .child(format!("~{mb} MB, over the {ceiling_mb} MB limit: will be shortened"))
            .into_any_element()
    } else {
        div().text_color(rgb(MUTED)).child(format!("~{mb} MB")).into_any_element()
    }
}

impl Render for ProfilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app_entity) = self.app.upgrade() else { return div().into_any_element() };
        // Copy what the screen needs, so the app isn't borrowed while the listeners are built.
        let (profiles, tracks, max_kbps, ceiling_mb, dirty, problems) = {
            let app = app_entity.read(cx);
            let d = &app.draft;
            (
                d.settings.profiles.clone(),
                d.settings.audio.app_tracks.clone(),
                d.settings.video.max_bitrate_kbps,
                u64::from(effective_ceiling(d.settings.dvr.ram_ceiling_mib).mib) * 1_048_576
                    / 1_000_000,
                d.is_dirty(&app.settings),
                d.problems.clone(),
            )
        };
        self.fields.resize_with(profiles.games.len(), || cx.focus_handle());

        let mut rows: Vec<AnyElement> = Vec::new();
        for i in 0..=profiles.games.len() {
            let profile = if i == 0 { &profiles.default } else { &profiles.games[i - 1] };
            let name: AnyElement = if i == 0 {
                div().w(px(190.0)).child("Any other game").into_any_element()
            } else {
                let handle = self.fields[i - 1].clone();
                let focused = handle.is_focused(window);
                let exe = profile.exe.clone();
                let empty = exe.is_empty();
                div()
                    .id(("exe", i))
                    .track_focus(&handle)
                    .w(px(190.0))
                    .px_2()
                    .py_1()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(FIELD))
                    .border_1()
                    .border_color(rgb(if focused { theme::RING } else { theme::INPUT }))
                    .cursor_text()
                    .truncate()
                    .on_click({
                        let handle = handle.clone();
                        move |_, window, _| window.focus(&handle)
                    })
                    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                        let k = &ev.keystroke;
                        // ponytail: typing and backspace only, no cursor movement or paste.
                        // Add when a shared text field lands (Settings needs one too).
                        match k.key.as_str() {
                            "enter" | "escape" => window.blur(),
                            "backspace" => this.edit(cx, |app| {
                                profile_mut(&mut app.draft.settings.profiles, i).exe.pop();
                            }),
                            _ if k.modifiers.control || k.modifiers.alt || k.modifiers.platform => {
                            }
                            _ => {
                                if let Some(c) = k.key_char.clone().filter(|c| {
                                    !c.chars().any(|ch| ch.is_control() || ch.is_whitespace())
                                }) {
                                    this.edit(cx, |app| {
                                        profile_mut(&mut app.draft.settings.profiles, i)
                                            .exe
                                            .push_str(&c);
                                    });
                                }
                            }
                        }
                    }))
                    .child(if empty && !focused {
                        div().text_color(rgb(MUTED)).child("game.exe").into_any_element()
                    } else {
                        div()
                            .child(if focused { format!("{exe}|") } else { exe })
                            .into_any_element()
                    })
                    .into_any_element()
            };
            let session = profile.session_recording;
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .px_3()
                    .py_2()
                    .rounded(px(theme::RADIUS))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(SURFACE))
                    .child(name)
                    .child(stepper(i, profile.buffer_secs, cx, set_secs))
                    .child(div().w(px(60.0)).child(toggle(("session", i), session, true).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.edit(cx, |app| {
                                let p = profile_mut(&mut app.draft.settings.profiles, i);
                                p.session_recording = !p.session_recording;
                            });
                        }),
                    )))
                    .child(div().flex_1().text_sm().child(memory_note(
                        profile.buffer_secs,
                        max_kbps,
                        ceiling_mb,
                    )))
                    .child(if i == 0 {
                        div().w(px(64.0)).into_any_element()
                    } else {
                        button(("remove", i), "Remove")
                            .w(px(64.0))
                            .text_sm()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.fields.remove(i - 1);
                                this.edit(cx, |app| {
                                    app.draft.settings.profiles.games.remove(i - 1);
                                });
                            }))
                            .into_any_element()
                    })
                    .into_any_element(),
            );
        }

        let header = div()
            .flex()
            .flex_row()
            .gap_4()
            .px_3()
            .child(div().w(px(190.0)).child(label("Executable")))
            .child(div().w(px(140.0)).child(label("DVR buffer")))
            .child(div().w(px(60.0)).child(label("Whole session")))
            .child(div().flex_1().child(label("Memory at max bitrate")));

        let apps = self.open_apps_section(&app_entity, &profiles, &tracks, cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("profiles_scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Game profiles"),
                    )
                    .child(div().text_sm().text_color(rgb(MUTED)).mb_2().child(
                        "How much footage to keep, and whether to record the whole session, \
                         per game. Games without a profile use the first row.",
                    ))
                    .child(header)
                    .children(rows)
                    .child(div().mt_1().child(button("add_game", "Add game").w(px(96.0)).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.edit(cx, |app| {
                                app.draft.settings.profiles.games.push(GameProfile {
                                    exe: String::new(),
                                    buffer_secs: 60,
                                    session_recording: false,
                                });
                            });
                            // Put the cursor in the new row's field.
                            let handle = cx.focus_handle();
                            window.focus(&handle);
                            this.fields.push(handle);
                        }),
                    )))
                    .child(apps),
            )
            .child(save_bar(dirty, problems, cx))
            .into_any_element()
    }
}

impl ProfilesView {
    /// Windows running now, each with switches to make it a game profile or a recorded audio track.
    fn open_apps_section(
        &self,
        app_entity: &gpui::Entity<ZeroClipApp>,
        profiles: &ProfileSet,
        tracks: &[zc_core::settings::AppTrack],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let listed: Vec<(String, String)> = app_entity.update(cx, |app, _| {
            app.open_apps.apps().iter().map(|a| (a.exe.clone(), a.title.clone())).collect()
        });
        let mut rows: Vec<AnyElement> = Vec::new();
        for (n, (exe, title)) in listed.iter().enumerate() {
            let is_game = profiles.find(exe).is_some();
            let live = has_live_track(tracks, exe);
            let free = track_slot_free(tracks, exe) || live;
            let (g_exe, a_exe) = (exe.clone(), exe.clone());
            let mut audio = toggle(("audio", n), live, free);
            if free {
                audio = audio.on_click(cx.listener(move |this, _, _, cx| {
                    this.edit(cx, |app| {
                        let on = !has_live_track(&app.draft.settings.audio.app_tracks, &a_exe);
                        set_audio_track(&mut app.draft.settings.audio.app_tracks, &a_exe, on);
                    });
                }));
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .px_3()
                    .py_1()
                    .child(div().w(px(160.0)).truncate().child(exe.clone()))
                    .child(div().flex_1().truncate().text_color(rgb(MUTED)).child(
                        if title.is_empty() { "(no title)".to_owned() } else { title.clone() },
                    ))
                    .child(div().w(px(60.0)).child(toggle(("game", n), is_game, true).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.edit(cx, |app| {
                                let on = app.draft.settings.profiles.find(&g_exe).is_none();
                                set_game_profile(&mut app.draft.settings.profiles, &g_exe, on);
                            });
                            this.fields.clear(); // rows changed; handles are rebuilt on render
                        }),
                    )))
                    .child(div().w(px(60.0)).child(audio))
                    .into_any_element(),
            );
        }
        let missing: Vec<String> = tracks
            .iter()
            .filter(|t| t.enabled && !listed.iter().any(|(e, _)| same_exe(e, &t.exe)))
            .map(|t| t.exe.clone())
            .collect();

        div()
            .mt_4()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Open apps"))
                    .child(button("refresh_apps", "Refresh").text_sm().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.edit(cx, |app| app.open_apps.refresh());
                        },
                    ))),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                "Game makes an app a profile. Audio track records it on its own track (up to {MAX_APP_TRACKS})."
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .px_3()
                    .child(div().w(px(160.0)).child(label("App")))
                    .child(div().flex_1().child(label("Window")))
                    .child(div().w(px(60.0)).child(label("Game")))
                    .child(div().w(px(60.0)).child(label("Audio track"))),
            )
            .children(if rows.is_empty() {
                vec![div().text_color(rgb(MUTED)).px_3().child("No open windows found.").into_any_element()]
            } else {
                rows
            })
            .children((!missing.is_empty()).then(|| {
                div()
                    .mt_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label("Audio tracks not running"))
                    .children(missing.into_iter().enumerate().map(|(n, exe)| {
                        let name = exe_name(&exe).to_owned();
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .child(div().w(px(160.0)).child(name))
                            .child(toggle(("stale", n), true, true).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.edit(cx, |app| {
                                        set_audio_track(
                                            &mut app.draft.settings.audio.app_tracks,
                                            &exe,
                                            false,
                                        );
                                    });
                                },
                            )))
                    }))
            }))
            .into_any_element()
    }
}

/// Save and Revert, enabled while the draft differs from what's saved, plus the last save's problems.
fn save_bar(
    dirty: bool,
    problems: Vec<String>,
    cx: &mut Context<ProfilesView>,
) -> impl IntoElement {
    let mut save = button_primary("save", "Save");
    let mut revert = button("revert", "Revert");
    if dirty {
        save = save.on_click(cx.listener(|this, _, _, cx| {
            this.edit(cx, ZeroClipApp::save_settings);
        }));
        revert = revert.on_click(cx.listener(|this, _, _, cx| {
            this.edit(cx, |app| app.draft = Draft::from_settings(&app.settings));
            this.fields.clear();
        }));
    } else {
        save = save.opacity(0.4);
        revert = revert.opacity(0.4);
    }
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .py_2()
        .bg(rgb(theme::BG))
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .children(
            problems.into_iter().map(|p| div().text_sm().text_color(rgb(theme::DANGER)).child(p)),
        )
        .child(div().flex().flex_row().items_center().gap_2().child(save).child(revert).children(
            dirty.then(|| div().text_sm().text_color(rgb(MUTED)).child("Unsaved changes")),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_pivots_at_a_minute_and_stays_in_range() {
        assert_eq!(step_secs(50, true), 60);
        assert_eq!(step_secs(60, true), 90);
        assert_eq!(step_secs(90, false), 60);
        assert_eq!(step_secs(60, false), 50);
        assert_eq!(step_secs(10, false), MIN_SECS);
        assert_eq!(step_secs(1_800, true), MAX_SECS);
    }
}
