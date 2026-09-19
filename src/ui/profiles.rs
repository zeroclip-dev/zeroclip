//! Game profiles (DESIGN §7). Edited here, applied automatically on focus change
//! once the profile engine lands (Phase 4).

use eframe::egui::{self, DragValue, TextEdit};
use zc_core::profile::GameProfile;

use super::dvr_megabytes;
use crate::app::ZeroClipApp;

pub fn show(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    egui::Panel::bottom("profiles_actions").show(ui, |ui| super::settings::save_bar(app, ui));

    ui.heading("Game profiles");
    ui.label(
        "How much footage to keep, and whether to record the whole session, per game. \
         Games without a profile use the first row.",
    );
    ui.add_space(8.0);

    let max_kbps = app.draft.settings.video.max_bitrate_kbps;
    let ceiling_mb = u64::from(app.draft.settings.dvr.ram_ceiling_mib) * 1_048_576 / 1_000_000;
    let profiles = &mut app.draft.settings.profiles;
    let mut remove = None;

    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        egui::Grid::new("profiles").num_columns(5).striped(true).spacing([20.0, 6.0]).show(
            ui,
            |ui| {
                ui.strong("Executable");
                ui.strong("DVR buffer");
                ui.strong("Session recording");
                ui.strong("Memory at max bitrate");
                ui.label("");
                ui.end_row();

                ui.label("Any other game");
                profile_cells(ui, &mut profiles.default, max_kbps, ceiling_mb);
                ui.label("");
                ui.end_row();

                for (i, profile) in profiles.games.iter_mut().enumerate() {
                    ui.add_sized([180.0, 20.0], TextEdit::singleline(&mut profile.exe));
                    profile_cells(ui, profile, max_kbps, ceiling_mb);
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            },
        );
        if let Some(i) = remove {
            profiles.games.remove(i);
        }
        ui.add_space(8.0);
        if ui.button("Add game").clicked() {
            profiles.games.push(GameProfile {
                exe: "game.exe".to_owned(),
                buffer_secs: 60,
                session_recording: false,
            });
        }
    });
}

fn profile_cells(ui: &mut egui::Ui, profile: &mut GameProfile, max_kbps: u32, ceiling_mb: u64) {
    ui.add(DragValue::new(&mut profile.buffer_secs).range(10..=1_800).suffix(" s"));
    ui.checkbox(&mut profile.session_recording, "");
    let mb = dvr_megabytes(profile.buffer_secs, max_kbps);
    if mb > ceiling_mb {
        ui.colored_label(
            egui::Color32::from_rgb(230, 190, 90),
            format!("~{mb} MB (over the {ceiling_mb} MB limit: will be shortened)"),
        );
    } else {
        ui.weak(format!("~{mb} MB"));
    }
}
