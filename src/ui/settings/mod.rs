//! The Settings screen: a draft of the settings being edited, and the view that edits it. Save
//! validates and applies the draft; Revert throws it away. A Simple | Advanced switch at the top
//! picks the view: Simple is the few settings most people change, Advanced is the full list.

mod simple;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled, Subscription,
    WeakEntity, Window, div, px, rgb, transparent_black,
};
use zc_audio::MicDevice;
use zc_core::control::Command;
use zc_core::hotkey::{Chord, Key};
use zc_core::media::{Codec, ContainerFormat};
use zc_core::settings::{
    AAC_BITRATES_KBPS, CaptureMode, DvrStorage, MIX_GAIN_DB, MicMode, Settings, StorageLayout,
    UiMode,
};
use zc_video::capture::MonitorInfo;

use super::components::{
    button::{button, button_primary_if},
    hotkey_field::{
        HotkeyFieldEvent, HotkeyFieldProps, ModifierFlags, RawCapture, handle_listening_key,
        hotkey_field,
    },
    number_stepper::{Stepper, number_stepper},
    segmented::segmented,
    status_dot::status_dot,
    text_field::text_field,
    toggle::toggle,
};
use super::icons::icon;
use super::theme;
use super::{dvr_megabytes, dvr_ring_megabytes, effective_ceiling, human_duration};
use crate::app::ZeroClipApp;
use simple::{
    MAX_MINUTES, MIN_MINUTES, QUALITIES, buffer_bitrate_bps, current_quality, free_space,
    minutes_from_buffer_secs, quality_line, storage_line,
};

const MIC_POLL: Duration = Duration::from_secs(2);
/// Brief grace window after committing a hotkey before resuming global Raw Input matching, so the
/// concurrent `WM_INPUT` event for that exact key press cannot fire the old/bound action.
const HOTKEY_RESUME_GRACE: Duration = Duration::from_millis(150);
/// How long a GPUI key-down waits for its raw twin before the toolkit fallback commits it.
const GPUI_KEY_DEFER: Duration = Duration::from_millis(60);

/// Width above which Advanced settings uses a two-column nav + content layout. At 760 px and
/// narrower it collapses to one column.
const TWO_COL_MIN_W: f32 = 760.0;
/// Maximum content column width in Advanced view.
const CONTENT_MAX_W: f32 = 760.0;
/// Maximum centered width in Simple view.
const SIMPLE_MAX_W: f32 = 680.0;

/// Settings sections shown in Advanced view's navigation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsSection {
    #[default]
    Capture,
    Audio,
    Hotkeys,
    Storage,
    Advanced,
}

impl SettingsSection {
    const ALL: [Self; 5] =
        [Self::Capture, Self::Audio, Self::Hotkeys, Self::Storage, Self::Advanced];

    fn id(self) -> &'static str {
        match self {
            Self::Capture => "nav-capture",
            Self::Audio => "nav-audio",
            Self::Hotkeys => "nav-hotkeys",
            Self::Storage => "nav-storage",
            Self::Advanced => "nav-advanced",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Capture => "Capture",
            Self::Audio => "Audio",
            Self::Hotkeys => "Hotkeys",
            Self::Storage => "Storage",
            Self::Advanced => "Advanced",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Capture => "film",
            Self::Audio => "headphones",
            Self::Hotkeys => "sliders-horizontal",
            Self::Storage => "folder-open",
            Self::Advanced => "settings",
        }
    }
}

fn is_two_column(viewport_width: f32) -> bool {
    viewport_width > TWO_COL_MIN_W
}

/// Settings being edited. Free-text fields (paths, hotkeys) stay strings until saved.
#[derive(Clone, Debug)]
pub struct Draft {
    pub settings: Settings,
    pub output_dir: String,
    save_clip: String,
    toggle_recording: String,
    screenshot: String,
    bookmark: String,
    push_to_talk: String,
    /// Path to `ffmpeg.exe` for the Editor; empty looks next to ZeroClip, then on `PATH`.
    ffmpeg_path: String,
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
            bookmark: hotkeys.bookmark.to_string(),
            push_to_talk: hotkeys.push_to_talk.as_ref().map(Chord::to_string).unwrap_or_default(),
            ffmpeg_path: settings
                .editor
                .ffmpeg_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
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

        let ffmpeg = self.ffmpeg_path.trim();
        settings.editor.ffmpeg_path = (!ffmpeg.is_empty()).then(|| PathBuf::from(ffmpeg));

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
        if let Some(c) = chord("Bookmark", &self.bookmark) {
            settings.hotkeys.bookmark = c;
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

/// The five configurable global hotkey slots on the Settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeySlot {
    SaveClip,
    ToggleRecording,
    Screenshot,
    Bookmark,
    PushToTalk,
}

impl HotkeySlot {
    const ALL: [Self; 5] =
        [Self::SaveClip, Self::ToggleRecording, Self::Screenshot, Self::Bookmark, Self::PushToTalk];

    const fn index(self) -> usize {
        match self {
            Self::SaveClip => 0,
            Self::ToggleRecording => 1,
            Self::Screenshot => 2,
            Self::Bookmark => 3,
            Self::PushToTalk => 4,
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::SaveClip => "f-save",
            Self::ToggleRecording => "f-rec",
            Self::Screenshot => "f-shot",
            Self::Bookmark => "f-mark",
            Self::PushToTalk => "f-ptt",
        }
    }

    const fn action_label(self) -> &'static str {
        match self {
            Self::SaveClip => "Save clip",
            Self::ToggleRecording => "Start or stop recording",
            Self::Screenshot => "Screenshot",
            Self::Bookmark => "Bookmark",
            Self::PushToTalk => "Push-to-talk",
        }
    }

    const fn optional(self) -> bool {
        matches!(self, Self::PushToTalk)
    }

    fn get(self, draft: &Draft) -> &str {
        match self {
            Self::SaveClip => &draft.save_clip,
            Self::ToggleRecording => &draft.toggle_recording,
            Self::Screenshot => &draft.screenshot,
            Self::Bookmark => &draft.bookmark,
            Self::PushToTalk => &draft.push_to_talk,
        }
    }

