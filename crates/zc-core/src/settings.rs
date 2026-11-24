//! User settings.
//!
//! Every field has a serde default, so settings files written by older versions
//! keep loading as options are added. The app stores this as TOML.

use std::ops::RangeInclusive;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hotkey::Chord;
use crate::media::{Codec, ContainerFormat, MAX_AUDIO_TRACKS};
use crate::profile::ProfileSet;
use crate::time::Hns;

/// Everything the user can configure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where files go and in what container.
    pub recording: RecordingSettings,
    /// Retrospective DVR buffer.
    pub dvr: DvrSettings,
    /// Capture and encoding.
    pub video: VideoSettings,
    /// Game audio and microphone.
    pub audio: AudioSettings,
    /// Global hotkeys.
    pub hotkeys: HotkeySettings,
    /// Disk behaviour.
    pub storage: StorageSettings,
    /// Upload behaviour.
    pub uploads: UploadSettings,
    /// Per-game profiles.
    pub profiles: ProfileSet,
    /// The lightning editor's external tools (P5.5).
    #[serde(default)]
    pub editor: EditorSettings,
    /// Tray and startup behaviour (P4.15, P4.16).
    #[serde(default)]
    pub app: AppSettings,
    /// Simple mode ("clip the last N minutes") or the full settings.
    #[serde(default)]
    pub ui_mode: UiMode,
    /// Small click-through cards for "clip saved", recording and screenshot events. They are
    /// excluded from capture, so they never end up in a recording.
    #[serde(default = "default_overlay_toasts")]
    pub overlay_toasts: bool,
}

fn default_overlay_toasts() -> bool {
    true
}

// Hand-written so `overlay_toasts` defaults to on, not to `bool::default()`.
impl Default for Settings {
    fn default() -> Self {
        Self {
            recording: RecordingSettings::default(),
            dvr: DvrSettings::default(),
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            hotkeys: HotkeySettings::default(),
            storage: StorageSettings::default(),
            uploads: UploadSettings::default(),
            profiles: ProfileSet::default(),
            editor: EditorSettings::default(),
            app: AppSettings::default(),
            ui_mode: UiMode::default(),
            overlay_toasts: default_overlay_toasts(),
        }
    }
}

/// How the app presents itself. Old settings files have no `ui_mode`, so they load as
/// [`Advanced`](UiMode::Advanced), the full settings they already had.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiMode {
    /// Just "clip the last N minutes"; the app only checks that storage is enough.
    Simple,
    /// The full settings.
    #[default]
    Advanced,
}

impl Settings {
    /// Problems that should block saving, as sentences for the UI. Empty when the
    /// settings are usable.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let video = &self.video;
        if !(1..=240).contains(&video.fps) {
            problems
                .push(format!("Frame rate must be between 1 and 240 fps (it is {}).", video.fps));
        }
        if video.min_bitrate_kbps > video.max_bitrate_kbps {
            problems.push("The minimum bitrate is above the maximum.".to_owned());
        }
        if !(500..=4_000).contains(&video.keyframe_interval_ms) {
            problems.push("The keyframe interval must be between 0.5 and 4 seconds.".to_owned());
        }
        if self.dvr.ram_ceiling_mib < 64 {
            problems.push("The DVR memory ceiling must be at least 64 MiB.".to_owned());
        }
        if !(1..=64).contains(&self.storage.write_chunk_mib) {
            problems.push("The write chunk size must be between 1 and 64 MiB.".to_owned());
        }
        if !AAC_BITRATES_KBPS.contains(&self.audio.bitrate_kbps) {
            problems.push("The audio bitrate must be 96, 128, 160 or 192 kbps.".to_owned());
        }
        for (source, db) in [
            ("game audio", self.audio.mix_gain_db.game),
            ("microphone", self.audio.mix_gain_db.mic),
        ] {
            if !MIX_GAIN_DB.contains(&db) {
                problems.push(format!(
                    "The {source} level in the Master track must be between -60 and +12 dB (it is {db})."
                ));
            }
        }
        if self.audio.mic == MicMode::PushToTalk && self.hotkeys.push_to_talk.is_none() {
            problems.push("Push-to-talk mode needs a push-to-talk hotkey.".to_owned());
        }
        if self.recording.container == ContainerFormat::Matroska {
            problems.push(
                "Matroska recordings aren't written. Choose Fragmented MP4 (.mp4).".to_owned(),
            );
        }
        problems.extend(app_track_problems(&self.audio.app_tracks));

