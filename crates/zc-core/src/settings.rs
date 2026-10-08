//! User settings.
//!
//! Every field has a serde default, so settings files written by older versions
//! keep loading as options are added. The app stores this as TOML.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hotkey::Chord;
use crate::media::{Codec, ContainerFormat};
use crate::profile::ProfileSet;
use crate::time::Hns;

/// Everything the user can configure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
        if self.audio.mic == MicMode::PushToTalk && self.hotkeys.push_to_talk.is_none() {
            problems.push("Push-to-talk mode needs a push-to-talk hotkey.".to_owned());
        }
        if self.recording.container == ContainerFormat::Matroska {
            problems.push(
                "Matroska recordings aren't written. Choose Fragmented MP4 (.mp4).".to_owned(),
            );
        }

        let hotkeys = &self.hotkeys;
        let bound = [
            ("Save clip", Some(&hotkeys.save_clip)),
            ("Start/stop recording", Some(&hotkeys.toggle_recording)),
            ("Screenshot", Some(&hotkeys.screenshot)),
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
}

impl Default for DvrSettings {
    fn default() -> Self {
        Self { enabled: true, ram_ceiling_mib: 1_024 }
    }
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
        }
    }
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

/// Audio settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Record the focused game's audio (track 1).
    pub game_audio: bool,
    /// Microphone behaviour (track 2).
    pub mic: MicMode,
    /// Which microphone, as a Windows endpoint id. `None` follows the Windows
    /// default communications device.
    pub mic_device: Option<String>,
    /// Run the mic through the dynamics compressor (DESIGN §6).
    pub mic_compressor: bool,
    /// AAC bitrate per track, kbit/s.
    pub bitrate_kbps: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            game_audio: true,
            mic: MicMode::Off,
            mic_device: None,
            mic_compressor: true,
            bitrate_kbps: 192,
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
    fn missing_fields_fall_back_to_defaults() {
        // An older settings file that only knows about one option.
        let s: Settings = toml::from_str("[video]\nfps = 144\n").unwrap();
        assert_eq!(s.video.fps, 144);
        assert_eq!(s.video.max_bitrate_kbps, 20_000);
        assert_eq!(s.hotkeys, HotkeySettings::default());
    }
}
