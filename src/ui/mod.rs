//! The UI, one module per tab. Immediate mode: every function here runs on each
//! repaint, and repaints only happen on input or when the engine wakes us.

pub mod clips;
pub mod home;
pub mod profiles;
pub mod settings;

use eframe::egui::{self, Align, Color32, Layout, RichText};
use zc_core::Hns;
use zc_core::media::Codec;

use crate::app::{NoticeLevel, Tab, ZeroClipApp};

pub fn show(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    egui::Panel::top("top_bar").show(ui, |ui| top_bar(app, ui));
    egui::Panel::bottom("status_bar").show(ui, |ui| status_bar(app, ui));
    egui::CentralPanel::default().show(ui, |ui| match app.tab {
        Tab::Home => home::show(app, ui),
        Tab::Clips => clips::show(app, ui),
        Tab::Profiles => profiles::show(app, ui),
        Tab::Settings => settings::show(app, ui),
    });
}

fn top_bar(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("ZeroClip").size(20.0).strong());
        ui.add_space(16.0);
        for (tab, label) in [
            (Tab::Home, "Home"),
            (Tab::Clips, "Clips"),
            (Tab::Profiles, "Profiles"),
            (Tab::Settings, "Settings"),
        ] {
            ui.selectable_value(&mut app.tab, tab, label);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let status = &app.status;
            let (color, text) = if status.recording {
                (Color32::LIGHT_RED, "Recording".to_owned())
            } else if status.capturing && status.buffered == Hns::ZERO {
                // Nothing is buffered until the DVR arrives in Phase 3.
                (Color32::LIGHT_GREEN, "Capturing".to_owned())
            } else if status.capturing {
                (Color32::LIGHT_GREEN, format!("Buffering {}", human_duration(status.buffered)))
            } else {
                (Color32::GRAY, "Idle".to_owned())
            };
            // Right-to-left: the label is placed first, the dot to its left.
            ui.colored_label(color, text);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
        });
    });
    ui.add_space(4.0);
}

fn status_bar(app: &ZeroClipApp, ui: &mut egui::Ui) {
    let status = &app.status;
    ui.horizontal(|ui| {
        ui.weak(format!("Codec: {}", status.codec.map_or("—", Codec::name)));
        ui.separator();
        ui.weak(format!(
            "Buffer: {} of {}",
            human_duration(status.buffered),
            human_duration(status.buffer_window)
        ));
        ui.separator();
        ui.weak(format!("Watchdog: {:?}", status.watchdog));
        ui.separator();
        ui.weak(format!("Game: {}", status.game.as_deref().unwrap_or("none")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.weak(concat!("v", env!("CARGO_PKG_VERSION")));
        });
    });
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

pub fn notice_color(level: NoticeLevel) -> Color32 {
    match level {
        NoticeLevel::Info => Color32::GRAY,
        NoticeLevel::Warning => Color32::from_rgb(230, 190, 90),
        NoticeLevel::Error => Color32::LIGHT_RED,
    }
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
