//! The "Open apps" list: windows running now, each with toggles to make it a game profile or a
//! recorded audio track. Used by the Profiles tab.

use std::path::Path;

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
    pub fn refresh(&mut self) {
        self.apps = list_open_apps();
        self.loaded = true;
    }

    /// The windows listed last, listing them first if that never happened.
    pub fn apps(&mut self) -> &[OpenApp] {
        if !self.loaded {
            self.refresh();
        }
        &self.apps
    }
}

pub fn same_exe(a: &str, b: &str) -> bool {
    exe_name(a).eq_ignore_ascii_case(exe_name(b))
}

pub fn has_live_track(tracks: &[AppTrack], exe: &str) -> bool {
    tracks.iter().any(|t| t.enabled && same_exe(&t.exe, exe))
}

/// Whether turning on a track for `exe` can succeed: it already has an entry to re-enable, or
/// there is a free slot for a new one.
pub fn track_slot_free(tracks: &[AppTrack], exe: &str) -> bool {
    live_tracks(tracks) < MAX_APP_TRACKS || tracks.iter().any(|t| same_exe(&t.exe, exe))
}

/// Tracks that are switched on. Switched-off entries are kept for stable labels but don't use a slot.
pub fn live_tracks(tracks: &[AppTrack]) -> usize {
    tracks.iter().filter(|t| t.enabled).count()
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
    if live_tracks(tracks) >= MAX_APP_TRACKS {
        return false;
    }
    // Settings cap the list length, so drop a switched-off entry to make room.
    if tracks.len() >= MAX_APP_TRACKS
        && let Some(i) = tracks.iter().position(|t| !t.enabled)
    {
        tracks.remove(i);
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
    fn switched_off_tracks_do_not_block_a_new_one() {
        let mut tracks: Vec<AppTrack> = (0..MAX_APP_TRACKS)
            .map(|i| track(&format!("App {i}"), &format!("a{i}.exe"), false))
            .collect();
        assert!(track_slot_free(&tracks, "New.exe"));
        assert!(set_audio_track(&mut tracks, "New.exe", true));
        assert_eq!(tracks.len(), MAX_APP_TRACKS);
        assert!(has_live_track(&tracks, "New.exe"));
    }

    #[test]
    fn a_label_clash_gets_a_number() {
        let mut tracks = vec![track("Discord", "Other.exe", true)];
        assert!(set_audio_track(&mut tracks, "Discord.exe", true));
        assert_eq!(tracks[1].label, "Discord 2");
    }
}
