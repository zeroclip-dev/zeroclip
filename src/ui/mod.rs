//! The UI shell: top bar, optional focus banner, the active screen, status bar. Screens are
//! views registered in `components/screens.rs`; this file never needs to change for a new screen.

pub mod clips;
pub mod components;
pub mod focus_prompt;
pub mod home;
pub mod overlay;
pub mod profiles;
pub mod rescue;
pub mod settings;
pub mod theme;

use gpui::{Context, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div, rgb};
use zc_core::Hns;
use zc_core::media::Codec;

use crate::app::{Tab, ZeroClipApp};
use components::{mic_indicator::mic_indicator, status_dot::status_dot, tab_button::tab_button};

const RED: u32 = theme::DANGER;
const GREEN: u32 = theme::BRAND;
const GRAY: u32 = theme::MUTED_FG;

pub fn shell(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::BG))
        .text_color(rgb(theme::FG))
        .child(top_bar(app, cx))
        .children(focus_prompt::banner(app, cx))
        .children(rescue::banner(app, cx))
        .child(div().flex_1().min_h_0().overflow_hidden().child(app.screens.view(app.tab)))
        .child(status_bar(app))
}

fn top_bar(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> impl IntoElement {
    let status = &app.status;
    let (color, text) = if status.recording {
        (RED, "Recording".to_owned())
    } else if status.capturing && status.buffered == Hns::ZERO {
        // Capturing, but the DVR has no footage yet, so there's no span to show.
        (GREEN, "Capturing".to_owned())
    } else if status.capturing {
        (GREEN, format!("Buffering {}", human_duration(status.buffered)))
    } else {
        (GRAY, "Idle".to_owned())
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .child(div().text_xl().font_weight(gpui::FontWeight::BOLD).mr_4().child("ZeroClip"))
        .children(Tab::ALL.map(|tab| {
            tab_button(tab.label(), app.tab == tab).on_click(cx.listener(move |app, _, _, cx| {
                app.tab = tab;
                cx.notify();
            }))
        }))
        .child(
            div()
                .ml_auto()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_color(rgb(color))
                .child(status_dot(rgb(color)))
                .child(text),
        )
}

fn status_bar(app: &ZeroClipApp) -> impl IntoElement {
    let status = &app.status;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .px_3()
        .py_1()
        .text_sm()
        .text_color(rgb(GRAY))
        .child(format!("Codec: {}", status.codec.map_or("—", Codec::name)))
        .child(format!(
            "Buffer: {} of {}",
            human_duration(status.buffered),
            human_duration(status.buffer_window)
        ))
        .child(format!("Watchdog: {:?}", status.watchdog))
        .child(format!("Game: {}", status.game.as_deref().unwrap_or("none")))
        .children(mic_indicator(status.mic))
        .child(div().ml_auto().child(concat!("v", env!("CARGO_PKG_VERSION"))))
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
#[allow(dead_code)] // used by the Settings and Profiles screens
pub fn dvr_megabytes(secs: u32, max_kbps: u32) -> u64 {
    u64::from(max_kbps) * 1_000 / 8 * u64::from(secs) / 1_000_000
}

/// Size of the disk-mode ring file for `secs` (P4.21), in MB. The engine sizes it the same way.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
