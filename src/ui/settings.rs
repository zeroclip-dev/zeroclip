//! The Settings tab, and the draft/save logic shared with the Profiles tab.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Button, ComboBox, DragValue, Grid, Slider, TextEdit};
use zc_audio::MicDevice;
use zc_core::hotkey::Chord;
use zc_core::media::{Codec, ContainerFormat};
use zc_core::settings::{AAC_BITRATES_KBPS, MIX_GAIN_DB, MicMode, Settings};
use zc_video::capture::MonitorInfo;

use super::{dvr_megabytes, human_duration};
use crate::app::ZeroClipApp;

const MIC_POLL: Duration = Duration::from_secs(2);

/// Settings being edited. Free-text fields (paths, hotkeys) are kept as strings
/// until saved, so a half-typed hotkey doesn't have to parse.
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

/// Save / Revert, plus the problems from the last attempt.
pub fn save_bar(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    ui.add_space(4.0);
    let dirty = app.draft.is_dirty(&app.settings);
    ui.horizontal(|ui| {
        if ui.add_enabled(dirty, Button::new("Save")).clicked() {
            app.save_settings();
        }
        if ui.add_enabled(dirty, Button::new("Revert")).clicked() {
            app.draft = Draft::from_settings(&app.settings);
        }
        if dirty {
            ui.weak("Unsaved changes");
        }
    });
    for problem in &app.draft.problems {
        ui.colored_label(egui::Color32::LIGHT_RED, problem);
    }
    ui.add_space(4.0);
}

