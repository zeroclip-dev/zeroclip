//! The UI shell: top bar, optional focus banner, the active screen, status bar. Screens are
//! views registered in `components/screens.rs`; this file never needs to change for a new screen.

pub mod art;
pub mod clip_menu;
pub mod clips;
pub mod components;
pub mod editor;
pub mod focus_prompt;
pub mod home;
pub mod icons;
pub mod image_viewer;
pub mod overlay;
pub mod profiles;
pub mod rescue;
pub mod settings;
pub mod theme;
pub mod update_banner;
pub mod video_player;

use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, WindowControlArea, div, px, rgb,
};
use zc_core::Hns;
use zc_core::media::Codec;

use crate::app::{Tab, ZeroClipApp};
use components::{mic_indicator::mic_indicator, status_dot::status_dot, tab_button::tab_button};
use icons::icon;

gpui::actions!(zeroclip, [ToggleFullscreen]);

pub fn shell(
    app: &ZeroClipApp,
    window: &gpui::Window,
    cx: &mut Context<ZeroClipApp>,
) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::BG))
        .text_color(rgb(theme::FG))
        .text_size(px(theme::TEXT_BODY))
        // F11 (bound in main.rs) toggles fullscreen.
        .on_action(|_: &ToggleFullscreen, window, _| window.toggle_fullscreen())
        .child(top_bar(app, cx))
        .children(focus_prompt::banner(app, cx))
        .children(rescue::banner(app, cx))
        .children(update_banner::banner(app, cx))
        .child(div().flex_1().min_h_0().overflow_hidden().child(app.screens.view(app.tab)))
        .child(status_bar(app))
        .children(clip_menu::layer(app, window, cx))
}

fn top_bar(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> impl IntoElement {
    let status = &app.status;
    let (dot_color, text_color, pill_bg, pill_border, text) = if status.recording {
        (
            theme::DANGER,
            theme::DANGER,
            theme::DANGER_SOFT,
            theme::DANGER_BORDER,
            "Recording".to_owned(),
        )
    } else if status.capturing && status.buffered == Hns::ZERO {
        // Capturing, but the DVR has no footage yet, so there's no span to show.
        (theme::BRAND, theme::BRAND, theme::BRAND_SOFT, theme::BRAND_BORDER, "Capturing".to_owned())
    } else if status.capturing {
        (
            theme::BRAND,
            theme::BRAND,
            theme::BRAND_SOFT,
            theme::BRAND_BORDER,
            format!("Buffering {}", human_duration(status.buffered)),
        )
    } else {
        (theme::TEXT_DIM, theme::MUTED_FG, theme::SURFACE_RAISED, theme::BORDER, "Idle".to_owned())
    };
    let tabs =
        div().flex().flex_row().items_center().gap_1().children(Tab::ALL.into_iter().map(|tab| {
            tab_button(tab.icon(), tab.label(), app.tab == tab).on_click(cx.listener(
                move |app, _, window, cx| {
                    // Fullscreen belongs to the Player and Editor; don't carry it to other tabs.
                    if tab != app.tab
                        && matches!(app.tab, Tab::Player | Tab::Editor)
                        && window.is_fullscreen()
                    {
                        window.toggle_fullscreen();
                    }
                    // Leaving the Editor stops its playback; it isn't drawn, so nothing else would.
                    if app.tab == Tab::Editor && tab != Tab::Editor {
                        app.screens.editor().clone().update(cx, |editor, cx| editor.pause(cx));
                    }
                    if app.tab == Tab::Player {
                        app.screens.player().clone().update(cx, |player, cx| player.pause(cx));
                    }
                    app.tab = tab;
                    cx.notify();
                },
            ))
        }));
    let status_pill = div()
        .h(px(26.))
        .px(px(10.))
        .mr_3()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .rounded_full()
        .bg(rgb(pill_bg))
        .border_1()
        .border_color(rgb(pill_border))
        .text_size(px(theme::TEXT_CAPTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(text_color))
        .child(status_dot(rgb(dot_color)))
        .child(text);
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(48.))
        .pl_4()
        .bg(rgb(theme::BG))
        .border_b_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .mr_5()
                .child(icons::logo(24.))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::FG))
                        .child("ZeroClip"),
                ),
        )
        .child(tabs)
        // The empty stretch is what you grab to move the window.
        .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
        .child(status_pill)
        .child(window_button("win-min", "minus", WindowControlArea::Min, false))
        .child(window_button("win-max", "square", WindowControlArea::Max, false))
        .child(window_button("win-close", "x", WindowControlArea::Close, true))
}