    fn set(self, draft: &mut Draft, value: String) {
        match self {
            Self::SaveClip => draft.save_clip = value,
            Self::ToggleRecording => draft.toggle_recording = value,
            Self::Screenshot => draft.screenshot = value,
            Self::Bookmark => draft.bookmark = value,
            Self::PushToTalk => draft.push_to_talk = value,
        }
    }
}

/// Live duplicate-hotkey check over the current draft strings. Mirrors `Settings::problems()` so
/// conflicting bindings are surfaced immediately inside the Hotkeys card before clicking Save.
fn duplicate_hotkeys(draft: &Draft) -> (Vec<String>, [bool; 5]) {
    let parsed: [Option<Chord>; 5] =
        HotkeySlot::ALL.map(|slot| slot.get(draft).trim().parse::<Chord>().ok());
    let mut warnings = Vec::new();
    let mut conflicting = [false; 5];
    for i in 0..HotkeySlot::ALL.len() {
        for j in (i + 1)..HotkeySlot::ALL.len() {
            if let (Some(a), Some(b)) = (&parsed[i], &parsed[j])
                && a == b
            {
                conflicting[i] = true;
                conflicting[j] = true;
                warnings.push(format!(
                    "{} and {} both use {a}.",
                    HotkeySlot::ALL[i].action_label(),
                    HotkeySlot::ALL[j].action_label(),
                ));
            }
        }
    }
    (warnings, conflicting)
}

pub struct SettingsView {
    app: WeakEntity<ZeroClipApp>,
    /// Text-field focus handles: output folder (0) and ffmpeg path (1). Made on the first render.
    text_focus: Vec<FocusHandle>,
    /// Focus handles for the five hotkey capture buttons, indexed by [`HotkeySlot::index`].
    hotkey_focus: Vec<FocusHandle>,
    /// Which hotkey field is currently listening for a key or mouse button, if any.
    listening: Option<HotkeySlot>,
    /// Live modifiers held while `listening` is active.
    held_modifiers: ModifierFlags,
    /// Raw-key chord builder for the current listening session (engine `CaptureKey` events).
    raw: RawCapture,
    /// Short hint shown under one hotkey field when an unsupported key was pressed.
    hotkey_hint: Option<(HotkeySlot, SharedString)>,
    /// Whether the engine currently has global hotkey dispatch suspended.
    hotkeys_suspended: bool,
    /// Bumped on every capture start/stop so stale grace timers never unsuspend a new capture.
    suspend_gen: u64,
    /// Blur, window-activation, and keystroke-interception subscriptions.
    subscriptions: Vec<Subscription>,
    /// A repaint is already scheduled for the microphone re-list.
    polling: bool,
    /// Active section in Advanced view.
    section: SettingsSection,
}

impl SettingsView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            text_focus: Vec::new(),
            hotkey_focus: Vec::new(),
            listening: None,
            held_modifiers: ModifierFlags::default(),
            raw: RawCapture::default(),
            hotkey_hint: None,
            hotkeys_suspended: false,
            suspend_gen: 0,
            subscriptions: Vec::new(),
            polling: false,
            section: SettingsSection::default(),
        }
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

    fn set_engine_hotkeys_suspended(&mut self, suspended: bool, cx: &mut Context<Self>) {
        if self.hotkeys_suspended == suspended {
            return;
        }
        self.hotkeys_suspended = suspended;
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, _| app.send(Command::SetHotkeysSuspended(suspended)));
        }
    }

    fn start_listening(&mut self, slot: HotkeySlot, held: Option<Key>, cx: &mut Context<Self>) {
        self.suspend_gen = self.suspend_gen.wrapping_add(1);
        self.listening = Some(slot);
        self.held_modifiers = ModifierFlags::default();
        self.raw = RawCapture::begin(held);
        self.hotkey_hint = None;
        self.set_engine_hotkeys_suspended(true, cx);
        cx.notify();
    }

    fn stop_listening(&mut self, with_grace: bool, cx: &mut Context<Self>) {
        if self.listening.is_none() {
            return;
        }
        self.listening = None;
        self.held_modifiers = ModifierFlags::default();
        self.hotkey_hint = None;
        self.suspend_gen = self.suspend_gen.wrapping_add(1);
        if with_grace {
            let generation = self.suspend_gen;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOTKEY_RESUME_GRACE).await;
                let _ = this.update(cx, |this, cx| {
                    if this.suspend_gen == generation && this.listening.is_none() {
                        this.set_engine_hotkeys_suspended(false, cx);
                    }
                });
            })
            .detach();
        } else {
            self.set_engine_hotkeys_suspended(false, cx);
        }
        cx.notify();
    }

    /// A raw key/button transition from the engine (`EngineEvent::CaptureKey`, sent while hotkeys
    /// are suspended). Builds the chord from true virtual-key codes.
    pub fn on_capture_key(&mut self, key: Key, down: bool, cx: &mut Context<Self>) {
        let Some(slot) = self.listening else { return };
        if !self.raw.seen {
            self.raw.seed(self.held_modifiers); // modifiers GPUI saw before the first raw event
        }
        match self.raw.push(key, down, slot.optional()) {
            Some(HotkeyFieldEvent::ModifiersChanged(mods)) => {
                self.held_modifiers = mods;
                cx.notify();
            }
            Some(event) => self.on_hotkey_event(slot, event, cx),
            None => {}
        }
    }

    fn on_hotkey_event(
        &mut self,
        slot: HotkeySlot,
        event: HotkeyFieldEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            HotkeyFieldEvent::StartListening { held } => self.start_listening(slot, held, cx),
            HotkeyFieldEvent::GpuiKey { key, mods, is_held } => {
                // Raw events own the chord once they flow; this is only the no-engine fallback.
                if self.listening != Some(slot) || self.raw.seen {
                    return;
                }
                match handle_listening_key(&key, mods, is_held, slot.optional()) {
                    Some(HotkeyFieldEvent::Cancelled) => self.stop_listening(false, cx),
                    Some(event) => {
                        // The raw event for this same press may still be in flight; give it a
                        // head start so a key is never committed twice or as the wrong key.
                        let generation = self.suspend_gen;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(GPUI_KEY_DEFER).await;
                            let _ = this.update(cx, |this, cx| {
                                if this.suspend_gen == generation && !this.raw.seen {
                                    this.on_hotkey_event(slot, event, cx);
                                }
                            });
                        })
                        .detach();
                    }
                    None => {}
                }
            }
            HotkeyFieldEvent::Captured(chord) => {
                self.edit(cx, |d| slot.set(d, chord));
                self.stop_listening(true, cx);
            }
            HotkeyFieldEvent::Cleared => {
                self.edit(cx, |d| slot.set(d, String::new()));
                self.stop_listening(true, cx);
            }
            HotkeyFieldEvent::Cancelled => {
                if self.listening == Some(slot) {
                    self.stop_listening(false, cx);
                }
            }
            HotkeyFieldEvent::InvalidKey(msg) => {
                if self.listening == Some(slot) {
                    self.hotkey_hint = Some((slot, msg.into()));
                    cx.notify();
                }
            }
            HotkeyFieldEvent::ModifiersChanged(mods) => {
                if self.listening == Some(slot) && !self.raw.seen && self.held_modifiers != mods {
                    self.held_modifiers = mods;
                    cx.notify();
                }
            }
        }
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