        let hotkeys = &self.hotkeys;
        let bound = [
            ("Save clip", Some(&hotkeys.save_clip)),
            ("Start/stop recording", Some(&hotkeys.toggle_recording)),
            ("Screenshot", Some(&hotkeys.screenshot)),
            ("Bookmark", Some(&hotkeys.bookmark)),
            ("Push-to-talk", hotkeys.push_to_talk.as_ref()),
        ];
        for (i, (name_a, chord_a)) in bound.iter().enumerate() {
            for (name_b, chord_b) in &bound[i + 1..] {
                if let (Some(a), Some(b)) = (chord_a, chord_b)
                    && a == b
                {
                    problems.push(format!("{name_a} and {name_b} both use {a}."));
                }
            }
        }
        problems
    }
}

/// Tray and startup behaviour (P4.15, P4.16).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Closing the window hides it to the tray instead of quitting. The tray's Quit still quits.
    pub close_to_tray: bool,
    /// Start ZeroClip with Windows, hidden in the tray (the HKCU Run key).
    pub start_with_windows: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { close_to_tray: true, start_with_windows: false }
    }
}

/// The editor's external tools (P5.5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    /// Full path to `ffmpeg.exe`. `None` looks next to ZeroClip, then on `PATH`. A path that
    /// doesn't exist is an error, not a silent fallback. `ffprobe` is expected beside it.
    pub ffmpeg_path: Option<PathBuf>,
    /// What "Done" does. `Ask` shows the choice each time; the other two remember it.
    pub finish: FinishChoice,
    /// The audio track the preview plays (0 is Master). Remembered between sessions.
    pub listen_track: u8,
}

/// What the editor does when you press Done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FinishChoice {
    /// Ask every time.
    #[default]
    Ask,
    /// Join the pieces into one new file.
    SaveAs,
    /// Save each piece as its own file and keep editing.
    Extract,
}

/// Output location and container.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// Output folder. `None` means `<Videos>\ZeroClip`.
    pub output_dir: Option<PathBuf>,
    /// Container for recordings. Only fragmented MP4 is written (D18).
    pub container: ContainerFormat,
}

/// DVR buffer settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DvrSettings {
    /// Keep a rolling buffer at all.
    pub enabled: bool,
    /// Upper bound on the buffer's RAM, in MiB. Profiles asking for more are clamped.
    pub ram_ceiling_mib: u32,
    /// Where the buffer lives (D32). Applies when the engine starts.
    pub storage: DvrStorage,
    /// Folder of the ring file in [`DvrStorage::Disk`] mode. `None` is the recordings folder.
    pub ring_dir: Option<PathBuf>,
}

impl Default for DvrSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            ram_ceiling_mib: 1_024,
            storage: DvrStorage::default(),
            ring_dir: None,
        }
    }
}

/// Where the DVR buffer is kept (D32). Hybrid (P4.22) is not built yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DvrStorage {
    /// Pre-allocated, page-locked RAM (D4).
    #[default]
    Ram,
    /// A fixed-size ring file on disk; only a small staging area stays in RAM (P4.21).
    Disk,
}

/// Capture and encoder settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    /// Force a codec. `None` picks the best one the GPU supports (DESIGN §3 ladder).
    pub codec: Option<Codec>,
    /// Target frame rate.
    pub fps: u32,
    /// VBR floor, kbit/s (DESIGN §3: ~5 Mbps).
    pub min_bitrate_kbps: u32,
    /// VBR ceiling, kbit/s (DESIGN §3: ~20 Mbps). Drives the DVR buffer's RAM use.
    pub max_bitrate_kbps: u32,
    /// Fixed keyframe interval, ms (DESIGN §3: 1–2 s). Also the editor's trim granularity.
    pub keyframe_interval_ms: u32,
    /// Draw the mouse cursor into the capture.
    pub capture_cursor: bool,
    /// The screen to record, as an index in the order Windows lists displays. `None` is
    /// the primary display. An index that isn't connected falls back to the primary.
    pub monitor: Option<u32>,
    /// What to record: only profiled games (default), or the whole screen (D31).
    #[serde(default)]
    pub capture_mode: CaptureMode,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: None,
            fps: 60,
            min_bitrate_kbps: 5_000,
            max_bitrate_kbps: 20_000,
            keyframe_interval_ms: 1_000,
            capture_cursor: false,
            monitor: None,
            capture_mode: CaptureMode::default(),
        }
    }
}

/// What the video source records (D31).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Only the window of a game that has a profile, while it has focus. Nothing else is
    /// captured.
    #[default]
    Game,
    /// The whole screen (`video.monitor`), from the start.
    Screen,
}

