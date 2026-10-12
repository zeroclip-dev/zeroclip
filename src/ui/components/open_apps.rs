//! The "Open apps" list: windows running now, each with toggles to make it a game profile or a
//! recorded audio track. Used by the Profiles tab.

use std::path::Path;

use eframe::egui::{self, Label};
use zc_core::profile::{GameProfile, ProfileSet, exe_name};
use zc_core::settings::{AppTrack, MAX_APP_TRACKS};
use zc_win::process::{OpenApp, list_open_apps};

/// The last listing of open windows. Filled on the first show and on Refresh only.
#[derive(Default)]
pub struct OpenAppsList {
    apps: Vec<OpenApp>,
    loaded: bool,
}

impl OpenAppsList {
    fn refresh(&mut self) {
        self.apps = list_open_apps();
        self.loaded = true;
    }
}

/// The Refresh button, the open windows, and the configured audio tracks that aren't running.
pub fn show(
    ui: &mut egui::Ui,
    list: &mut OpenAppsList,
    profiles: &mut ProfileSet,
    tracks: &mut Vec<AppTrack>,
) {
    if !list.loaded {
        list.refresh();
    }
    if ui.button("Refresh").clicked() {
        list.refresh();
    }
    ui.add_space(4.0);

    if list.apps.is_empty() {
        ui.weak("No open windows found.");
    } else {
        egui::Grid::new("open_apps").num_columns(4).striped(true).spacing([20.0, 6.0]).show(
            ui,
            |ui| {
                ui.strong("App");
                ui.strong("Window");
                ui.strong("Game");
                ui.strong("Audio track");
                ui.end_row();

                for app in &list.apps {
                    ui.label(&app.exe);
                    let title =
                        if app.title.is_empty() { "(no title)" } else { app.title.as_str() };
                    ui.add_sized([260.0, 18.0], Label::new(title).truncate()).on_hover_text(title);

                    let mut game = profiles.find(&app.exe).is_some();
                    if ui.checkbox(&mut game, "").changed() {
                        set_game_profile(profiles, &app.exe, game);
                    }

                    let checked = has_live_track(tracks, &app.exe);
                    let mut track = checked;
                    let free = track_slot_free(tracks, &app.exe);
                    let response =
                        ui.add_enabled(free || checked, egui::Checkbox::new(&mut track, ""));
                    let response = if free || checked {
                        response
                    } else {
                        response.on_disabled_hover_text("Up to 4 app tracks")
                    };
                    if response.changed() {
                        set_audio_track(tracks, &app.exe, track);
                    }
                    ui.end_row();
                }
            },
        );
    }

    let not_running: Vec<String> = tracks
        .iter()
        .filter(|t| t.enabled && !list.apps.iter().any(|a| same_exe(&a.exe, &t.exe)))
        .map(|t| t.exe.clone())
        .collect();
    if !not_running.is_empty() {
        ui.add_space(8.0);
        ui.weak("Audio tracks not running");
        for exe in not_running {
            let mut track = true;
            ui.horizontal(|ui| {
                if ui.checkbox(&mut track, exe_name(&exe)).changed() {
                    set_audio_track(tracks, &exe, false);
                }
                ui.weak("not running");
            });
        }
    }

    ui.add_space(6.0);
    ui.weak("Audio tracks apply after restarting ZeroClip.");
}

fn same_exe(a: &str, b: &str) -> bool {
    exe_name(a).eq_ignore_ascii_case(exe_name(b))
}

fn has_live_track(tracks: &[AppTrack], exe: &str) -> bool {
    tracks.iter().any(|t| t.enabled && same_exe(&t.exe, exe))
}

/// Whether turning on a track for `exe` can succeed: it already has an entry to re-enable, or
/// there is a free slot for a new one.
fn track_slot_free(tracks: &[AppTrack], exe: &str) -> bool {
    tracks.len() < MAX_APP_TRACKS || tracks.iter().any(|t| same_exe(&t.exe, exe))
}

/// Makes `exe` a game profile, or removes its profile. Adding keeps an existing profile as it is,
/// and the new one starts from the default buffer length.
pub fn set_game_profile(profiles: &mut ProfileSet, exe: &str, on: bool) {
    let name = exe_name(exe);
    if !on {
        profiles.games.retain(|g| !g.exe.eq_ignore_ascii_case(name));
    } else if profiles.find(name).is_none() {
        let buffer_secs = profiles.default.buffer_secs;
        profiles.upsert(GameProfile {
            exe: name.to_owned(),
            buffer_secs,
            session_recording: false,
        });
    }
}

