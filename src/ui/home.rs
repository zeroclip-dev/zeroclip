use eframe::egui::{self, RichText};
use zc_core::control::Command;
use zc_core::hotkey::Chord;

use super::{human_duration, notice_color};
use crate::app::ZeroClipApp;

pub fn show(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    let window = app.settings.profiles.default.buffer();
    ui.heading("Clip what just happened");
    ui.label(format!(
        "ZeroClip keeps the last {} of gameplay in memory. Press a hotkey to save it.",
        human_duration(window)
    ));
    ui.add_space(8.0);

    let hotkeys = app.settings.hotkeys.clone();
    egui::Grid::new("home_hotkeys").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
        hotkey_row(ui, &hotkeys.save_clip, "Save the buffer as a clip");
        hotkey_row(ui, &hotkeys.toggle_recording, "Start or stop recording");
        hotkey_row(ui, &hotkeys.screenshot, "Screenshot");
        if let Some(ptt) = &hotkeys.push_to_talk {
            hotkey_row(ui, ptt, "Push-to-talk (hold)");
        }
    });

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button("Save clip").clicked() {
            app.send(Command::SaveClip);
        }
        let record = if app.status.recording { "Stop recording" } else { "Start recording" };
        if ui.button(record).clicked() {
            app.send(Command::ToggleRecording);
        }
        if ui.button("Screenshot").clicked() {
            app.send(Command::Screenshot);
        }
    });

    ui.add_space(16.0);
    ui.separator();
    ui.strong("Activity");
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        if app.notices.is_empty() {
            ui.weak("Nothing yet.");
        }
        for notice in &app.notices {
            ui.colored_label(notice_color(notice.level), &notice.text);
        }
    });
}

fn hotkey_row(ui: &mut egui::Ui, chord: &Chord, what: &str) {
    ui.label(RichText::new(chord.to_string()).monospace().strong());
    ui.label(what);
    ui.end_row();
}