/// A settings section: heading + one-line description above a bordered card of rows.
fn section_block(
    title: &'static str,
    description: impl Into<SharedString>,
    rows: Vec<AnyElement>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(theme::section_heading(title))
                .child(theme::caption(description)),
        )
        .child(settings_card(rows))
        .into_any_element()
}

/// A bordered card containing rows separated by 1 px hairlines (never after the last row).
fn settings_card(rows: Vec<AnyElement>) -> gpui::Div {
    let mut children: Vec<AnyElement> = Vec::with_capacity(rows.len() * 2);
    for (idx, row) in rows.into_iter().enumerate() {
        if idx > 0 {
            children.push(div().h(px(1.)).bg(rgb(theme::BORDER_SUBTLE)).into_any_element());
        }
        children.push(row);
    }
    div()
        .flex()
        .flex_col()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .overflow_hidden()
        .children(children)
}

/// One row inside a settings card: label + helper copy on the left, control on the right.
fn setting_row(
    label: &'static str,
    helper: impl Into<SharedString>,
    control: impl IntoElement,
) -> AnyElement {
    let helper: SharedString = helper.into();
    div()
        .px_4()
        .py_3()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x_4()
        .gap_y_2()
        .child(
            div()
                .flex_1()
                .min_w(px(200.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(theme::TEXT_BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(theme::FG))
                        .child(label),
                )
                .children((!helper.is_empty()).then(|| theme::caption(helper))),
        )
        .child(div().flex_shrink_0().child(control))
        .into_any_element()
}

/// Quiet informational footnote row inside a settings card.
fn info_row(text: impl Into<SharedString>) -> AnyElement {
    div()
        .px_4()
        .py(px(10.))
        .bg(rgb(theme::SURFACE_INSET))
        .text_size(px(theme::TEXT_CAPTION))
        .line_height(px(17.))
        .text_color(rgb(theme::MUTED_FG))
        .child(text.into())
        .into_any_element()
}

/// Calm amber warning row inside a settings card.
fn warn_row(text: impl Into<SharedString>) -> AnyElement {
    div()
        .px_4()
        .py(px(10.))
        .bg(rgb(theme::WARN_SOFT))
        .text_size(px(theme::TEXT_CAPTION))
        .line_height(px(17.))
        .text_color(rgb(theme::WARN))
        .child(text.into())
        .into_any_element()
}

/// A switch row with label + helper on the left and the toggle switch on the right.
fn toggle_row(
    id: &'static str,
    label: &'static str,
    helper: impl Into<SharedString>,
    on: bool,
    enabled: bool,
    on_toggle: impl Fn(&bool, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let mut sw = toggle(id, on, enabled).aria_label(label);
    if enabled {
        sw = sw.on_click(move |_, window, cx| on_toggle(&!on, window, cx));
    }
    setting_row(label, helper, sw)
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

/// The Simple | Advanced switch. Picking a view saves it like any other setting.
fn view_switch(mode: UiMode, cx: &mut Context<SettingsView>) -> gpui::Div {
    let selected = match mode {
        UiMode::Simple => 0,
        UiMode::Advanced => 1,
    };
    segmented(
        "settings-view",
        labels(["Simple", "Advanced"]),
        Some(selected),
        true,
        cx.listener(|this: &mut SettingsView, ix: &usize, _, cx| {
            this.stop_listening(false, cx);
            let mode = if *ix == 0 { UiMode::Simple } else { UiMode::Advanced };
            if let Some(app) = this.app.upgrade() {
                app.update(cx, |app, cx| {
                    app.set_ui_mode(mode);
                    cx.notify();
                });
            }
            cx.notify();
        }),
    )
}

/// Top header bar shared by both Simple and Advanced views.
fn page_header(mode: UiMode, wide: bool, cx: &mut Context<SettingsView>) -> gpui::Div {
    let subtitle = match mode {
        UiMode::Simple => "Quick setup for buffer length, quality, and shortcuts.",
        UiMode::Advanced => "Configure capture, audio, hotkeys, storage, and app preferences.",
    };
    div()
        .px(px(if wide { 24. } else { 16. }))
        .py(px(14.))
        .bg(rgb(theme::BG))
        .border_b_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(theme::page_title("Settings"))
                .child(theme::caption(subtitle)),
        )
        .child(view_switch(mode, cx))
}

/// One quiet section-navigation button, used vertically on wide windows and horizontally on narrow ones.
fn nav_item(
    item: SettingsSection,
    active: bool,
    vertical: bool,
    cx: &mut Context<SettingsView>,
) -> gpui::Stateful<gpui::Div> {
    let label = item.label();
    let base = div()
        .id(item.id())
        .tab_stop(true)
        .role(Role::Tab)
        .aria_label(label)
        .aria_selected(active)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .h(px(theme::CONTROL_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded(px(theme::RADIUS_MD))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
        .cursor_pointer();
    let base = if vertical { base.w_full() } else { base };
    let (styled, icon_color) = if active {
        (
            base.bg(rgb(theme::SURFACE_RAISED))
                .border_color(rgb(theme::BORDER))
                .text_color(rgb(theme::FG))
                .hover(|s| s.bg(rgb(theme::ACCENT)))
                .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE))),
            theme::BRAND,
        )
    } else {
        (
            base.text_color(rgb(theme::MUTED_FG))
                .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)).text_color(rgb(theme::FG)))
                .active(|s| s.bg(rgb(theme::ACCENT))),
            theme::MUTED_FG,
        )
    };
    styled
        .child(icon(item.icon(), icon_color))
        .child(label)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.stop_listening(false, cx);
            this.section = item;
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
            if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                this.stop_listening(false, cx);
                this.section = item;
                cx.notify();
            }
        }))
}

