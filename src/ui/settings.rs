//! The Settings screen: a draft of the settings being edited, and the view that edits it. Save
//! validates and applies the draft; Revert throws it away.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px, rgb,
};
use zc_audio::MicDevice;
use zc_core::hotkey::Chord;
use zc_core::media::{Codec, ContainerFormat};
use zc_core::settings::{
    AAC_BITRATES_KBPS, CaptureMode, DvrStorage, MIX_GAIN_DB, MicMode, Settings,
};
use zc_video::capture::MonitorInfo;

use super::components::{
    button::{button, button_primary},
    labeled_toggle::labeled_toggle,
    number_stepper::{Stepper, number_stepper},
    segmented::segmented,
    text_field::text_field,
};
use super::theme;
use super::{dvr_megabytes, dvr_ring_megabytes, effective_ceiling, human_duration};
use crate::app::ZeroClipApp;

const MIC_POLL: Duration = Duration::from_secs(2);
const HINT: u32 = theme::MUTED_FG;

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
    /// The user picked "Custom" for the frame rate. UI state only; never saved.
    fps_custom: bool,
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
            fps_custom: false,
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

/// Wires a draft edit to a component callback: `bind!(cx, |d, v: &bool| d.x = *v)`.
macro_rules! bind {
    ($cx:expr, |$d:ident, $v:ident: $t:ty| $body:expr) => {
        $cx.listener(|this: &mut SettingsView, $v: $t, _, cx| {
            this.edit(cx, |$d: &mut Draft| {
                $body;
            })
        })
    };
}

pub struct SettingsView {
    app: WeakEntity<ZeroClipApp>,
    /// Text-field focus: output folder, then the four hotkeys. Made on the first render.
    focus: Vec<FocusHandle>,
    /// A repaint is already scheduled for the microphone re-list.
    polling: bool,
}

impl SettingsView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app, focus: Vec::new(), polling: false }
    }

    /// Changes the draft and repaints.
    fn edit(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut Draft)) {
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, cx| {
                f(&mut app.draft);
                cx.notify();
            });
        }
        cx.notify();
    }
}

/// Where the ring file goes: its own folder, else the output folder (as typed), else the default.
fn ring_dir_text(draft: &Draft, default_dir: &str) -> String {
    draft.settings.dvr.ring_dir.as_ref().map_or_else(
        || {
            let typed = draft.output_dir.trim();
            if typed.is_empty() { default_dir.to_owned() } else { typed.to_owned() }
        },
        |dir| dir.display().to_string(),
    )
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
}

fn row(label: &'static str, control: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .min_h(px(30.))
        .pb_2()
        .border_b_1()
        .border_color(rgb(theme::BORDER))
        .child(div().w(px(190.)).flex_shrink_0().text_color(rgb(theme::FG)).child(label))
        .child(control)
}

fn hint(text: impl Into<SharedString>) -> gpui::Div {
    div().text_sm().text_color(rgb(HINT)).child(text.into())
}

/// Frame-rate presets, in segment order; the segment after them is "Custom".
const FPS_PRESETS: [u32; 3] = [30, 60, 120];

/// Which frame-rate segment is selected: the preset equal to `fps`, else (or when the user chose
/// Custom) the Custom segment, whose index is `FPS_PRESETS.len()`.
fn fps_segment(fps: u32, custom: bool) -> usize {
    if custom {
        return FPS_PRESETS.len();
    }
    FPS_PRESETS.iter().position(|p| *p == fps).unwrap_or(FPS_PRESETS.len())
}

fn labels<const N: usize>(items: [&str; N]) -> Vec<SharedString> {
    items.into_iter().map(|s| SharedString::from(s.to_owned())).collect()
}