impl VideoSettings {
    /// The keyframe interval as [`Hns`].
    #[must_use]
    pub fn keyframe_interval(&self) -> Hns {
        Hns::from_millis(i64::from(self.keyframe_interval_ms))
    }
}

/// How the microphone track behaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MicMode {
    /// No microphone and no mic track.
    #[default]
    Off,
    /// Always recording.
    AlwaysOn,
    /// Only while the push-to-talk hotkey is held; silence otherwise.
    PushToTalk,
}

/// The AAC bitrates the Microsoft encoder supports, in kbit/s.
pub const AAC_BITRATES_KBPS: [u32; 4] = [96, 128, 160, 192];

/// Each source's level in the Master mix, in dB (D30). Zero is the level the source is recorded at.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixGains {
    /// Game audio's level in Master.
    pub game: f32,
    /// The microphone's level in Master.
    pub mic: f32,
}

impl Default for MixGains {
    fn default() -> Self {
        Self { game: 0.0, mic: 0.0 }
    }
}

/// The range a Master mix level may take, in dB.
pub const MIX_GAIN_DB: RangeInclusive<f32> = -60.0..=12.0;

/// Audio slots left after Master, Game and Mic: the most per-app tracks a recording can hold.
pub const MAX_APP_TRACKS: usize = MAX_AUDIO_TRACKS as usize - 3;

/// One app whose audio is recorded as its own track (D26). The app is found by its
/// executable name; a tree of processes counts as one app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTrack {
    /// Name shown in the UI and written as the track's title. Unique, case-insensitively.
    pub label: String,
    /// Executable file name, such as `Discord.exe`. A path prefix is ignored.
    pub exe: String,
    /// Whether the track is recorded at all.
    pub enabled: bool,
}

/// The per-app tracks a fresh install records: Discord and Spotify (D27).
fn default_app_tracks() -> Vec<AppTrack> {
    vec![
        AppTrack { label: "Discord".to_owned(), exe: "Discord.exe".to_owned(), enabled: true },
        AppTrack { label: "Spotify".to_owned(), exe: "Spotify.exe".to_owned(), enabled: true },
    ]
}

/// Problems with the per-app tracks, as sentences for the UI.
fn app_track_problems(tracks: &[AppTrack]) -> Vec<String> {
    let mut problems = Vec::new();
    if tracks.len() > MAX_APP_TRACKS {
        problems.push(format!(
            "At most {MAX_APP_TRACKS} app tracks are allowed (there are {}).",
            tracks.len()
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    for track in tracks {
        let label = track.label.trim();
        if label.is_empty() {
            problems.push("Every app track needs a label.".to_owned());
            continue;
        }
        let key = label.to_lowercase();
        if seen.contains(&key) {
            problems.push(format!("Two app tracks are both called \"{label}\"."));
        } else {
            seen.push(key);
        }
        if track.exe.trim().is_empty() {
            problems.push(format!("The app track \"{label}\" needs an executable name."));
        }
    }
    problems
}

/// Audio settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Record the focused game's audio (track 2).
    pub game_audio: bool,
    /// Microphone behaviour (track 3).
    pub mic: MicMode,
    /// Which microphone, as a Windows endpoint id. `None` follows the Windows
    /// default communications device.
    pub mic_device: Option<String>,
    /// Run the mic through the dynamics compressor (DESIGN §6).
    pub mic_compressor: bool,
    /// AAC bitrate per track, kbit/s.
    pub bitrate_kbps: u32,
    /// Each source's level in the Master track (track 1). Read when the sources start.
    pub mix_gain_db: MixGains,
    /// Apps recorded as their own tracks (D26). Settings files from before this field
    /// get the defaults.
    #[serde(default = "default_app_tracks")]
    pub app_tracks: Vec<AppTrack>,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            game_audio: true,
            mic: MicMode::Off,
            mic_device: None,
            mic_compressor: true,
            bitrate_kbps: 192,
            mix_gain_db: MixGains::default(),
            app_tracks: default_app_tracks(),
        }
    }
}

/// Global hotkeys. Defaults mirror the ones most PC gamers already know from
/// GeForce Experience / NVIDIA App.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    /// Save the DVR buffer.
    pub save_clip: Chord,
    /// Start/stop manual recording.
    pub toggle_recording: Chord,
    /// Screenshot.
    pub screenshot: Chord,
    /// Mark a moment of the running recording.
    pub bookmark: Chord,
    /// Push-to-talk (only used when the mic is in push-to-talk mode).
    pub push_to_talk: Option<Chord>,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        use crate::hotkey::Key;
        let chord = |k: Key| Chord::new([Key::ALT, k]).expect("two keys is a valid chord");
        Self {
            save_clip: chord(Key::f(10)),
            toggle_recording: chord(Key::f(9)),
            screenshot: chord(Key::f(1)),
            bookmark: chord(Key::f(8)),
            push_to_talk: None,
        }
    }
}