pub fn show(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    egui::Panel::bottom("settings_actions").show(ui, |ui| save_bar(app, ui));

    let default_dir = app.paths.default_output_dir.display().to_string();
    let draft = &mut app.draft;
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        section(ui, "Recording", |ui| {
            Grid::new("recording").num_columns(2).spacing([24.0, 8.0]).show(ui, |ui| {
                ui.label("Output folder");
                ui.add(
                    TextEdit::singleline(&mut draft.output_dir)
                        .hint_text(default_dir.as_str())
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("Container");
                let container = &mut draft.settings.recording.container;
                // Fragmented MP4 is the only container written (D18). A settings file that still
                // says Matroska shows that choice here, and Settings::problems blocks saving it.
                ComboBox::from_id_salt("container")
                    .selected_text(container_label(*container))
                    .show_ui(ui, |ui| {
                        let option = ContainerFormat::FragmentedMp4;
                        ui.selectable_value(container, option, container_label(option));
                    });
                ui.end_row();
            });
        });

        section(ui, "Video", |ui| {
            let (monitors, video) = (&mut draft.monitors, &mut draft.settings.video);
            Grid::new("video").num_columns(2).spacing([24.0, 8.0]).show(ui, |ui| {
                ui.label("Screen");
                ui.horizontal(|ui| {
                    let selected = video.monitor.map_or_else(
                        || "Primary display (default)".to_owned(),
                        |i| {
                            monitors.iter().find(|m| m.index == i).map_or_else(
                                || format!("Display {} (not connected)", i + 1),
                                |m| m.name.clone(),
                            )
                        },
                    );
                    ComboBox::from_id_salt("monitor").selected_text(selected).show_ui(ui, |ui| {
                        ui.selectable_value(&mut video.monitor, None, "Primary display (default)");
                        for m in monitors.iter() {
                            ui.selectable_value(&mut video.monitor, Some(m.index), &m.name);
                        }
                    });
                    if ui.button("Refresh").clicked() {
                        *monitors = zc_video::capture::list_monitors();
                    }
                });
                ui.end_row();

                ui.label("Codec");
                ComboBox::from_id_salt("codec").selected_text(codec_label(video.codec)).show_ui(
                    ui,
                    |ui| {
                        for option in [None, Some(Codec::Av1), Some(Codec::Hevc), Some(Codec::H264)]
                        {
                            ui.selectable_value(&mut video.codec, option, codec_label(option));
                        }
                    },
                );
                ui.end_row();

                ui.label("Frame rate");
                ui.add(DragValue::new(&mut video.fps).range(1..=240).suffix(" fps"));
                ui.end_row();

                ui.label("Bitrate (VBR)");
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut video.min_bitrate_kbps).range(500..=100_000).suffix(" kbps"));
                    ui.label("to");
                    ui.add(DragValue::new(&mut video.max_bitrate_kbps).range(500..=100_000).suffix(" kbps"));
                });
                ui.end_row();

                ui.label("Keyframe interval");
                ui.add(
                    DragValue::new(&mut video.keyframe_interval_ms)
                        .range(500..=4_000)
                        .speed(50)
                        .suffix(" ms"),
                );
                ui.end_row();

                ui.label("");
                ui.checkbox(&mut video.capture_cursor, "Capture the mouse cursor");
                ui.end_row();
            });
            ui.weak("Screen applies when recording starts. Changing it during a recording stops that recording.");
            ui.weak("The keyframe interval is also the trim precision in the editor.");
        });

        section(ui, "DVR buffer", |ui| {
            let window = draft.settings.profiles.default.buffer();
            let max_kbps = draft.settings.video.max_bitrate_kbps;
            let dvr = &mut draft.settings.dvr;
            ui.checkbox(&mut dvr.enabled, "Keep a rolling buffer of recent gameplay");
            ui.horizontal(|ui| {
                ui.label("Memory limit");
                ui.add(DragValue::new(&mut dvr.ram_ceiling_mib).range(64..=16_384).suffix(" MiB"));
            });
            ui.weak(format!(
                "Buffer length is set per game on the Profiles tab. The default ({}) needs about {} MB at the maximum bitrate.",
                human_duration(window),
                dvr_megabytes(draft.settings.profiles.default.buffer_secs, max_kbps),
            ));
        });

        section(ui, "Audio", |ui| {
            let audio = &mut draft.settings.audio;
            let (mics, listed) = (&mut draft.mics, &mut draft.mics_listed);
            ui.checkbox(&mut audio.game_audio, "Record the game's audio (track 2)");
            ui.weak(
                "Only the game in focus is captured: Discord, music and notifications stay \
                 out of your clips. Needs Windows 11; older Windows records all system audio.",
            );
            ui.horizontal(|ui| {
                ui.label("Microphone (track 3)");
                ComboBox::from_id_salt("mic").selected_text(mic_label(audio.mic)).show_ui(ui, |ui| {
                    for option in [MicMode::Off, MicMode::AlwaysOn, MicMode::PushToTalk] {
                        ui.selectable_value(&mut audio.mic, option, mic_label(option));
                    }
                });
            });
            ui.add_enabled_ui(audio.mic != MicMode::Off, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Microphone device");
                    let name = match &audio.mic_device {
                        None => "Windows default".to_owned(),
                        Some(id) => mics.iter().find(|m| &m.id == id).map_or_else(
                            || "Chosen microphone (not connected)".to_owned(),
                            |m| m.name.clone(),
                        ),
                    };
                    ComboBox::from_id_salt("mic-device").selected_text(name).show_ui(ui, |ui| {
                        ui.selectable_value(&mut audio.mic_device, None, "Windows default");
                        for m in mics.iter() {
                            ui.selectable_value(&mut audio.mic_device, Some(m.id.clone()), &m.name);
                        }
                    });
                });
            });
            // Hot-plug: re-list while this screen is open, so a mic plugged in appears by itself.
            if listed.elapsed() >= MIC_POLL {
                *mics = zc_audio::list_microphones();
                *listed = Instant::now();
            }
            ui.ctx().request_repaint_after(MIC_POLL);
            ui.add_enabled(
                audio.mic != MicMode::Off,
                egui::Checkbox::new(&mut audio.mic_compressor, "Even out the mic level (compressor)"),
            );
            ui.horizontal(|ui| {
                ui.label("AAC bitrate");
                // The Microsoft AAC encoder supports exactly these.
                ComboBox::from_id_salt("aac")
                    .selected_text(format!("{} kbps", audio.bitrate_kbps))
                    .show_ui(ui, |ui| {
                        for kbps in AAC_BITRATES_KBPS {
                            ui.selectable_value(&mut audio.bitrate_kbps, kbps, format!("{kbps} kbps"));
                        }
                    });
            });
            ui.add_space(4.0);
            ui.strong("Master track (track 1)");
            ui.weak("The live mix players and Discord play. Each level sets how loud that source is in it.");
            ui.horizontal(|ui| {
                ui.label("Game level");
                ui.add_enabled(
                    audio.game_audio,
                    Slider::new(&mut audio.mix_gain_db.game, MIX_GAIN_DB).suffix(" dB"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Microphone level");
                ui.add_enabled(
                    audio.mic != MicMode::Off,
                    Slider::new(&mut audio.mix_gain_db.mic, MIX_GAIN_DB).suffix(" dB"),
                );
            });
            ui.weak("Turning game audio or the microphone on or off, changing the microphone, or changing a Master level takes effect the next time ZeroClip starts.");
        });

        section(ui, "Hotkeys", |ui| {
            Grid::new("hotkeys").num_columns(2).spacing([24.0, 8.0]).show(ui, |ui| {
                for (label, text) in [
                    ("Save clip", &mut draft.save_clip),
                    ("Start/stop recording", &mut draft.toggle_recording),
                    ("Screenshot", &mut draft.screenshot),
                    ("Push-to-talk", &mut draft.push_to_talk),
                ] {
                    ui.label(label);
                    ui.add(TextEdit::singleline(text).desired_width(200.0).hint_text("none"));
                    ui.end_row();
                }
            });
            ui.weak("Examples: Alt+F10, Ctrl+Shift+S, Mouse5. Hotkeys work while a game has focus.");
        });

        section(ui, "Storage", |ui| {
            let storage = &mut draft.settings.storage;
            ui.checkbox(&mut storage.pace_hdd_writes, "Write gently on hard disks so games don't stutter");
            ui.horizontal(|ui| {
                ui.label("Hard-disk write speed");
                ui.add(DragValue::new(&mut storage.hdd_write_mb_per_sec).range(5..=500).suffix(" MB/s"));
            });
            ui.horizontal(|ui| {
                ui.label("Write chunk size");
                ui.add(DragValue::new(&mut storage.write_chunk_mib).range(1..=64).suffix(" MiB"));
            });
        });

        section(ui, "Uploads", |ui| {
            let uploads = &mut draft.settings.uploads;
            ui.checkbox(
                &mut uploads.delete_local_after_verified_upload,
                "Delete the local file after a verified upload",
            );
            ui.weak(
                "Off by default. Platforms re-encode uploads, can block them after the fact, and \
                 change visibility rules — the local file is your master copy.",
            );
        });
    });
}

pub(super) fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    ui.strong(title);
    ui.add_space(4.0);
    add(ui);
    ui.add_space(6.0);
    ui.separator();
}

fn codec_label(codec: Option<Codec>) -> &'static str {
    codec.map_or("Auto (best the GPU supports)", Codec::name)
}

fn container_label(container: ContainerFormat) -> &'static str {
    match container {
        ContainerFormat::FragmentedMp4 => "Fragmented MP4 (.mp4)",
        ContainerFormat::Matroska => "Matroska (.mkv)",
    }
}

fn mic_label(mode: MicMode) -> &'static str {
    match mode {
        MicMode::Off => "Off",
        MicMode::AlwaysOn => "Always on",
        MicMode::PushToTalk => "Push-to-talk",
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