/// A minimise, maximise or close button. Windows acts on the control area itself, so there is
/// no click handler.
fn window_button(
    id: &'static str,
    name: &str,
    area: WindowControlArea,
    close: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(46.))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(theme::MUTED_FG))
        .hover(move |s| {
            if close {
                s.bg(rgb(theme::DANGER)).text_color(rgb(theme::PRIMARY_FG))
            } else {
                s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG))
            }
        })
        .active(move |s| {
            if close {
                s.bg(rgb(theme::DANGER)).opacity(0.85)
            } else {
                s.bg(rgb(theme::SURFACE_ACTIVE))
            }
        })
        .window_control_area(area)
        .child(icon(name, theme::MUTED_FG))
}

fn status_item(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(div().text_color(rgb(theme::TEXT_DIM)).child(label))
        .child(div().text_color(rgb(theme::MUTED_FG)).child(value.into()))
}

fn status_sep() -> gpui::Div {
    div().text_color(rgb(theme::BORDER_STRONG)).child("\u{00b7}")
}

fn status_bar(app: &ZeroClipApp) -> impl IntoElement {
    let status = &app.status;
    let mic = mic_indicator(status.mic);
    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .h(px(28.))
        .px_4()
        .bg(rgb(theme::BG))
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .text_size(px(theme::TEXT_MICRO))
        .text_color(rgb(theme::MUTED_FG))
        .child(status_item("Codec", status.codec.map_or("-", Codec::name)))
        .child(status_sep())
        .child(status_item(
            "Buffer",
            format!(
                "{} / {}",
                human_duration(status.buffered),
                human_duration(status.buffer_window)
            ),
        ))
        .child(status_sep())
        .child(status_item("Watchdog", format!("{:?}", status.watchdog)))
        .child(status_sep())
        .child(status_item("Game", status.game.as_deref().unwrap_or("none")));
    if let Some(mic_el) = mic {
        bar = bar.child(status_sep()).child(mic_el);
    }
    bar.child(
        div()
            .ml_auto()
            .text_color(rgb(theme::TEXT_DIM))
            .child(concat!("v", env!("CARGO_PKG_VERSION"))),
    )
}

/// "45 s", "2 min", "1 min 30 s".
pub fn human_duration(t: Hns) -> String {
    let secs = t.as_millis().max(0) / 1_000;
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// Approximate DVR memory for `secs` at `max_kbps`, in MB (DESIGN §4 table).
pub fn dvr_megabytes(secs: u32, max_kbps: u32) -> u64 {
    u64::from(max_kbps) * 1_000 / 8 * u64::from(secs) / 1_000_000
}

/// Size of the disk-mode ring file for `secs` (P4.21), in MB. The engine sizes it the same way.
pub fn dvr_ring_megabytes(secs: u32, max_kbps: u32, audio_kbps: u32) -> u64 {
    let window = zc_core::Hns::from_secs(i64::from(secs));
    let config = zc_core::dvr::DvrConfig::for_window(
        window,
        u64::from(max_kbps) * 1_000,
        u64::from(audio_kbps) * 1_000,
    );
    config.ring_file_bytes() / 1_000_000
}

/// The DVR memory limit the engine really runs under: the setting, capped at a share of this
/// machine's RAM (D32, P4.20). The engine and the UI must agree on it, or the Profiles tab shows
/// windows the engine then cuts.
pub fn effective_ceiling(setting_mib: u32) -> zc_core::profile::RamCeiling {
    zc_core::profile::effective_ram_ceiling(setting_mib, zc_win::memory::total_physical_mib())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(Hns::from_secs(45)), "45 s");
        assert_eq!(human_duration(Hns::from_secs(120)), "2 min");
        assert_eq!(human_duration(Hns::from_secs(90)), "1 min 30 s");
        assert_eq!(human_duration(Hns(-5)), "0 s");
    }

    #[test]
    fn dvr_memory_matches_the_design_doc() {
        // DESIGN §4: 20 Mbps → 150 MB per minute.
        assert_eq!(dvr_megabytes(60, 20_000), 150);
        assert_eq!(dvr_megabytes(300, 20_000), 750);
    }
}