/// Records `exe` as an audio track, or stops recording it. Turning a track off keeps its entry,
/// so labels stay stable. Returns false when a new track has no free slot.
pub fn set_audio_track(tracks: &mut Vec<AppTrack>, exe: &str, on: bool) -> bool {
    let name = exe_name(exe);
    if let Some(track) = tracks.iter_mut().find(|t| same_exe(&t.exe, name)) {
        track.enabled = on;
        return true;
    }
    if !on {
        return true;
    }
    if tracks.len() >= MAX_APP_TRACKS {
        return false;
    }
    let label = unique_label(tracks, track_label(name));
    tracks.push(AppTrack { label, exe: name.to_owned(), enabled: true });
    true
}

/// "Discord.exe" becomes "Discord".
fn track_label(name: &str) -> &str {
    Path::new(name).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or(name)
}

/// `base`, or `base 2`, `base 3`, ... if another track already uses that label.
fn unique_label(tracks: &[AppTrack], base: &str) -> String {
    let taken = |label: &str| tracks.iter().any(|t| t.label.trim().eq_ignore_ascii_case(label));
    let mut label = base.to_owned();
    let mut n = 2;
    while taken(&label) {
        label = format!("{base} {n}");
        n += 1;
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use zc_core::settings::Settings;

    fn track(label: &str, exe: &str, enabled: bool) -> AppTrack {
        AppTrack { label: label.to_owned(), exe: exe.to_owned(), enabled }
    }

    #[test]
    fn checking_a_game_adds_a_profile_with_the_default_buffer() {
        let mut profiles = ProfileSet::default();
        set_game_profile(&mut profiles, "Discord.exe", true);
        let added = profiles.find("Discord.exe").expect("added");
        assert_eq!(added.buffer_secs, profiles.default.buffer_secs);
        assert!(!added.session_recording);
    }

    #[test]
    fn checking_a_game_keeps_its_existing_profile() {
        let mut profiles = ProfileSet::default();
        set_game_profile(&mut profiles, "cs2.exe", true);
        assert_eq!(profiles.find("cs2.exe").map(|g| g.buffer_secs), Some(120));
        assert_eq!(profiles.games.len(), 2);
    }

    #[test]
    fn unchecking_a_game_removes_its_profile() {
        let mut profiles = ProfileSet::default();
        set_game_profile(&mut profiles, "CS2.EXE", false);
        assert!(profiles.find("cs2.exe").is_none());
        assert_eq!(profiles.games.len(), 1);
    }

    #[test]
    fn checking_an_app_adds_a_track_labelled_by_its_name() {
        let mut tracks = Vec::new();
        assert!(set_audio_track(&mut tracks, r"C:\Apps\Obs64.exe", true));
        assert_eq!(tracks, vec![track("Obs64", "Obs64.exe", true)]);
    }

    #[test]
    fn checking_a_track_again_reuses_its_entry() {
        let mut tracks = Settings::default().audio.app_tracks;
        assert!(set_audio_track(&mut tracks, "Spotify.exe", false));
        assert!(!tracks[1].enabled);
        assert_eq!(tracks.len(), 2);
        assert!(set_audio_track(&mut tracks, "Spotify.exe", true));
        assert!(tracks[1].enabled);
        assert_eq!(tracks.len(), 2);
    }

    #[test]
    fn a_new_track_never_takes_a_fifth_slot() {
        let mut tracks: Vec<AppTrack> = (0..MAX_APP_TRACKS)
            .map(|i| track(&format!("App {i}"), &format!("a{i}.exe"), true))
            .collect();
        assert!(!track_slot_free(&tracks, "New.exe"));
        assert!(!set_audio_track(&mut tracks, "New.exe", true));
        assert_eq!(tracks.len(), MAX_APP_TRACKS);

        // A disabled entry for the same app can still be turned back on at the cap.
        tracks[0].enabled = false;
        assert!(track_slot_free(&tracks, "a0.exe"));
        assert!(set_audio_track(&mut tracks, "a0.exe", true));
        assert_eq!(tracks.len(), MAX_APP_TRACKS);
    }

    #[test]
    fn a_label_clash_gets_a_number() {
        let mut tracks = vec![track("Discord", "Other.exe", true)];
        assert!(set_audio_track(&mut tracks, "Discord.exe", true));
        assert_eq!(tracks[1].label, "Discord 2");
    }
}
