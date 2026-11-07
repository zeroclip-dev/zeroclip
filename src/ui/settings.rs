//! Settings draft (the edit buffer the Settings screen binds to). The screen itself is ported to GPUI later.

#![allow(dead_code)] // fields are read once the Settings screen is ported

use std::path::PathBuf;
use std::time::Instant;

use zc_audio::MicDevice;
use zc_core::hotkey::Chord;
use zc_core::settings::Settings;
use zc_video::capture::MonitorInfo;

/// Settings being edited. Free-text fields (paths, hotkeys) stay strings until saved.
#[derive(Clone, Debug)]
pub struct Draft {
    pub settings: Settings,
    output_dir: String,
    save_clip: String,
    toggle_recording: String,
    screenshot: String,
    push_to_talk: String,
    /// Microphones Windows reports, re-listed every `MIC_POLL` while the screen is open.
    mics: Vec<MicDevice>,
    mics_listed: Instant,
    /// Displays Windows reports. Listed when the draft is made, and by the Refresh button.
    monitors: Vec<MonitorInfo>,
    /// Whether the ring folder named here (as typed) is on a spinning disk. Probed when the folder
    /// changes, not on every frame.
    ring_drive: Option<(String, bool)>,
    /// Problems from the last save attempt.
    pub problems: Vec<String>,
}

impl Draft {
    pub fn from_settings(settings: &Settings) -> Self {
        let hotkeys = &settings.hotkeys;
        Self {
            settings: settings.clone(),
            output_dir: settings
                .recording
                .output_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            save_clip: hotkeys.save_clip.to_string(),
            toggle_recording: hotkeys.toggle_recording.to_string(),
            screenshot: hotkeys.screenshot.to_string(),
            push_to_talk: hotkeys.push_to_talk.as_ref().map(Chord::to_string).unwrap_or_default(),
            mics: zc_audio::list_microphones(),
            mics_listed: Instant::now(),
            monitors: zc_video::capture::list_monitors(),
            ring_drive: None,
            problems: Vec::new(),
        }
    }

    /// The settings this draft describes, or everything wrong with it.
    pub fn build(&self) -> Result<Settings, Vec<String>> {
        let mut settings = self.settings.clone();
        let mut problems = Vec::new();

        let dir = self.output_dir.trim();
        settings.recording.output_dir = (!dir.is_empty()).then(|| PathBuf::from(dir));

        let mut chord = |label: &str, text: &str| match text.parse::<Chord>() {
            Ok(chord) => Some(chord),
            Err(e) => {
                problems.push(format!("{label} hotkey: {e}."));
                None
            }
        };
        if let Some(c) = chord("Save clip", &self.save_clip) {
            settings.hotkeys.save_clip = c;
        }
        if let Some(c) = chord("Recording", &self.toggle_recording) {
            settings.hotkeys.toggle_recording = c;
        }
        if let Some(c) = chord("Screenshot", &self.screenshot) {
            settings.hotkeys.screenshot = c;
        }
        settings.hotkeys.push_to_talk = if self.push_to_talk.trim().is_empty() {
            None
        } else {
            chord("Push-to-talk", &self.push_to_talk)
        };

        problems.extend(settings.problems());
        if problems.is_empty() { Ok(settings) } else { Err(problems) }
    }

    /// Whether saving would change anything.
    pub fn is_dirty(&self, saved: &Settings) -> bool {
        self.build().map_or(true, |settings| &settings != saved)
    }
}

/// Placeholder until the Settings screen is ported to GPUI.
pub struct SettingsView {
    #[allow(dead_code)] // read once the screen has content
    app: gpui::WeakEntity<crate::app::ZeroClipApp>,
}

impl SettingsView {
    pub fn new(app: gpui::WeakEntity<crate::app::ZeroClipApp>) -> Self {
        Self { app }
    }
}

impl gpui::Render for SettingsView {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div().p_4().child("Settings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_draft_is_clean() {
        let settings = Settings::default();
        let draft = Draft::from_settings(&settings);
        assert!(!draft.is_dirty(&settings));
        assert_eq!(draft.build(), Ok(settings));
    }

    #[test]
    fn bad_hotkeys_block_saving() {
        let mut draft = Draft::from_settings(&Settings::default());
        draft.save_clip = "Alt+Nope".to_owned();
        let problems = draft.build().unwrap_err();
        assert!(problems[0].starts_with("Save clip hotkey"), "{problems:?}");
    }

    #[test]
    fn empty_output_dir_means_default() {
        let mut draft = Draft::from_settings(&Settings::default());
        draft.output_dir = "  ".to_owned();
        assert_eq!(draft.build().unwrap().recording.output_dir, None);
        draft.output_dir = r"D:\Clips".to_owned();
        assert_eq!(draft.build().unwrap().recording.output_dir, Some(PathBuf::from(r"D:\Clips")));
    }
}