/// Save and Revert, and every problem with the draft. Shared by both views.
fn save_bar(d: &Draft, dirty: bool, wide: bool, cx: &mut Context<SettingsView>) -> gpui::Div {
    let mut save = button_primary_if("save", "Save changes", dirty);
    let revert: AnyElement = if dirty {
        save = save.on_click(cx.listener(|this, _, _, cx| {
            this.stop_listening(false, cx);
            if let Some(app) = this.app.upgrade() {
                app.update(cx, |app, cx| {
                    app.save_settings();
                    cx.notify();
                });
            }
            cx.notify();
        }));
        button("revert", "Revert")
            .on_click(cx.listener(|this, _, _, cx| {
                this.stop_listening(false, cx);
                if let Some(app) = this.app.upgrade() {
                    app.update(cx, |app, cx| {
                        app.draft = Draft::from_settings(&app.settings);
                        cx.notify();
                    });
                }
                cx.notify();
            }))
            .into_any_element()
    } else {
        div()
            .id("revert")
            .role(Role::Button)
            .aria_label("Revert")
            .h(px(theme::CONTROL_H))
            .px_3()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::MUTED))
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .text_size(px(theme::TEXT_BODY))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(theme::TEXT_DIM))
            .cursor_default()
            .child("Revert")
            .into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .gap_2()
        .px(px(if wide { 24. } else { 16. }))
        .py_3()
        .bg(rgb(theme::BG))
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .children((!d.problems.is_empty()).then(|| {
            div()
                .px_3()
                .py_2()
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::DANGER_SOFT))
                .border_1()
                .border_color(rgb(theme::DANGER_BORDER))
                .flex()
                .flex_col()
                .gap_1()
                .children(d.problems.iter().map(|p| {
                    div()
                        .text_size(px(theme::TEXT_CAPTION))
                        .text_color(rgb(theme::DANGER))
                        .child(p.clone())
                }))
        }))
        .child(div().flex().flex_row().items_center().gap_2().child(save).child(revert).children(
            dirty.then(|| {
                div()
                    .ml_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(status_dot(rgb(theme::BRAND)))
                    .child(theme::caption("Unsaved changes"))
            }),
        ))
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div() };
        if self.hotkey_focus.is_empty() {
            self.text_focus = (0..2).map(|_| cx.focus_handle()).collect();
            self.hotkey_focus = (0..HotkeySlot::ALL.len()).map(|_| cx.focus_handle()).collect();
            for slot in HotkeySlot::ALL {
                let handle = self.hotkey_focus[slot.index()].clone();
                self.subscriptions.push(cx.on_blur(&handle, window, move |this, _, cx| {
                    if this.listening == Some(slot) {
                        this.stop_listening(false, cx);
                    }
                }));
            }
            self.subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() && this.listening.is_some() {
                    this.stop_listening(false, cx);
                }
            }));
            // While a hotkey field is listening, prevent window-level GPUI keybindings (like
            // F11 -> ToggleFullscreen) from intercepting the key before `on_key_down` sees it.
            let weak = cx.weak_entity();
            self.subscriptions.push(cx.intercept_keystrokes(move |_, _, cx| {
                if weak.upgrade().is_some_and(|view| view.read(cx).listening.is_some()) {
                    cx.stop_propagation();
                }
            }));
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
        let wide = is_two_column(f32::from(window.viewport_size().width));
        let path_w = if wide { 300. } else { 230. };
        let (video, audio, dvr) = (&d.settings.video, &d.settings.audio, &d.settings.dvr);

        // Text fields first: each holds `cx` only while it is built.
        let field = |ix: usize,
                     id: &'static str,
                     aria: &'static str,
                     value: &str,
                     ph: &str,
                     width: f32,
                     set: fn(&mut Draft, &str)| {
            text_field(
                id,
                &self.text_focus[ix],
                window,
                value,
                ph,
                width,
                cx.listener(move |this, v: &String, _, cx| this.edit(cx, |d| set(d, v))),
            )
            .tab_stop(true)
            .role(Role::TextInput)
            .aria_label(aria)
        };
        let f_out =
            field(0, "f-out", "Output folder", &d.output_dir, &default_dir, path_w, |d, v| {
                d.output_dir = v.to_owned()
            });
        let browse = button("f-out-browse", "Browse").on_click(cx.listener(|_, _, _, cx| {
            crate::app::pick_folder(cx, |this: &mut Self, dir, cx| {
                this.edit(cx, |d| d.output_dir = dir.display().to_string());
            });
        }));
        let f_out = div().flex().flex_row().items_center().gap_2().child(f_out).child(browse);
        let (dup_warnings, dup_flags) = duplicate_hotkeys(&d);
        let hotkey_view = cx.entity();
        let listening = self.listening;
        let held_modifiers = self.held_modifiers;
        let hotkey_hint = self.hotkey_hint.clone();
        let hotkey = |slot: HotkeySlot| {
            let view = hotkey_view.clone();
            let is_listening = listening == Some(slot);
            let hint =
                hotkey_hint.as_ref().and_then(|(s, msg)| (*s == slot).then_some(msg.as_ref()));
            hotkey_field(
                HotkeyFieldProps {
                    id: slot.id(),
                    action_label: slot.action_label(),
                    value: slot.get(&d),
                    optional: slot.optional(),
                    listening: is_listening,
                    held_modifiers: if is_listening {
                        held_modifiers
                    } else {
                        ModifierFlags::default()
                    },
                    duplicate: dup_flags[slot.index()],
                    hint,
                },
                &self.hotkey_focus[slot.index()],
                move |event, _, cx| {
                    view.update(cx, |this, cx| this.on_hotkey_event(slot, event, cx));
                },
            )
        };
        let f_save = hotkey(HotkeySlot::SaveClip);
        let f_rec = hotkey(HotkeySlot::ToggleRecording);
        let f_shot = hotkey(HotkeySlot::Screenshot);
        let f_mark = hotkey(HotkeySlot::Bookmark);
        let f_ptt = hotkey(HotkeySlot::PushToTalk);
        let f_ffmpeg = field(
            1,
            "f-ffmpeg",
            "ffmpeg path",
            &d.ffmpeg_path,
            "Next to ZeroClip, then PATH",
            path_w + 60.,
            |d, v| d.ffmpeg_path = v.to_owned(),
        );

        let header = page_header(saved.ui_mode, wide, cx);
        let bar = save_bar(&d, dirty, wide, cx);

        if saved.ui_mode == UiMode::Simple {
            let minutes = minutes_from_buffer_secs(d.settings.profiles.default.buffer_secs);
            let bitrate = buffer_bitrate_bps(
                d.settings.video.max_bitrate_kbps,
                d.settings.audio.bitrate_kbps,
            );
            let line = storage_line(minutes, bitrate, free_space(&app.read(cx).output_dir()));

            // The number sits inside the sentence, so the heading reads "Clip the last 20 minutes".
            let number = number_stepper(
                "simple-minutes",
                f64::from(minutes),
                stepper(MIN_MINUTES, MAX_MINUTES, 1., "", true),
                bind!(cx, |d, v: &f64| d.settings.profiles.default.buffer_secs = (*v as u32) * 60),
            );
            let storage_note: AnyElement = if line.too_small {
                div()
                    .px_3()
                    .py_2()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::DANGER_SOFT))
                    .border_1()
                    .border_color(rgb(theme::DANGER_BORDER))
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::DANGER))
                    .child(line.text)
                    .into_any_element()
            } else {
                theme::caption(line.text).into_any_element()
            };
            let intro = div()
                .p_5()
                .rounded(px(theme::RADIUS))
                .bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_3()
                        .text_size(px(18.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::FG))
                        .child("Clip the last")
                        .child(number)
                        .child("minutes"),
                )
                .child(storage_note);

            let v = &d.settings.video;
            let quality_ix = current_quality(v.fps, v.min_bitrate_kbps, v.max_bitrate_kbps);
            let quality_hint = quality_ix.map_or_else(
                || {
                    format!(
                        "Custom: {} fps, up to {} Mbps. Pick a preset to replace it, or edit in Advanced.",
                        v.fps,
                        v.max_bitrate_kbps / 1_000
                    )
                },
                |i| quality_line(&QUALITIES[i]),
            );
            let quality = section_block(
                "Quality",
                "Frame rate and bitrate preset used for clips and recordings.",
                vec![setting_row(
                    "Video preset",
                    quality_hint,
                    segmented(
                        "simple-quality",
                        QUALITIES.iter().map(|q| q.label.into()).collect(),
                        quality_ix,
                        true,
                        bind!(cx, |d, i: &usize| {
                            let q = &QUALITIES[*i];
                            d.settings.video.fps = q.fps;
                            d.settings.video.min_bitrate_kbps = q.min_kbps;
                            d.settings.video.max_bitrate_kbps = q.max_kbps;
                        }),
                    ),
                )],
            );

            let recording = section_block(
                "Storage",
                "Where saved clips, recordings, and screenshots are written.",
                vec![setting_row("Output folder", "Leave empty to use the default folder.", f_out)],
            );

            let mut simple_hotkey_rows = vec![
                setting_row("Save clip", "Save the rolling buffer to a clip.", f_save),
                setting_row("Start or stop recording", "Toggle a full-session recording.", f_rec),
                setting_row("Screenshot", "Capture a still image.", f_shot),
                setting_row("Bookmark", "Drop a timestamp marker.", f_mark),
            ];
            for warning in &dup_warnings {
                simple_hotkey_rows.push(warn_row(warning.clone()));
            }
            let hotkeys = section_block(
                "Hotkeys",
                "Click a shortcut and press the keys or mouse side button you want. Press Esc to cancel.",
                simple_hotkey_rows,
            );

            let app_sec = section_block(
                "App",
                "Startup, tray behavior, and on-screen notifications.",
                vec![
                    toggle_row(
                        "start-win",
                        "Start with Windows",
                        "Launch ZeroClip in the system tray when you sign in.",
                        d.settings.app.start_with_windows,
                        true,
                        bind!(cx, |d, v: &bool| d.settings.app.start_with_windows = *v),
                    ),
                    toggle_row(
                        "close-tray",
                        "Close to tray",
                        "Keep ZeroClip running in the tray when you close this window.",
                        d.settings.app.close_to_tray,
                        true,
                        bind!(cx, |d, v: &bool| d.settings.app.close_to_tray = *v),
                    ),
                    toggle_row(
                        "toasts",
                        "Clip toasts",
                        "Show a brief notification card when a clip or screenshot is saved.",
                        d.settings.overlay_toasts,
                        true,
                        bind!(cx, |d, v: &bool| d.settings.overlay_toasts = *v),
                    ),
                ],
            );

            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(header)
                .child(
                    div()
                        .id("settings-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px(px(if wide { 24. } else { 16. }))
                        .py_5()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(SIMPLE_MAX_W))
                                .mx_auto()
                                .flex()
                                .flex_col()
                                .gap_6()
                                .child(intro)
                                .child(quality)
                                .child(recording)
                                .child(hotkeys)
                                .child(app_sec)
                                .child(div().h(px(8.))),
                        ),
                )
                .child(bar);
        }

        // Advanced view: render only the active section's cards in the content pane.
        let content_sections: Vec<AnyElement> = match self.section {
            SettingsSection::Capture => {
                let screen_on = video.capture_mode == CaptureMode::Screen;
                let mut monitor_opts = labels(["Primary display (default)"]);
                monitor_opts.extend(d.monitors.iter().map(|m| SharedString::from(m.name.clone())));
                let monitor_sel = match video.monitor {
                    None => Some(0),
                    Some(i) => d.monitors.iter().position(|m| m.index == i).map(|p| p + 1),
                };
                let monitor_lost = video.monitor.filter(|_| monitor_sel.is_none());
                let codecs = [None, Some(Codec::Av1), Some(Codec::Hevc), Some(Codec::H264)];
                let refresh: AnyElement = if screen_on {
                    button("monitor-refresh", "Refresh")
                        .on_click(bind!(cx, |d, _e: &gpui::ClickEvent| {
                            d.monitors = zc_video::capture::list_monitors();
                        }))
                        .into_any_element()
                } else {
                    div()
                        .id("monitor-refresh")
                        .role(Role::Button)
                        .aria_label("Refresh displays")
                        .h(px(theme::CONTROL_H))
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(theme::RADIUS_MD))
                        .bg(rgb(theme::MUTED))
                        .border_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .text_size(px(theme::TEXT_BODY))
                        .text_color(rgb(theme::TEXT_DIM))
                        .cursor_default()
                        .child("Refresh")
                        .into_any_element()
                };
                let fps_sel = fps_segment(video.fps, d.fps_custom);

                let mut video_rows: Vec<AnyElement> = vec![
                    setting_row(
                        "Capture source",
                        if screen_on {
                            "Captures everything visible on the chosen display."
                        } else {
                            "Records only games with a profile. Add games on the Profiles tab."
                        },
                        segmented(
                            "capture",
                            labels(["Game only (default)", "Entire screen"]),
                            Some(usize::from(screen_on)),
                            true,
                            bind!(cx, |d, i: &usize| d.settings.video.capture_mode =
                                if *i == 0 { CaptureMode::Game } else { CaptureMode::Screen }),
                        ),
                    ),
                    setting_row(
                        "Display",
                        "Applies when recording starts. Changing it mid-recording stops that recording.",
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
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
                    ),
                ];
                if let Some(lost_ix) = monitor_lost {
                    video_rows.push(warn_row(format!("Display {} is not connected.", lost_ix + 1)));
                }
                video_rows.push(setting_row(
                    "Codec",
                    "Hardware encoder used by your GPU.",
                    segmented(
                        "codec",
                        labels(["Auto (best)", "AV1", "HEVC", "H.264"]),
                        codecs.iter().position(|c| *c == video.codec),
                        true,
                        bind!(cx, |d, i: &usize| d.settings.video.codec =
                            [None, Some(Codec::Av1), Some(Codec::Hevc), Some(Codec::H264)][*i]),
                    ),
                ));
                video_rows.push(setting_row(
                    "Frame rate",
                    "Target capture frame rate.",
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
                ));
                if fps_sel == FPS_PRESETS.len() {
                    video_rows.push(setting_row(
                        "Custom frame rate",
                        "Allowed range: 1 to 240 fps.",
                        number_stepper(
                            "fps",
                            f64::from(video.fps),
                            stepper(1., 240., 1., " fps", true),
                            bind!(cx, |d, v: &f64| d.settings.video.fps = *v as u32),
                        ),
                    ));
                }
                video_rows.extend([
                    setting_row(
                        "Minimum bitrate (VBR)",
                        "Lower bitrate floor during quiet scenes.",
                        number_stepper(
                            "minbr",
                            f64::from(video.min_bitrate_kbps),
                            stepper(500., 100_000., 500., " kbps", true),
                            bind!(cx, |d, v: &f64| d.settings.video.min_bitrate_kbps = *v as u32),
                        ),
                    ),
                    setting_row(
                        "Maximum bitrate (VBR)",
                        "Peak bitrate for fast motion and high detail.",
                        number_stepper(
                            "maxbr",
                            f64::from(video.max_bitrate_kbps),
                            stepper(500., 100_000., 500., " kbps", true),
                            bind!(cx, |d, v: &f64| d.settings.video.max_bitrate_kbps = *v as u32),
                        ),
                    ),
                    setting_row(
                        "Keyframe interval",
                        "Also sets the trim precision in the editor.",
                        number_stepper(
                            "kf",
                            f64::from(video.keyframe_interval_ms),
                            stepper(500., 4_000., 50., " ms", true),
                            bind!(cx, |d, v: &f64| d.settings.video.keyframe_interval_ms =
                                *v as u32),
                        ),
                    ),
                    toggle_row(
                        "cursor",
                        "Capture mouse cursor",
                        "Include the Windows pointer in captured footage.",
                        video.capture_cursor,
                        true,
                        bind!(cx, |d, v: &bool| d.settings.video.capture_cursor = *v),
                    ),
                ]);

                let profiles = &d.settings.profiles;
                let max_kbps = video.max_bitrate_kbps;
                let mut dvr_rows: Vec<AnyElement> = vec![
                    toggle_row(
                        "dvr-on",
                        "Rolling DVR buffer",
                        "Continuously buffer recent gameplay so you can save clips on demand.",
                        dvr.enabled,
                        true,
                        bind!(cx, |d, v: &bool| d.settings.dvr.enabled = *v),
                    ),
                    setting_row(
                        "Buffer location",
                        "SSD recommended for disk mode; uses almost no RAM.",
                        segmented(
                            "dvr-storage",
                            labels(["RAM", "Disk (SSD)"]),
                            Some(usize::from(dvr.storage == DvrStorage::Disk)),
                            true,
                            bind!(cx, |d, i: &usize| d.settings.dvr.storage =
                                if *i == 0 { DvrStorage::Ram } else { DvrStorage::Disk }),
                        ),
                    ),
                ];
                if dvr.storage == DvrStorage::Disk {
                    let dir = ring_dir_text(&d, &default_dir);
                    let on_hdd = d.ring_drive.as_ref().is_some_and(|(p, hdd)| *p == dir && *hdd);
                    let longest = profiles
                        .games
                        .iter()
                        .map(|g| g.buffer_secs)
                        .chain([profiles.default.buffer_secs])
                        .max()
                        .unwrap_or_default();
                    dvr_rows.push(info_row(format!(
                        "The ring file, sized for your longest profile ({}), takes about {} MB in {dir}. Applies when ZeroClip restarts.",
                        human_duration(zc_core::Hns::from_secs(i64::from(longest))),
                        dvr_ring_megabytes(longest, max_kbps, audio.bitrate_kbps),
                    )));
                    if on_hdd {
                        dvr_rows.push(warn_row(
                            "That folder is on a hard disk. Constant buffer writes can make games stutter; pick an SSD.",
                        ));
                    }
                } else {
                    let ceiling = effective_ceiling(dvr.ram_ceiling_mib);
                    let ceiling_helper = if ceiling.from_system_ram {
                        format!(
                            "Capped at {} MiB (5% of your system RAM), which is the limit in use.",
                            ceiling.mib
                        )
                    } else {
                        "Maximum RAM reserved for the rolling video buffer.".to_owned()
                    };
                    dvr_rows.push(setting_row(
                        "Memory limit",
                        ceiling_helper,
                        number_stepper(
                            "ram",
                            f64::from(dvr.ram_ceiling_mib),
                            stepper(64., 16_384., 64., " MiB", true),
                            bind!(cx, |d, v: &f64| d.settings.dvr.ram_ceiling_mib = *v as u32),
                        ),
                    ));
                    dvr_rows.push(info_row(format!(
                        "Buffer length is set per game on the Profiles tab. Default ({}) needs about {} MB at maximum bitrate.",
                        human_duration(profiles.default.buffer()),
                        dvr_megabytes(profiles.default.buffer_secs, max_kbps),
                    )));
                }

                vec![
                    section_block(
                        "Video capture",
                        "Choose what ZeroClip captures and how your GPU encodes it.",
                        video_rows,
                    ),
                    section_block(
                        "DVR buffer",
                        "Control how recent gameplay is kept in memory or on disk.",
                        dvr_rows,
                    ),
                ]
            }

            SettingsSection::Audio => {
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

                let mut source_rows: Vec<AnyElement> = vec![
                    toggle_row(
                        "game-audio",
                        "Game audio (track 2)",
                        "Records only the focused game on Windows 11, keeping Discord and music out. Older Windows captures all system audio.",
                        audio.game_audio,
                        true,
                        bind!(cx, |d, v: &bool| d.settings.audio.game_audio = *v),
                    ),
                    setting_row(
                        "Microphone (track 3)",
                        "Recorded on its own track so you can mute or balance it later.",
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
                    ),
                    setting_row(
                        "Microphone device",
                        "New microphones appear automatically when plugged in.",
                        segmented(
                            "mic-device",
                            mic_opts,
                            mic_sel,
                            mic_on,
                            bind!(cx, |d, i: &usize| d.settings.audio.mic_device =
                                i.checked_sub(1).and_then(|p| d.mics.get(p)).map(|m| m.id.clone())),
                        ),
                    ),
                ];
                if mic_lost {
                    source_rows.push(warn_row("The chosen microphone is not connected."));
                }
                source_rows.extend([
                    toggle_row(
                        "mic-comp",
                        "Microphone compressor",
                        "Even out quiet and loud microphone levels automatically.",
                        audio.mic_compressor,
                        mic_on,
                        bind!(cx, |d, v: &bool| d.settings.audio.mic_compressor = *v),
                    ),
                    setting_row(
                        "AAC bitrate",
                        "Audio encoding bitrate used for every recorded track.",
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
                    ),
                ]);

                let mix_rows = vec![
                    setting_row(
                        "Game level",
                        "Loudness of game audio in the Master track.",
                        number_stepper(
                            "gain-game",
                            f64::from(audio.mix_gain_db.game),
                            stepper(gain_lo, gain_hi, 1., " dB", audio.game_audio),
                            bind!(cx, |d, v: &f64| d.settings.audio.mix_gain_db.game = *v as f32),
                        ),
                    ),
                    setting_row(
                        "Microphone level",
                        "Loudness of your microphone in the Master track.",
                        number_stepper(
                            "gain-mic",
                            f64::from(audio.mix_gain_db.mic),
                            stepper(gain_lo, gain_hi, 1., " dB", mic_on),
                            bind!(cx, |d, v: &f64| d.settings.audio.mix_gain_db.mic = *v as f32),
                        ),
                    ),
                    info_row(
                        "Turning game audio or the microphone on or off, changing devices, or adjusting Master levels takes effect next time ZeroClip starts.",
                    ),
                ];

                vec![
                    section_block(
                        "Audio sources",
                        "Pick which audio tracks are recorded and how they are encoded.",
                        source_rows,
                    ),
                    section_block(
                        "Master track (track 1)",
                        "The live mix that media players and Discord play by default.",
                        mix_rows,
                    ),
                ]
            }

            SettingsSection::Hotkeys => {
                let mut rows = vec![
                    setting_row(
                        "Save clip",
                        "Write the rolling DVR buffer to a clip file.",
                        f_save,
                    ),
                    setting_row(
                        "Start or stop recording",
                        "Begin or finish a full-session recording.",
                        f_rec,
                    ),
                    setting_row(
                        "Screenshot",
                        "Capture a still image of the current frame.",
                        f_shot,
                    ),
                    setting_row(
                        "Bookmark",
                        "Drop a timestamp marker in the current footage.",
                        f_mark,
                    ),
                    setting_row(
                        "Push-to-talk",
                        "Hold to unmute your microphone in push-to-talk mode. Press Backspace while setting to clear.",
                        f_ptt,
                    ),
                ];
                for warning in &dup_warnings {
                    rows.push(warn_row(warning.clone()));
                }
                rows.push(toggle_row(
                    "toasts",
                    "Save confirmation toasts",
                    "Show a brief overlay card when a clip, recording, or screenshot is saved.",
                    d.settings.overlay_toasts,
                    true,
                    bind!(cx, |d, v: &bool| d.settings.overlay_toasts = *v),
                ));
                vec![section_block(
                    "Global hotkeys",
                    "Click a shortcut and press the keys or mouse side button you want. Press Esc to cancel.",
                    rows,
                )]
            }

            SettingsSection::Storage => {
                let st = &d.settings.storage;
                vec![
                    section_block(
                        "Output and files",
                        "Choose where recordings are saved and how folders are organized.",
                        vec![
                            setting_row(
                                "Output folder",
                                "Leave empty to use the default Videos folder.",
                                f_out,
                            ),
                            setting_row(
                                "Organize by game",
                                "Keep every file together or group into per-game subfolders.",
                                segmented(
                                    "layout",
                                    labels(["One folder", "Folder per game"]),
                                    Some(usize::from(
                                        d.settings.recording.layout == StorageLayout::PerGame,
                                    )),
                                    true,
                                    bind!(cx, |d, i: &usize| d.settings.recording.layout =
                                        if *i == 1 {
                                            StorageLayout::PerGame
                                        } else {
                                            StorageLayout::Single
                                        }),
                                ),
                            ),
                            setting_row(
                                "Container",
                                "Crash-safe fragmented MP4 so power loss never corrupts footage.",
                                // Fragmented MP4 is the only container written (D18). A settings
                                // file that still says Matroska shows no chip selected, and
                                // Settings::problems blocks saving it.
                                segmented(
                                    "container",
                                    labels(["Fragmented MP4 (.mp4)"]),
                                    (d.settings.recording.container
                                        == ContainerFormat::FragmentedMp4)
                                        .then_some(0),
                                    true,
                                    bind!(cx, |d, _i: &usize| d.settings.recording.container =
                                        ContainerFormat::FragmentedMp4),
                                ),
                            ),
                        ],
                    ),
                    section_block(
                        "Disk writes",
                        "Throttle background disk flushing on spinning hard drives.",
                        vec![
                            toggle_row(
                                "pace",
                                "Pace hard-disk writes",
                                "Write gently on hard disks so games don't stutter.",
                                st.pace_hdd_writes,
                                true,
                                bind!(cx, |d, v: &bool| d.settings.storage.pace_hdd_writes = *v),
                            ),
                            setting_row(
                                "Hard-disk write speed",
                                "Sustained write rate limit when saving to an HDD.",
                                number_stepper(
                                    "hdd",
                                    f64::from(st.hdd_write_mb_per_sec),
                                    stepper(5., 500., 5., " MB/s", true),
                                    bind!(cx, |d, v: &f64| {
                                        d.settings.storage.hdd_write_mb_per_sec = *v as u32
                                    }),
                                ),
                            ),
                            setting_row(
                                "Write chunk size",
                                "Amount of footage flushed to disk per write operation.",
                                number_stepper(
                                    "chunk",
                                    f64::from(st.write_chunk_mib),
                                    stepper(1., 64., 1., " MiB", true),
                                    bind!(cx, |d, v: &f64| {
                                        d.settings.storage.write_chunk_mib = *v as u32
                                    }),
                                ),
                            ),
                        ],
                    ),
                ]
            }

            SettingsSection::Advanced => vec![
                // Tray and startup (P4.15, P4.16)
                section_block(
                    "App behavior",
                    "Control window closing and Windows startup behavior.",
                    vec![
                        toggle_row(
                            "close-tray",
                            "Close to tray",
                            "Hide to the system tray instead of quitting. Use the tray menu to quit.",
                            d.settings.app.close_to_tray,
                            true,
                            bind!(cx, |d, v: &bool| d.settings.app.close_to_tray = *v),
                        ),
                        toggle_row(
                            "start-win",
                            "Start with Windows",
                            "Launch ZeroClip silently in the system tray when you sign in.",
                            d.settings.app.start_with_windows,
                            true,
                            bind!(cx, |d, v: &bool| d.settings.app.start_with_windows = *v),
                        ),
                    ],
                ),
                section_block(
                    "Editor",
                    "External tools used for lossless stream-copy trimming.",
                    vec![setting_row(
                        "ffmpeg location",
                        "Full path to ffmpeg.exe (ffprobe.exe beside it). Leave empty to search next to ZeroClip, then PATH.",
                        f_ffmpeg,
                    )],
                ),
                section_block(
                    "Uploads",
                    "Manage local files after sharing clips online.",
                    vec![toggle_row(
                        "del-local",
                        "Delete local file after verified upload",
                        "Off by default. Platforms re-encode or remove uploads; the local file stays your master copy.",
                        d.settings.uploads.delete_local_after_verified_upload,
                        true,
                        bind!(cx, |d, v: &bool| {
                            d.settings.uploads.delete_local_after_verified_upload = *v
                        }),
                    )],
                ),
            ],
        };

        let nav_items: Vec<AnyElement> = SettingsSection::ALL
            .into_iter()
            .map(|item| nav_item(item, self.section == item, wide, cx).into_any_element())
            .collect();

        let scroll_content = div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(if wide { 24. } else { 16. }))
            .py_5()
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_W))
                    .flex()
                    .flex_col()
                    .gap_6()
                    .children(content_sections)
                    .child(div().h(px(8.))),
            );

        let body: AnyElement = if wide {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_row()
                .child(
                    div()
                        .id("settings-nav-vertical")
                        .role(Role::TabList)
                        .aria_label("Settings sections")
                        .w(px(196.))
                        .flex_none()
                        .py_5()
                        .pl_6()
                        .pr_3()
                        .border_r_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(nav_items),
                )
                .child(scroll_content)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("settings-nav-horizontal")
                        .role(Role::TabList)
                        .aria_label("Settings sections")
                        .px_4()
                        .py_2()
                        .bg(rgb(theme::BG))
                        .border_b_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1()
                        .children(nav_items),
                )
                .child(scroll_content)
                .into_any_element()
        };

        div().size_full().flex().flex_col().child(header).child(body).child(bar)
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

    #[test]
    fn settings_collapses_to_one_column_at_760px() {
        assert!(!is_two_column(760.0));
        assert!(!is_two_column(640.0));
        assert!(is_two_column(761.0));
        assert!(is_two_column(1280.0));
    }

    #[test]
    fn duplicate_hotkeys_flags_conflicts_live_before_save() {
        let mut draft = Draft::from_settings(&Settings::default());
        let (warnings, flags) = duplicate_hotkeys(&draft);
        assert!(warnings.is_empty());
        assert_eq!(flags, [false; 5]);

        // Order-independent duplicate between Save clip (Alt+F10) and Screenshot (f10+alt).
        draft.screenshot = "f10+alt".to_owned();
        let (warnings, flags) = duplicate_hotkeys(&draft);
        assert_eq!(warnings, vec!["Save clip and Screenshot both use Alt+F10."]);
        assert_eq!(flags, [true, false, true, false, false]);

        // Duplicate involving optional push-to-talk is also flagged; clearing removes it.
        draft.screenshot = "Alt+F1".to_owned();
        draft.push_to_talk = "Alt+F9".to_owned();
        let (warnings, flags) = duplicate_hotkeys(&draft);
        assert_eq!(warnings, vec!["Start or stop recording and Push-to-talk both use Alt+F9."]);
        assert_eq!(flags, [false, true, false, false, true]);

        draft.push_to_talk.clear();
        let (warnings, flags) = duplicate_hotkeys(&draft);
        assert!(warnings.is_empty());
        assert_eq!(flags, [false; 5]);
    }
}