/// Disk behaviour (DESIGN §4, "Disk strategy").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    /// On a spinning disk, write DVR dumps at a gentle pace so the game doesn't stutter.
    pub pace_hdd_writes: bool,
    /// Pacing rate on HDDs, MB/s (DESIGN §4: 30–50).
    pub hdd_write_mb_per_sec: u32,
    /// Size of each buffered write, MiB (DESIGN §4: 8–16).
    pub write_chunk_mib: u32,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self { pace_hdd_writes: true, hdd_write_mb_per_sec: 40, write_chunk_mib: 16 }
    }
}

/// Upload behaviour (DESIGN §10).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadSettings {
    /// Delete the local file after an upload is *verified* (returned id plus a
    /// status check, never just an HTTP 200). Off by default, deliberately.
    pub delete_local_after_verified_upload: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_design_doc() {
        let s = Settings::default();
        assert_eq!(s.video.max_bitrate_kbps, 20_000);
        assert_eq!(s.video.keyframe_interval(), Hns::from_secs(1));
        assert_eq!(s.hotkeys.save_clip.to_string(), "Alt+F10");
        assert!(!s.uploads.delete_local_after_verified_upload, "auto-delete must default off");
        assert_eq!(s.audio.mic, MicMode::Off);
    }

    #[test]
    fn audio_bitrate_must_be_one_the_encoder_supports() {
        let mut s = Settings::default();
        s.audio.bitrate_kbps = 200;
        assert!(s.problems().iter().any(|p| p.contains("audio bitrate")));
        s.audio.bitrate_kbps = 128;
        assert!(s.problems().is_empty());
    }

    #[test]
    fn default_settings_have_no_problems() {
        assert_eq!(Settings::default().problems(), Vec::<String>::new());
    }

    #[test]
    fn problems_catch_inconsistent_settings() {
        let mut s = Settings::default();
        s.video.min_bitrate_kbps = 30_000;
        s.audio.mic = MicMode::PushToTalk;
        s.hotkeys.screenshot = s.hotkeys.save_clip.clone();
        let problems = s.problems();
        assert_eq!(problems.len(), 3, "{problems:#?}");
        assert!(problems.iter().any(|p| p.contains("both use Alt+F10")));
    }

    #[test]
    fn a_settings_file_that_chose_matroska_still_loads_but_is_flagged() {
        let mut s: Settings = toml::from_str("[recording]\ncontainer = \"mkv\"\n").unwrap();
        assert_eq!(s.recording.container, ContainerFormat::Matroska);
        assert!(s.problems().iter().any(|p| p.contains("Matroska")));
        s.recording.container = ContainerFormat::FragmentedMp4;
        assert!(s.problems().is_empty());
    }

    #[test]
    fn settings_round_trip_through_toml() {
        let mut s = Settings::default();
        s.video.codec = Some(Codec::Hevc);
        s.hotkeys.push_to_talk = Some("Mouse5".parse().unwrap());
        s.recording.output_dir = Some(PathBuf::from("D:/Clips"));
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn master_mix_levels_default_to_zero_and_must_stay_in_range() {
        let mut s = Settings::default();
        assert_eq!(s.audio.mix_gain_db, MixGains::default());
        s.audio.mix_gain_db.mic = 13.0;
        assert!(s.problems().iter().any(|p| p.contains("microphone level")));
        s.audio.mix_gain_db.mic = -60.0;
        assert!(s.problems().is_empty(), "-60 dB is the bottom of the range");
        // A settings file from before the Master track has no levels: they read as zero.
        let older: Settings = toml::from_str("[audio]\nbitrate_kbps = 128\n").unwrap();
        assert_eq!(older.audio.mix_gain_db, MixGains::default());
    }

    #[test]
    fn overlay_toasts_default_on_for_new_and_old_files() {
        assert!(Settings::default().overlay_toasts);
        let older: Settings = toml::from_str("[video]\nfps = 60\n").unwrap();
        assert!(older.overlay_toasts);
        let off: Settings = toml::from_str("overlay_toasts = false\n").unwrap();
        assert!(!off.overlay_toasts);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // An older settings file that only knows about one option.
        let s: Settings = toml::from_str("[video]\nfps = 144\n").unwrap();
        assert_eq!(s.video.fps, 144);
        assert_eq!(s.video.max_bitrate_kbps, 20_000);
        assert_eq!(s.hotkeys, HotkeySettings::default());
        assert!(s.app.close_to_tray);
    }

    #[test]
    fn a_settings_file_without_a_monitor_records_the_primary() {
        // Settings written before the Screen option existed have no `monitor` key.
        let older: Settings = toml::from_str("[video]\nfps = 60\ncapture_cursor = true\n").unwrap();
        assert_eq!(older.video.monitor, None);
        let picked: Settings = toml::from_str("[video]\nmonitor = 1\n").unwrap();
        assert_eq!(picked.video.monitor, Some(1));
    }

    #[test]
    fn old_settings_without_ui_mode_load_as_advanced() {
        let older: Settings = toml::from_str("[video]\nfps = 60\n").unwrap();
        assert_eq!(older.ui_mode, UiMode::Advanced);
        assert_eq!(Settings::default().ui_mode, UiMode::Advanced);
        let simple: Settings = toml::from_str("ui_mode = \"simple\"\n").unwrap();
        assert_eq!(simple.ui_mode, UiMode::Simple);
    }

    #[test]
    fn capture_mode_defaults_to_game_and_loads_from_older_files() {
        assert_eq!(Settings::default().video.capture_mode, CaptureMode::Game);
        let older: Settings = toml::from_str("[video]\nfps = 60\n").unwrap();
        assert_eq!(older.video.capture_mode, CaptureMode::Game);
        let screen: Settings = toml::from_str("[video]\ncapture_mode = \"screen\"\n").unwrap();
        assert_eq!(screen.video.capture_mode, CaptureMode::Screen);
    }

    #[test]
    fn dvr_storage_defaults_to_ram_and_loads_from_older_files() {
        assert_eq!(Settings::default().dvr.storage, DvrStorage::Ram);
        let older: Settings = toml::from_str(
            "[dvr]
enabled = true
ram_ceiling_mib = 512
",
        )
        .unwrap();
        assert_eq!(older.dvr.storage, DvrStorage::Ram);
        assert_eq!(older.dvr.ring_dir, None);
        let disk: Settings = toml::from_str(
            "[dvr]
storage = \"disk\"
",
        )
        .unwrap();
        assert_eq!(disk.dvr.storage, DvrStorage::Disk);
    }

    fn track(label: &str, exe: &str) -> AppTrack {
        AppTrack { label: label.to_owned(), exe: exe.to_owned(), enabled: true }
    }

    #[test]
    fn app_tracks_default_to_discord_and_spotify_enabled() {
        let tracks = Settings::default().audio.app_tracks;
        assert_eq!(tracks, vec![track("Discord", "Discord.exe"), track("Spotify", "Spotify.exe")]);
        assert!(tracks.iter().all(|t| t.enabled));
    }

    #[test]
    fn a_settings_file_without_app_tracks_gets_the_defaults() {
        let older: Settings = toml::from_str("[audio]\nbitrate_kbps = 128\n").unwrap();
        assert_eq!(older.audio.app_tracks, AudioSettings::default().app_tracks);
        assert_eq!(older.audio.app_tracks.len(), 2);
    }

    #[test]
    fn app_track_labels_must_be_unique_ignoring_case() {
        let mut s = Settings::default();
        s.audio.app_tracks = vec![track("Chat", "Discord.exe"), track("chat ", "Spotify.exe")];
        assert!(s.problems().iter().any(|p| p.contains("both called \"chat\"")));
        s.audio.app_tracks[1].label = "Music".to_owned();
        assert!(s.problems().is_empty());
    }

    #[test]
    fn at_most_four_app_tracks_fit_the_audio_slots() {
        assert_eq!(MAX_APP_TRACKS, 4);
        let mut s = Settings::default();
        s.audio.app_tracks =
            (0..MAX_APP_TRACKS).map(|i| track(&format!("App {i}"), "a.exe")).collect();
        assert!(s.problems().is_empty(), "{:#?}", s.problems());
        s.audio.app_tracks.push(track("Extra", "b.exe"));
        assert!(s.problems().iter().any(|p| p.contains("At most 4 app tracks")));
    }

    #[test]
    fn app_tracks_need_a_label_and_an_executable() {
        let mut s = Settings::default();
        s.audio.app_tracks = vec![track("  ", "Discord.exe"), track("Spotify", " ")];
        let problems = s.problems();
        assert!(problems.iter().any(|p| p == "Every app track needs a label."));
        assert!(problems.iter().any(|p| p.contains("\"Spotify\" needs an executable name")));
        assert_eq!(problems.len(), 2, "{problems:#?}");
    }
}