fn stepper(min: f64, max: f64, step: f64, suffix: &'static str, enabled: bool) -> Stepper {
    Stepper { min, max, step, suffix, enabled }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div() };
        if self.focus.is_empty() {
            self.focus = (0..5).map(|_| cx.focus_handle()).collect();
        }
        // Hot-plug: re-list microphones while this screen is open, so a new one appears by itself.
        if !self.polling {
            self.polling = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(MIC_POLL).await;
                let _ = this.update(cx, |this, cx| {
                    this.polling = false;
                    cx.notify();
                });
            })
            .detach();
        }
        let default_dir = app.read(cx).paths.default_output_dir.display().to_string();
        app.update(cx, |app, _| {
            let d = &mut app.draft;
            if d.mics_listed.elapsed() >= MIC_POLL {
                d.mics = zc_audio::list_microphones();
                d.mics_listed = Instant::now();
            }
            if d.settings.dvr.storage == DvrStorage::Disk {
                let dir = ring_dir_text(d, &default_dir);
                if d.ring_drive.as_ref().is_none_or(|(probed, _)| *probed != dir) {
                    let hdd = zc_win::drive::seek_penalty(std::path::Path::new(&dir))
                        .ok()
                        .flatten()
                        .unwrap_or(false);
                    d.ring_drive = Some((dir, hdd));
                }
            }
        });
        let (d, saved) = {
            let a = app.read(cx);
            (a.draft.clone(), a.settings.clone())
        };
        let dirty = d.is_dirty(&saved);
        let (video, audio, dvr) = (&d.settings.video, &d.settings.audio, &d.settings.dvr);

        // Text fields first: each holds `cx` only while it is built.
        let field = |ix: usize,
                     id: &'static str,
                     value: &str,
                     ph: &str,
                     width: f32,
                     set: fn(&mut Draft, &str)| {
            text_field(
                id,
                &self.focus[ix],
                window,
                value,
                ph,
                width,
                cx.listener(move |this, v: &String, _, cx| this.edit(cx, |d| set(d, v))),
            )
        };
        let f_out = field(0, "f-out", &d.output_dir, &default_dir, 380., |d, v| {
            d.output_dir = v.to_owned()
        });
        let f_save =
            field(1, "f-save", &d.save_clip, "none", 200., |d, v| d.save_clip = v.to_owned());
        let f_rec = field(2, "f-rec", &d.toggle_recording, "none", 200., |d, v| {
            d.toggle_recording = v.to_owned();
        });
        let f_shot =
            field(3, "f-shot", &d.screenshot, "none", 200., |d, v| d.screenshot = v.to_owned());
        let f_ptt =
            field(4, "f-ptt", &d.push_to_talk, "none", 200., |d, v| d.push_to_talk = v.to_owned());

        let recording = section("Recording").child(row("Output folder", f_out)).child(row(
            "Container",
            // Fragmented MP4 is the only container written (D18). A settings file that still
            // says Matroska shows no chip selected, and Settings::problems blocks saving it.
            segmented(
                "container",
                labels(["Fragmented MP4 (.mp4)"]),
                (d.settings.recording.container == ContainerFormat::FragmentedMp4).then_some(0),
                true,
                bind!(cx, |d, _i: &usize| d.settings.recording.container =
                    ContainerFormat::FragmentedMp4),
            ),
        ));

        let screen_on = video.capture_mode == CaptureMode::Screen;
        let mut monitor_opts = labels(["Primary display (default)"]);
        monitor_opts.extend(d.monitors.iter().map(|m| SharedString::from(m.name.clone())));
        let monitor_sel = match video.monitor {
            None => Some(0),
            Some(i) => d.monitors.iter().position(|m| m.index == i).map(|p| p + 1),
        };
        let monitor_lost = video.monitor.filter(|_| monitor_sel.is_none());
        let codecs = [None, Some(Codec::Av1), Some(Codec::Hevc), Some(Codec::H264)];
        let refresh =
            button("monitor-refresh", "Refresh").opacity(if screen_on { 1.0 } else { 0.4 });
        let refresh = if screen_on {
            refresh.on_click(bind!(cx, |d, _e: &gpui::ClickEvent| d.monitors =
                zc_video::capture::list_monitors()))
        } else {
            refresh
        };
        let fps_sel = fps_segment(video.fps, d.fps_custom);
        let video_sec = section("Video")
            .child(row(
                "Capture",
                segmented(
                    "capture",
                    labels(["Game only (default)", "Entire screen"]),
                    Some(usize::from(screen_on)),
                    true,
                    bind!(cx, |d, i: &usize| d.settings.video.capture_mode = if *i == 0 {
                        CaptureMode::Game
                    } else {
                        CaptureMode::Screen
                    }),
                ),
            ))
            .children(
                (!screen_on)
                    .then(|| hint("Records only games that have a profile. Add games on the Profiles tab.")),
            )
            .child(row(
                "Screen",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(segmented(
                        "monitor",
                        monitor_opts,
                        monitor_sel,
                        screen_on,
                        bind!(cx, |d, i: &usize| d.settings.video.monitor = i
                            .checked_sub(1)
                            .and_then(|p| d.monitors.get(p))
                            .map(|m| m.index)),
                    ))
                    .child(refresh),
            ))
            .children(monitor_lost.map(|i| hint(format!("Display {} is not connected.", i + 1))))
            .child(row(
                "Codec",
                segmented(
                    "codec",
                    labels(["Auto (best the GPU supports)", "AV1", "HEVC", "H.264"]),
                    codecs.iter().position(|c| *c == video.codec),
                    true,
                    bind!(cx, |d, i: &usize| d.settings.video.codec =
                        [None, Some(Codec::Av1), Some(Codec::Hevc), Some(Codec::H264)][*i]),
                ),
            ))
            .child(row(
                "Frame rate",
                segmented(
                    "fps-preset",
                    labels(["30", "60", "120", "Custom"]),
                    Some(fps_sel),
                    true,
                    bind!(cx, |d, i: &usize| match FPS_PRESETS.get(*i) {
                        Some(p) => {
                            d.settings.video.fps = *p;
                            d.fps_custom = false;
                        }
                        None => d.fps_custom = true,
                    }),
                ),
            ))
            .children((fps_sel == FPS_PRESETS.len()).then(|| {
                row(
                    "",
                    number_stepper(
                        "fps",
                        f64::from(video.fps),
                        stepper(1., 240., 1., " fps", true),
                        bind!(cx, |d, v: &f64| d.settings.video.fps = *v as u32),
                    ),
                )
            }))
            .child(row(
                "Minimum bitrate (VBR)",
                number_stepper(
                    "minbr",
                    f64::from(video.min_bitrate_kbps),
                    stepper(500., 100_000., 500., " kbps", true),
                    bind!(cx, |d, v: &f64| d.settings.video.min_bitrate_kbps = *v as u32),
                ),
            ))
            .child(row(
                "Maximum bitrate (VBR)",
                number_stepper(
                    "maxbr",
                    f64::from(video.max_bitrate_kbps),
                    stepper(500., 100_000., 500., " kbps", true),
                    bind!(cx, |d, v: &f64| d.settings.video.max_bitrate_kbps = *v as u32),
                ),
            ))
            .child(row(
                "Keyframe interval",
                number_stepper(
                    "kf",
                    f64::from(video.keyframe_interval_ms),
                    stepper(500., 4_000., 50., " ms", true),
                    bind!(cx, |d, v: &f64| d.settings.video.keyframe_interval_ms = *v as u32),
                ),
            ))
            .child(labeled_toggle(
                "cursor",
                "Capture the mouse cursor",
                video.capture_cursor,
                true,
                bind!(cx, |d, v: &bool| d.settings.video.capture_cursor = *v),
            ))
            .child(hint(
                "Screen applies when recording starts. Changing it during a recording stops that recording.",
            ))
            .child(hint("The keyframe interval is also the trim precision in the editor."));

        // DVR buffer
        let profiles = &d.settings.profiles;
        let max_kbps = video.max_bitrate_kbps;
        let mut dvr_sec = section("DVR buffer")
            .child(labeled_toggle(
                "dvr-on",
                "Keep a rolling buffer of recent gameplay",
                dvr.enabled,
                true,
                bind!(cx, |d, v: &bool| d.settings.dvr.enabled = *v),
            ))
            .child(row(
                "Keep the buffer in",
                segmented(
                    "dvr-storage",
                    labels(["RAM", "Disk (SSD recommended, uses almost no RAM)"]),
                    Some(usize::from(dvr.storage == DvrStorage::Disk)),
                    true,
                    bind!(cx, |d, i: &usize| d.settings.dvr.storage =
                        if *i == 0 { DvrStorage::Ram } else { DvrStorage::Disk }),
                ),
            ));
        if dvr.storage == DvrStorage::Disk {
            let dir = ring_dir_text(&d, &default_dir);
            let on_hdd = d.ring_drive.as_ref().is_some_and(|(p, hdd)| *p == dir && *hdd);
            // The engine sizes the file once, for the longest profile.
            let longest = profiles
                .games
                .iter()
                .map(|g| g.buffer_secs)
                .chain([profiles.default.buffer_secs])
                .max()
                .unwrap_or_default();
            dvr_sec = dvr_sec.child(hint(format!(
                "The ring file, sized for your longest profile ({}), takes about {} MB in {dir}, and the buffer uses almost no RAM. It applies when ZeroClip restarts.",
                human_duration(zc_core::Hns::from_secs(i64::from(longest))),
                dvr_ring_megabytes(longest, max_kbps, audio.bitrate_kbps),
            )));
            if on_hdd {
                dvr_sec = dvr_sec.child(div().text_sm().text_color(rgb(theme::WARN)).child(
                    "That folder is on a hard disk. Constant buffer writes can make games stutter; pick an SSD.",
                ));
            }
        } else {
            let ceiling = effective_ceiling(dvr.ram_ceiling_mib);
            dvr_sec = dvr_sec.child(row(
                "Memory limit",
                number_stepper(
                    "ram",
                    f64::from(dvr.ram_ceiling_mib),
                    stepper(64., 16_384., 64., " MiB", true),
                    bind!(cx, |d, v: &f64| d.settings.dvr.ram_ceiling_mib = *v as u32),
                ),
            ));
            if ceiling.from_system_ram {
                dvr_sec = dvr_sec.child(hint(format!(
                    "Your RAM caps this at {} MiB (5% of the total), so that is the limit in use.",
                    ceiling.mib
                )));
            }
            dvr_sec = dvr_sec.child(hint(format!(
                "Buffer length is set per game on the Profiles tab. The default ({}) needs about {} MB at the maximum bitrate.",
                human_duration(profiles.default.buffer()),
                dvr_megabytes(profiles.default.buffer_secs, max_kbps),
            )));
        }

        // Audio
        let mic_on = audio.mic != MicMode::Off;
        let mut mic_opts = labels(["Windows default"]);
        mic_opts.extend(d.mics.iter().map(|m| SharedString::from(m.name.clone())));
        let mic_sel = match &audio.mic_device {
            None => Some(0),
            Some(id) => d.mics.iter().position(|m| &m.id == id).map(|p| p + 1),
        };
        let mic_lost = audio.mic_device.is_some() && mic_sel.is_none();
        let gain_lo = f64::from(*MIX_GAIN_DB.start());
        let gain_hi = f64::from(*MIX_GAIN_DB.end());
        let audio_sec = section("Audio")
            .child(labeled_toggle(
                "game-audio",
                "Record the game's audio (track 2)",
                audio.game_audio,
                true,
                bind!(cx, |d, v: &bool| d.settings.audio.game_audio = *v),
            ))
            .child(hint(
                "Only the game in focus is captured: Discord, music and notifications stay out of your clips. Needs Windows 11; older Windows records all system audio.",
            ))
            .child(row(
                "Microphone (track 3)",
                segmented(
                    "mic",
                    labels(["Off", "Always on", "Push-to-talk"]),
                    Some(match audio.mic {
                        MicMode::Off => 0,
                        MicMode::AlwaysOn => 1,
                        MicMode::PushToTalk => 2,
                    }),
                    true,
                    bind!(cx, |d, i: &usize| d.settings.audio.mic =
                        [MicMode::Off, MicMode::AlwaysOn, MicMode::PushToTalk][*i]),
                ),
            ))
            .child(row(
                "Microphone device",
                segmented(
                    "mic-device",
                    mic_opts,
                    mic_sel,
                    mic_on,
                    bind!(cx, |d, i: &usize| d.settings.audio.mic_device = i
                        .checked_sub(1)
                        .and_then(|p| d.mics.get(p))
                        .map(|m| m.id.clone())),
                ),
            ))
            .children(mic_lost.then(|| hint("The chosen microphone is not connected.")))
            .child(labeled_toggle(
                "mic-comp",
                "Even out the mic level (compressor)",
                audio.mic_compressor,
                mic_on,
                bind!(cx, |d, v: &bool| d.settings.audio.mic_compressor = *v),
            ))
            .child(row(
                "AAC bitrate",
                // The Microsoft AAC encoder supports exactly these.
                segmented(
                    "aac",
                    AAC_BITRATES_KBPS
                        .iter()
                        .map(|k| SharedString::from(format!("{k} kbps")))
                        .collect(),
                    AAC_BITRATES_KBPS.iter().position(|k| *k == audio.bitrate_kbps),
                    true,
                    bind!(cx, |d, i: &usize| d.settings.audio.bitrate_kbps =
                        AAC_BITRATES_KBPS[*i]),
                ),
            ))
            .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Master track (track 1)"))
            .child(hint(
                "The live mix players and Discord play. Each level sets how loud that source is in it.",
            ))
            .child(row(
                "Game level",
                number_stepper(
                    "gain-game",
                    f64::from(audio.mix_gain_db.game),
                    stepper(gain_lo, gain_hi, 1., " dB", audio.game_audio),
                    bind!(cx, |d, v: &f64| d.settings.audio.mix_gain_db.game = *v as f32),
                ),
            ))
            .child(row(
                "Microphone level",
                number_stepper(
                    "gain-mic",
                    f64::from(audio.mix_gain_db.mic),
                    stepper(gain_lo, gain_hi, 1., " dB", mic_on),
                    bind!(cx, |d, v: &f64| d.settings.audio.mix_gain_db.mic = *v as f32),
                ),
            ))
            .child(hint(
                "Turning game audio or the microphone on or off, changing the microphone, or changing a Master level takes effect the next time ZeroClip starts.",
            ));

        let hotkeys = section("Hotkeys")
            .child(row("Save clip", f_save))
            .child(row("Start/stop recording", f_rec))
            .child(row("Screenshot", f_shot))
            .child(row("Push-to-talk", f_ptt))
            .child(labeled_toggle(
                "toasts",
                "Show a small card when a clip, recording or screenshot is saved",
                d.settings.overlay_toasts,
                true,
                bind!(cx, |d, v: &bool| d.settings.overlay_toasts = *v),
            ))
            .child(hint(
                "Examples: Alt+F10, Ctrl+Shift+S, Mouse5. Hotkeys work while a game has focus.",
            ));

        let st = &d.settings.storage;
        let storage = section("Storage")
            .child(labeled_toggle(
                "pace",
                "Write gently on hard disks so games don't stutter",
                st.pace_hdd_writes,
                true,
                bind!(cx, |d, v: &bool| d.settings.storage.pace_hdd_writes = *v),
            ))
            .child(row(
                "Hard-disk write speed",
                number_stepper(
                    "hdd",
                    f64::from(st.hdd_write_mb_per_sec),
                    stepper(5., 500., 5., " MB/s", true),
                    bind!(cx, |d, v: &f64| d.settings.storage.hdd_write_mb_per_sec = *v as u32),
                ),
            ))
            .child(row(
                "Write chunk size",
                number_stepper(
                    "chunk",
                    f64::from(st.write_chunk_mib),
                    stepper(1., 64., 1., " MiB", true),
                    bind!(cx, |d, v: &f64| d.settings.storage.write_chunk_mib = *v as u32),
                ),
            ));

        let uploads = section("Uploads")
            .child(labeled_toggle(
                "del-local",
                "Delete the local file after a verified upload",
                d.settings.uploads.delete_local_after_verified_upload,
                true,
                bind!(cx, |d, v: &bool| d.settings.uploads.delete_local_after_verified_upload =
                    *v),
            ))
            .child(hint(
                "Off by default. Platforms re-encode uploads, can block them after the fact, and change visibility rules; the local file is your master copy.",
            ));

        let dim = if dirty { 1.0 } else { 0.4 };
        let save = button_primary("save", "Save").opacity(dim);
        let revert = button("revert", "Revert").opacity(dim);
        let (save, revert) = if dirty {
            (
                save.on_click(cx.listener(|this, _, _, cx| {
                    if let Some(app) = this.app.upgrade() {
                        app.update(cx, |app, cx| {
                            app.save_settings();
                            cx.notify();
                        });
                    }
                    cx.notify();
                })),
                revert.on_click(cx.listener(|this, _, _, cx| {
                    if let Some(app) = this.app.upgrade() {
                        app.update(cx, |app, cx| {
                            app.draft = Draft::from_settings(&app.settings);
                            cx.notify();
                        });
                    }
                    cx.notify();
                })),
            )
        } else {
            (save, revert)
        };
        let bar = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .bg(rgb(theme::BG))
            .border_t_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(save)
                    .child(revert)
                    .children(dirty.then(|| hint("Unsaved changes"))),
            )
            .children(
                d.problems
                    .iter()
                    .map(|p| div().text_sm().text_color(rgb(theme::DANGER)).child(p.clone())),
            );

        // Tray and startup (P4.15, P4.16)
        let app_sec = section("App")
            .child(labeled_toggle(
                "close-tray",
                "Close to the tray instead of quitting",
                d.settings.app.close_to_tray,
                true,
                bind!(cx, |d, v: &bool| d.settings.app.close_to_tray = *v),
            ))
            .child(labeled_toggle(
                "start-win",
                "Start with Windows, in the tray",
                d.settings.app.start_with_windows,
                true,
                bind!(cx, |d, v: &bool| d.settings.app.start_with_windows = *v),
            ))
            .child(hint("Quit from the tray menu to close ZeroClip while it runs in the tray."));

        let sections: [AnyElement; 9] = [
            recording.into_any_element(),
            video_sec.into_any_element(),
            dvr_sec.into_any_element(),
            audio_sec.into_any_element(),
            hotkeys.into_any_element(),
            storage.into_any_element(),
            uploads.into_any_element(),
            app_sec.into_any_element(),
            div().h(px(8.)).into_any_element(),
        ];
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .px_4()
                    .children(sections),
            )
            .child(bar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_presets_select_themselves_everything_else_is_custom() {
        for (fps, seg) in [(30, 0), (60, 1), (120, 2), (144, 3), (1, 3), (240, 3)] {
            assert_eq!(fps_segment(fps, false), seg, "{fps}");
        }
        // Choosing Custom keeps the stepper open even on a preset value.
        assert_eq!(fps_segment(60, true), 3);
    }

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
