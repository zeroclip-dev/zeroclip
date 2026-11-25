//! The Profiles screen (DESIGN §7): how much footage to keep, and whether to record the whole
//! session, per game. Edits go to `app.draft`; the save bar at the bottom writes them.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Bounds, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Pixels, Render, Role, ScrollHandle,
    StatefulInteractiveElement, Styled, Toggled, WeakEntity, Window, anchored, canvas, deferred,
    div, point, px, rgb, transparent_black,
};
use zc_core::Hns;
use zc_core::profile::{GameProfile, ProfileSet, clamp_window, exe_name};
use zc_core::settings::{DvrStorage, MAX_APP_TRACKS};

use super::components::button::{
    button, button_danger, button_ghost, button_primary, button_primary_if,
};
use super::components::exe_icon;
use super::components::labeled_toggle::labeled_toggle;
use super::components::open_apps::{
    has_live_track, same_exe, set_audio_track, set_game_profile, track_slot_free,
};
use super::components::slider::{SliderState, slider};
use super::components::status_dot::status_dot;
use super::components::text_field::text_field;
use super::icons::icon;
use super::settings::Draft;
use super::theme;
use super::{dvr_megabytes, dvr_ring_megabytes, effective_ceiling, human_duration};
use crate::app::ZeroClipApp;

const MIN_SECS: u32 = 10;
const MAX_SECS: u32 = 1_800;
const MIN_CUSTOM_MINUTES: u32 = 1;
const MAX_CUSTOM_MINUTES: u32 = MAX_SECS / 60;

/// Widest the page content grows, keeping cards readable on ultrawide monitors.
const PAGE_MAX_W: f32 = 920.0;
/// Width above which page horizontal padding expands from 16 px to 24 px (matches Settings).
const WIDE_MIN_W: f32 = 760.0;
/// Show the profile filter input once at least this many per-game overrides exist.
const FILTER_THRESHOLD: usize = 4;
/// How long the inline "Changes saved" confirmation remains visible in the save bar.
const SAVED_NOTICE_DURATION: Duration = Duration::from_millis(2_500);

/// Buffer lengths offered by the dropdown, in seconds.
const PRESETS: [u32; 7] = [30, 60, 120, 300, 600, 900, 1_800];

/// Geometry constants for the duration popover, used both for rendering and viewport clamping.
const MENU_W: f32 = 228.0;
const MENU_ITEM_H: f32 = theme::CONTROL_H;
const MENU_ITEM_GAP: f32 = 2.0;
const MENU_DIVIDER_H: f32 = 9.0;
const MENU_CUSTOM_H: f32 = 40.0;
const MENU_ERROR_H: f32 = 20.0;
const MENU_PAD: f32 = 10.0;
const MENU_GAP: f32 = 4.0;
const MENU_MARGIN: f32 = 12.0;
const MENU_MIN_H: f32 = 148.0;

/// Characters forbidden in Windows file names.
const INVALID_EXE_CHARS: &str = "<>:\"/\\|?*";

/// Validation outcome for a per-game executable name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExeValidation {
    /// Well-formed executable name ending in `.exe`.
    Valid,
    /// Non-empty and syntactically valid, but missing a `.exe` extension (advisory only).
    MissingExeExtension,
    /// Blank or whitespace-only (blocks saving).
    Empty,
    /// Contains path separators or characters Windows forbids in file names (blocks saving).
    InvalidChars,
    /// Repeats an earlier game row (1-based display index of the first match; blocks saving).
    Duplicate { first_row: usize },
}

/// The Profiles screen: default fallback profile, per-game overrides, and currently running apps.
pub struct ProfilesView {
    app: WeakEntity<ZeroClipApp>,
    /// Scroll handle for the main page scroll container (used to reveal newly added profiles).
    scroll: ScrollHandle,
    /// One focus handle per game row, for its executable text field.
    fields: Vec<FocusHandle>,
    /// One focus handle per game row, for its Remove button (to restore focus after Cancel).
    remove_focus: Vec<FocusHandle>,
    /// Whether each game row's exe field has been interacted with / blurred (avoids screaming red
    /// on a brand-new blank card before the user types or attempts to save).
    touched: Vec<bool>,
    /// One slider state per row, default row first.
    sliders: Vec<SliderState>,
    /// Focus handles for each row's buffer slider, default row first.
    slider_focus: Vec<FocusHandle>,
    /// Focus handles for each row's duration dropdown trigger, default row first.
    trigger_focus: Vec<FocusHandle>,
    /// Last painted window bounds of each row's duration dropdown trigger, default row first.
    triggers: Vec<Rc<Cell<Bounds<Pixels>>>>,
    /// The row whose duration dropdown is open (0 = default, 1.. = games[i - 1]).
    menu: Option<usize>,
    /// Keyboard-highlighted preset index inside the open dropdown (`0..PRESETS.len()`).
    menu_highlight: Option<usize>,
    /// Focused when the duration dropdown opens so arrows, Enter, and Esc work immediately.
    menu_focus: FocusHandle,
    /// Minutes typed into the open dropdown's custom field.
    custom: String,
    /// Inline validation error for the custom minutes input, if any.
    custom_error: Option<&'static str>,
    custom_focus: FocusHandle,
    /// Row index (`1..=games.len()`) awaiting inline delete confirmation.
    confirm_remove: Option<usize>,
    /// Focus handle for the inline Cancel button in a delete confirmation strip.
    confirm_cancel_focus: FocusHandle,
    /// Search query for filtering per-game overrides when many profiles exist.
    filter: String,
    filter_focus: FocusHandle,
    /// Set when the user clicks Save with invalid profiles, revealing errors on untouched rows.
    save_attempted: bool,
    /// Timestamp of the last successful save from this screen, for transient confirmation copy.
    saved_notice_at: Option<Instant>,
    /// Scroll to the bottom on the next frame after adding a new game profile.
    pending_scroll_to_new: bool,
    /// When each executable's icon was last looked for, so a game that isn't running isn't
    /// searched for on every frame.
    icon_tried: HashMap<String, Instant>,
}

impl ProfilesView {
    /// Creates a new Profiles screen bound to `app`.
    pub fn new(app: WeakEntity<ZeroClipApp>, cx: &mut Context<Self>) -> Self {
        Self {
            app,
            scroll: ScrollHandle::new(),
            fields: Vec::new(),
            remove_focus: Vec::new(),
            touched: Vec::new(),
            sliders: Vec::new(),
            slider_focus: Vec::new(),
            trigger_focus: Vec::new(),
            triggers: Vec::new(),
            menu: None,
            menu_highlight: None,
            menu_focus: cx.focus_handle(),
            custom: String::new(),
            custom_error: None,
            custom_focus: cx.focus_handle(),
            confirm_remove: None,
            confirm_cancel_focus: cx.focus_handle(),
            filter: String::new(),
            filter_focus: cx.focus_handle(),
            save_attempted: false,
            saved_notice_at: None,
            pending_scroll_to_new: false,
            icon_tried: HashMap::new(),
        }
    }

    /// Ensures all per-row state vectors match `games_len` (with row 0 being the default profile).
    fn sync_rows(&mut self, games_len: usize, cx: &mut Context<Self>) {
        let total = games_len + 1;
        self.fields.resize_with(games_len, || cx.focus_handle());
        self.remove_focus.resize_with(games_len, || cx.focus_handle());
        self.touched.resize(games_len, false);
        self.sliders.resize_with(total, SliderState::default);
        self.slider_focus.resize_with(total, || cx.focus_handle());
        self.trigger_focus.resize_with(total, || cx.focus_handle());
        self.triggers.resize_with(total, || Rc::new(Cell::new(Bounds::default())));
        if let Some(open_row) = self.menu
            && open_row > games_len
        {
            self.menu = None;
            self.menu_highlight = None;
        }
        if let Some(confirm_row) = self.confirm_remove
            && (confirm_row == 0 || confirm_row > games_len)
        {
            self.confirm_remove = None;
        }
    }

    /// Clears all per-row handles and cached bounds after bulk changes (Revert or Open Apps toggle).
    fn reset_row_state(&mut self) {
        self.fields.clear();
        self.remove_focus.clear();
        self.touched.clear();
        self.sliders.clear();
        self.slider_focus.clear();
        self.trigger_focus.clear();
        self.triggers.clear();
        self.menu = None;
        self.menu_highlight = None;
        self.custom.clear();
        self.custom_error = None;
        self.confirm_remove = None;
    }

    /// Runs `f` on the app (where the draft lives) and repaints it and this screen.
    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut ZeroClipApp)) {
        self.saved_notice_at = None;
        self.app
            .update(cx, |app, cx| {
                f(app);
                cx.notify();
            })
            .ok();
        cx.notify();
    }

    fn open_menu(
        &mut self,
        i: usize,
        current_secs: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_remove = None;
        if self.menu == Some(i) {
            self.close_menu(window, cx);
        } else {
            self.menu = Some(i);
            self.menu_highlight = PRESETS.iter().position(|&p| p == current_secs).or(Some(0));
            self.custom.clear();
            self.custom_error = None;
            window.focus(&self.menu_focus, cx);
            cx.notify();
        }
    }

    fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.menu.take() {
            self.menu_highlight = None;
            self.custom.clear();
            self.custom_error = None;
            if let Some(trigger) = self.trigger_focus.get(row) {
                window.focus(trigger, cx);
            } else {
                window.blur(cx);
            }
            cx.notify();
        }
    }

    fn apply_custom_minutes(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        match parse_custom_minutes(&self.custom) {
            Ok(secs) => {
                self.close_menu(window, cx);
                self.edit(cx, |app| set_secs(app, i, secs));
            }
            Err(msg) => {
                self.custom_error = Some(msg);
                window.focus(&self.custom_focus, cx);
                cx.notify();
            }
        }
    }

    fn add_game(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        self.confirm_remove = None;
        self.filter.clear();
        self.edit(cx, |app| {
            let default_secs = app.draft.settings.profiles.default.buffer_secs;
            app.draft.settings.profiles.games.push(GameProfile {
                exe: String::new(),
                buffer_secs: default_secs,
                session_recording: false,
            });
        });
        let handle = cx.focus_handle();
        window.focus(&handle, cx);
        self.fields.push(handle);
        self.remove_focus.push(cx.focus_handle());
        self.touched.push(false);
        self.sliders.push(SliderState::default());
        self.slider_focus.push(cx.focus_handle());
        self.trigger_focus.push(cx.focus_handle());
        self.triggers.push(Rc::new(Cell::new(Bounds::default())));
        self.pending_scroll_to_new = true;
        cx.notify();
    }

    fn begin_remove(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        self.confirm_remove = Some(i);
        // Focus Cancel by default so a stray Enter cannot accidentally delete a profile.
        window.focus(&self.confirm_cancel_focus, cx);
        cx.notify();
    }

    fn cancel_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.confirm_remove.take() {
            if let Some(handle) = row.checked_sub(1).and_then(|g| self.remove_focus.get(g)) {
                window.focus(handle, cx);
            }
            cx.notify();
        }
    }

    fn confirm_remove_row(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        self.confirm_remove = None;
        let Some(game_idx) = i.checked_sub(1) else { return };
        if game_idx < self.fields.len() {
            self.fields.remove(game_idx);
        }
        if game_idx < self.remove_focus.len() {
            self.remove_focus.remove(game_idx);
        }
        if game_idx < self.touched.len() {
            self.touched.remove(game_idx);
        }
        if i < self.sliders.len() {
            self.sliders.remove(i);
        }
        if i < self.slider_focus.len() {
            self.slider_focus.remove(i);
        }
        if i < self.trigger_focus.len() {
            self.trigger_focus.remove(i);
        }
        if i < self.triggers.len() {
            self.triggers.remove(i);
        }
        self.edit(cx, |app| {
            if game_idx < app.draft.settings.profiles.games.len() {
                app.draft.settings.profiles.games.remove(game_idx);
            }
        });
    }

    fn save_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        self.confirm_remove = None;
        let Some(app_entity) = self.app.upgrade() else { return };

        let profile_errors = {
            let app = app_entity.read(cx);
            blocking_profile_problems(&app.draft.settings.profiles)
        };
        if !profile_errors.is_empty() {
            self.save_attempted = true;
            for touched in &mut self.touched {
                *touched = true;
            }
            cx.notify();
            return;
        }

        let saved_ok = app_entity.update(cx, |app, cx| {
            // Trim surrounding whitespace on valid exe names before persisting.
            for game in &mut app.draft.settings.profiles.games {
                let trimmed = game.exe.trim();
                if trimmed.len() != game.exe.len() {
                    game.exe = trimmed.to_owned();
                }
            }
            app.save_settings();
            cx.notify();
            app.draft.problems.is_empty()
        });

        if saved_ok {
            self.save_attempted = false;
            let now = Instant::now();
            self.saved_notice_at = Some(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SAVED_NOTICE_DURATION).await;
                let _ = this.update(cx, |this, cx| {
                    if this.saved_notice_at == Some(now) {
                        this.saved_notice_at = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn revert_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        self.save_attempted = false;
        self.saved_notice_at = None;
        self.reset_row_state();
        self.edit(cx, |app| {
            app.draft = Draft::from_settings(&app.settings);
        });
    }
}

/// Slider position `0.0..=1.0` to seconds. Squared, so the short end (10 s to 2 min) gets as much
/// room as the long end, then snapped: 10 s steps below 2 min, 30 s below 10 min, 1 min above.
fn secs_at(fraction: f32) -> u32 {
    let raw = MIN_SECS as f32 + (MAX_SECS - MIN_SECS) as f32 * fraction.clamp(0.0, 1.0).powi(2);
    let step = if raw < 120.0 {
        10.0
    } else if raw < 600.0 {
        30.0
    } else {
        60.0
    };
    ((raw / step).round() * step) as u32
}

/// The inverse of [`secs_at`], for placing the thumb.
fn fraction_of(secs: u32) -> f32 {
    ((secs.clamp(MIN_SECS, MAX_SECS) - MIN_SECS) as f32 / (MAX_SECS - MIN_SECS) as f32).sqrt()
}

/// Moves `current` by one snapped slider step (`delta` negative = shorter, positive = longer),
/// staying within `MIN_SECS..=MAX_SECS`.
fn step_duration_secs(current: u32, delta: i32) -> u32 {
    let clamped = current.clamp(MIN_SECS, MAX_SECS);
    if delta == 0 {
        return clamped;
    }
    let step = if delta > 0 {
        if clamped < 120 {
            10
        } else if clamped < 600 {
            30
        } else {
            60
        }
    } else if clamped <= 120 {
        10
    } else if clamped <= 600 {
        30
    } else {
        60
    };
    if delta > 0 {
        let next = ((clamped / step) + 1) * step;
        next.clamp(MIN_SECS, MAX_SECS)
    } else {
        let rem = clamped % step;
        let prev = if rem == 0 { clamped.saturating_sub(step) } else { clamped - rem };
        prev.clamp(MIN_SECS, MAX_SECS)
    }
}

fn secs_label(secs: u32) -> String {
    human_duration(Hns::from_secs(i64::from(secs)))
}

/// Validates a custom duration entered in minutes (`1..=30`) and returns seconds.
fn parse_custom_minutes(raw: &str) -> Result<u32, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter 1–30 minutes.");
    }
    let minutes = trimmed.parse::<u32>().map_err(|_| "Enter a whole number (1–30).")?;
    if !(MIN_CUSTOM_MINUTES..=MAX_CUSTOM_MINUTES).contains(&minutes) {
        return Err("Enter a value between 1 and 30 min.");
    }
    Ok(minutes * 60)
}

/// Cleans pasted text for the executable name field: keeps the first line, strips any folder path
/// prefix, and trims surrounding whitespace.
fn sanitize_pasted_exe(raw: &str) -> String {
    let first_line = raw.lines().next().unwrap_or("").trim();
    let unquoted = first_line.trim_matches('"').trim();
    exe_name(unquoted).trim().to_owned()
}

/// Validates `profiles.games[game_idx].exe` against empty, illegal Windows characters, duplicate
/// entries above it, and missing `.exe` suffix.
fn validate_game_exe(profiles: &ProfileSet, game_idx: usize) -> ExeValidation {
    let Some(game) = profiles.games.get(game_idx) else {
        return ExeValidation::Valid;
    };
    let trimmed = game.exe.trim();
    if trimmed.is_empty() {
        return ExeValidation::Empty;
    }
    if trimmed.chars().any(|c| c.is_control() || INVALID_EXE_CHARS.contains(c))
        || trimmed.ends_with('.')
    {
        return ExeValidation::InvalidChars;
    }
    if let Some(first) = profiles.games[..game_idx]
        .iter()
        .position(|earlier| !earlier.exe.trim().is_empty() && same_exe(earlier.exe.trim(), trimmed))
    {
        return ExeValidation::Duplicate { first_row: first + 1 };
    }
    if !trimmed.to_ascii_lowercase().ends_with(".exe") {
        return ExeValidation::MissingExeExtension;
    }
    ExeValidation::Valid
}

/// Blocking validation errors across all per-game profiles, as sentences for the save bar.
fn blocking_profile_problems(profiles: &ProfileSet) -> Vec<String> {
    let mut problems = Vec::new();
    for (idx, game) in profiles.games.iter().enumerate() {
        let row = idx + 1;
        let name = game.exe.trim();
        match validate_game_exe(profiles, idx) {
            ExeValidation::Empty => {
                problems.push(format!("Game profile {row} needs an executable name."));
            }
            ExeValidation::InvalidChars => {
                problems.push(format!(
                    "\"{name}\" contains characters Windows doesn't allow in an executable name."
                ));
            }
            ExeValidation::Duplicate { first_row } => {
                problems.push(format!(
                    "\"{name}\" is already configured in profile {first_row}; remove or rename the duplicate."
                ));
            }
            ExeValidation::Valid | ExeValidation::MissingExeExtension => {}
        }
    }
    problems
}

/// Indices into `games` matching `query` (case-insensitive substring), preserving original order.
fn filter_game_indices(games: &[GameProfile], query: &str) -> Vec<usize> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return (0..games.len()).collect();
    }
    games
        .iter()
        .enumerate()
        .filter(|(_, g)| g.exe.to_ascii_lowercase().contains(&needle))
        .map(|(idx, _)| idx)
        .collect()
}

/// Human-readable unsaved-changes label for the save bar.
fn dirty_summary(dirty: bool, profiles_changed: bool, other_changed: bool) -> Option<&'static str> {
    if !dirty {
        return None;
    }
    match (profiles_changed, other_changed) {
        (true, true) => Some("Unsaved profile and Settings changes"),
        (true, false) => Some("Unsaved changes"),
        (false, true) => Some("Unsaved changes in Settings"),
        (false, false) => Some("Unsaved changes"),
    }
}

/// Whether the currently active game matches row `i` (0 = default fallback, 1.. = game override).
fn is_active_profile(profiles: &ProfileSet, active_game: Option<&str>, i: usize) -> bool {
    let Some(active) = active_game.map(str::trim).filter(|g| !g.is_empty()) else {
        return false;
    };
    let matched_idx = profiles.games.iter().position(|g| {
        let trimmed = g.exe.trim();
        !trimmed.is_empty() && same_exe(trimmed, active)
    });
    match matched_idx {
        Some(idx) => i == idx + 1,
        None => i == 0,
    }
}

/// Screen placement for the floating duration dropdown so it never overflows any viewport edge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DropdownPlacement {
    x: f32,
    y: f32,
    height: f32,
    presets_max_h: f32,
    open_upward: bool,
}

fn dropdown_natural_height(has_error: bool) -> f32 {
    let n = PRESETS.len() as f32;
    let error_h = if has_error { MENU_ERROR_H } else { 0.0 };
    n * MENU_ITEM_H
        + (n - 1.0).max(0.0) * MENU_ITEM_GAP
        + MENU_DIVIDER_H
        + MENU_CUSTOM_H
        + error_h
        + MENU_PAD
}

/// Chooses whether the duration dropdown opens below or above `trigger`, and caps its preset list
/// height so it always stays inside `viewport_w x viewport_h` (e.g. at 760x500).
fn dropdown_placement(
    trigger_top: f32,
    trigger_bottom: f32,
    trigger_right: f32,
    viewport_w: f32,
    viewport_h: f32,
    has_error: bool,
) -> DropdownPlacement {
    let natural = dropdown_natural_height(has_error);
    let chrome_h =
        MENU_DIVIDER_H + MENU_CUSTOM_H + if has_error { MENU_ERROR_H } else { 0.0 } + MENU_PAD;
    let max_in_viewport = (viewport_h - 2.0 * MENU_MARGIN).max(MENU_MIN_H);
    let space_below = (viewport_h - trigger_bottom - MENU_GAP - MENU_MARGIN).max(0.0);
    let space_above = (trigger_top - MENU_GAP - MENU_MARGIN).max(0.0);
    let open_upward = space_below < natural && space_above > space_below;
    let available = if open_upward { space_above } else { space_below };
    let height = available.min(natural).max(MENU_MIN_H.min(max_in_viewport)).min(max_in_viewport);
    let presets_max_h = (height - chrome_h).max(MENU_ITEM_H * 2.0);
    let y = if open_upward {
        (trigger_top - MENU_GAP - height).max(MENU_MARGIN)
    } else {
        (trigger_bottom + MENU_GAP).min((viewport_h - height - MENU_MARGIN).max(MENU_MARGIN))
    };
    let max_x = (viewport_w - MENU_W - MENU_MARGIN).max(MENU_MARGIN);
    let x = (trigger_right - MENU_W).clamp(MENU_MARGIN, max_x);
    DropdownPlacement { x, y, height, presets_max_h, open_upward }
}

fn field_caption(text: &'static str) -> gpui::Div {
    div()
        .text_size(px(theme::TEXT_CAPTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::MUTED_FG))
        .child(text)
}

/// A labelled control column inside a profile card.
fn control_block(title: &'static str) -> gpui::Div {
    div().flex().flex_col().gap_2().child(field_caption(title))
}

fn status_badge(label: &'static str, bg: u32, border: u32, fg: u32) -> gpui::Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(1.))
        .rounded_full()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .text_size(px(theme::TEXT_MICRO))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(fg))
        .child(label)
}

fn count_pill(text: impl Into<gpui::SharedString>, warn: bool) -> gpui::Div {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(2.))
        .rounded_full()
        .bg(rgb(if warn { theme::WARN_SOFT } else { theme::SURFACE_RAISED }))
        .border_1()
        .border_color(rgb(if warn { theme::WARN_BORDER } else { theme::BORDER }))
        .text_size(px(theme::TEXT_MICRO))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(if warn { theme::WARN } else { theme::MUTED_FG }))
        .child(text.into())
}

/// Attaches Enter and Space activation to a keyboard-focusable button or switch.
fn on_activate_key<E: InteractiveElement>(
    el: E,
    handler: impl Fn(&(), &mut Window, &mut gpui::App) + 'static,
) -> E {
    el.on_key_down(move |ev: &KeyDownEvent, window, cx| {
        if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
            cx.stop_propagation();
            handler(&(), window, cx);
        }
    })
}

/// A 48×32 px clickable switch hit target for table cells, matching `components::toggle`'s pill
/// visual while keeping hit-target height at `theme::CONTROL_H` (32 px) and supporting Enter/Space.
fn table_switch(
    id: impl Into<gpui::ElementId>,
    aria_label: String,
    on: bool,
    enabled: bool,
    on_toggle: impl Fn(&(), &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let knob =
        div().size(px(14.)).rounded_full().bg(rgb(if on { theme::FG } else { theme::FG_SOFT }));
    let mut track = div()
        .w(px(36.))
        .h(px(20.))
        .flex_shrink_0()
        .p(px(2.))
        .rounded_full()
        .border_1()
        .border_color(rgb(if on { theme::BRAND } else { theme::INPUT }))
        .bg(rgb(if on { theme::BRAND } else { theme::SURFACE_INSET }))
        .flex()
        .flex_row()
        .items_center()
        .child(knob);
    if on {
        track = track.justify_end();
    }

    let cell = div()
        .id(id)
        .tab_stop(enabled)
        .role(Role::Switch)
        .aria_label(aria_label)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .min_w(px(48.))
        .h(px(theme::CONTROL_H))
        .px(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_MD))
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .child(track);

    if enabled {
        let on_toggle = Rc::new(on_toggle);
        let on_click = Rc::clone(&on_toggle);
        let on_key = on_toggle;
        cell.cursor_pointer()
            .hover(|s| s.bg(rgb(theme::ACCENT)))
            .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
            .on_click(move |_, window, cx| on_click(&(), window, cx))
            .on_key_down(move |ev: &KeyDownEvent, window, cx| {
                if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    on_key(&(), window, cx);
                }
            })
    } else {
        cell.opacity(0.45).cursor_default()
    }
}

fn profile_target_name(profiles: &ProfileSet, i: usize) -> String {
    if i == 0 {
        "any other game".to_owned()
    } else {
        let exe = profiles.games[i - 1].exe.trim();
        if exe.is_empty() { format!("game profile {i}") } else { exe.to_owned() }
    }
}

/// A draggable (and keyboard-steppable) slider for the buffer length, and beside it the value as
/// a dropdown button: presets, or a custom number of minutes.
#[expect(
    clippy::too_many_arguments,
    reason = "control builder receiving per-row state handles; a wrapper struct would add boilerplate"
)]
fn duration_control(
    i: usize,
    secs: u32,
    target_name: &str,
    menu_open: bool,
    state: &SliderState,
    slider_focus: &FocusHandle,
    trigger_focus: &FocusHandle,
    trigger_bounds: &Rc<Cell<Bounds<Pixels>>>,
    cx: &mut Context<ProfilesView>,
) -> impl IntoElement {
    let view = cx.entity().downgrade();
    let bounds_cell = Rc::clone(trigger_bounds);
    let label = secs_label(secs);
    let trigger_aria = format!("Buffer duration preset for {target_name}, currently {label}");
    let slider_aria = format!("Buffer duration for {target_name}, currently {label}");

    let trigger = div()
        .id(("duration-value", i))
        .relative()
        .tab_stop(true)
        .track_focus(trigger_focus)
        .role(Role::Button)
        .aria_label(trigger_aria)
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .w(px(116.0))
        .flex_none()
        .h(px(theme::CONTROL_H))
        .px(px(10.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_1()
        .whitespace_nowrap()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(if menu_open { theme::ACCENT } else { theme::SURFACE_RAISED }))
        .border_1()
        .border_color(rgb(if menu_open { theme::BORDER_STRONG } else { theme::INPUT }))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::FG))
        .cursor_pointer()
        .hover(|h| h.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
        .active(|a| a.bg(rgb(theme::SURFACE_ACTIVE)).border_color(rgb(theme::BORDER_STRONG)))
        .child(canvas(move |b, _, _| bounds_cell.set(b), |_, (), _, _| {}).absolute().size_full())
        .child(label)
        .child(
            div()
                .text_size(px(theme::TEXT_MICRO))
                .text_color(rgb(theme::MUTED_FG))
                .child(if menu_open { "\u{25b4}" } else { "\u{25be}" }),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_menu(i, secs, window, cx);
        }))
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
            match ev.keystroke.key.as_str() {
                "enter" | "space" | "down" => {
                    cx.stop_propagation();
                    this.open_menu(i, secs, window, cx);
                }
                "escape" if this.menu == Some(i) => {
                    cx.stop_propagation();
                    this.close_menu(window, cx);
                }
                _ => {}
            }
        }));

    let slider_el = slider(format!("duration-{i}"), fraction_of(secs), state, move |f, _, cx| {
        view.update(cx, |this, cx| this.edit(cx, |app| set_secs(app, i, secs_at(f)))).ok();
    })
    .flex_1()
    .w_auto()
    .min_w(px(110.0))
    .max_w(px(280.0))
    .tab_stop(true)
    .track_focus(slider_focus)
    .role(Role::Slider)
    .aria_label(slider_aria)
    .rounded(px(theme::RADIUS_SM))
    .border_1()
    .border_color(transparent_black())
    .focus_visible(|s| s.border_color(rgb(theme::RING)))
    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
        let next = match ev.keystroke.key.as_str() {
            "left" | "down" => Some(step_duration_secs(secs, -1)),
            "right" | "up" => Some(step_duration_secs(secs, 1)),
            "home" => Some(MIN_SECS),
            "end" => Some(MAX_SECS),
            _ => None,
        };
        if let Some(target) = next {
            cx.stop_propagation();
            this.edit(cx, |app| set_secs(app, i, target));
        }
    }));

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .flex_shrink(1.)
        .min_w_0()
        .child(slider_el)
        .child(trigger)
}

/// Floating duration preset menu rendered above everything in a viewport-clamped anchored layer.
#[expect(
    clippy::too_many_arguments,
    reason = "one dropdown overlay; a params struct would only rename the arguments"
)]
fn duration_dropdown_layer(
    i: usize,
    current_secs: u32,
    highlight: Option<usize>,
    trigger: Bounds<Pixels>,
    custom: &str,
    custom_error: Option<&'static str>,
    menu_focus: &FocusHandle,
    custom_focus: &FocusHandle,
    window: &Window,
    cx: &mut Context<ProfilesView>,
) -> AnyElement {
    let view = window.viewport_size();
    let vw = f32::from(view.width);
    let vh = f32::from(view.height);
    let placement = dropdown_placement(
        f32::from(trigger.top()),
        f32::from(trigger.bottom()),
        f32::from(trigger.right()),
        vw,
        vh,
        custom_error.is_some(),
    );
    let custom_focused = custom_focus.is_focused(window);
    let has_error = custom_error.is_some();

    let presets = PRESETS.into_iter().enumerate().map(|(k, preset)| {
        let selected = preset == current_secs;
        let highlighted = highlight == Some(k) && !custom_focused;
        let label = secs_label(preset);
        let aria = format!("Set buffer duration to {label}");
        div()
            .id(("preset", i * 16 + k))
            .tab_stop(true)
            .role(Role::MenuItem)
            .aria_label(aria)
            .aria_selected(selected)
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .h(px(MENU_ITEM_H))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .rounded(px(theme::RADIUS_SM))
            .border_1()
            .border_color(transparent_black())
            .when(selected, |d| d.bg(rgb(theme::BRAND_SOFT)).border_color(rgb(theme::BRAND_BORDER)))
            .when(highlighted, |d| d.border_color(rgb(theme::RING)))
            .when(!selected && highlighted, |d| d.bg(rgb(theme::ACCENT)))
            .text_size(px(theme::TEXT_BODY))
            .font_weight(if selected { FontWeight::MEDIUM } else { FontWeight::NORMAL })
            .text_color(rgb(if selected || highlighted { theme::FG } else { theme::FG_SOFT }))
            .cursor_pointer()
            .hover(move |h| {
                if selected {
                    h.bg(rgb(theme::BRAND_SOFT)).text_color(rgb(theme::FG))
                } else {
                    h.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG))
                }
            })
            .active(|a| a.bg(rgb(theme::SURFACE_ACTIVE)))
            .child(label)
            .children(selected.then(|| icon("check", theme::BRAND)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.close_menu(window, cx);
                this.edit(cx, |app| set_secs(app, i, preset));
            }))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "enter" | "space" => {
                        cx.stop_propagation();
                        this.close_menu(window, cx);
                        this.edit(cx, |app| set_secs(app, i, preset));
                    }
                    "escape" => {
                        cx.stop_propagation();
                        this.close_menu(window, cx);
                    }
                    _ => {}
                }
            }))
    });

    let custom_handle = custom_focus.clone();
    let custom_border = if has_error {
        theme::DANGER
    } else if custom_focused {
        theme::RING
    } else {
        theme::INPUT
    };
    let custom_field = div()
        .id(("custom", i))
        .tab_stop(true)
        .role(Role::TextInput)
        .aria_label("Custom buffer duration in minutes (1 to 30)")
        .track_focus(custom_focus)
        .w(px(56.0))
        .h(px(theme::CONTROL_H))
        .px_2()
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .rounded(px(theme::RADIUS_SM))
        .bg(rgb(theme::SURFACE_INSET))
        .border_1()
        .border_color(rgb(custom_border))
        .text_size(px(theme::TEXT_BODY))
        .text_color(rgb(if custom.is_empty() { theme::MUTED_FG } else { theme::FG }))
        .cursor_text()
        .hover(move |h| {
            if custom_focused || has_error { h } else { h.border_color(rgb(theme::BORDER_STRONG)) }
        })
        .child(if custom.is_empty() { "1–30".to_owned() } else { custom.to_owned() })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| window.focus(&custom_handle, cx))
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
            let k = &ev.keystroke;
            match k.key.as_str() {
                "escape" => {
                    cx.stop_propagation();
                    this.close_menu(window, cx);
                }
                "enter" => {
                    cx.stop_propagation();
                    this.apply_custom_minutes(i, window, cx);
                }
                "backspace" => {
                    cx.stop_propagation();
                    if k.modifiers.control {
                        this.custom.clear();
                    } else {
                        this.custom.pop();
                    }
                    this.custom_error = None;
                    cx.notify();
                }
                "v" if k.modifiers.control => {
                    cx.stop_propagation();
                    if let Some(paste) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        let digits: String =
                            paste.chars().filter(|ch| ch.is_ascii_digit()).take(3).collect();
                        if !digits.is_empty() {
                            this.custom = digits;
                            this.custom_error = None;
                            cx.notify();
                        }
                    }
                }
                _ if k.modifiers.control || k.modifiers.alt || k.modifiers.platform => {}
                _ => {
                    if let Some(c) = k.key_char.as_deref()
                        && !c.is_empty()
                        && c.chars().all(|ch| ch.is_ascii_digit())
                        && this.custom.len() < 3
                    {
                        cx.stop_propagation();
                        this.custom.push_str(c);
                        this.custom_error = None;
                        cx.notify();
                    }
                }
            }
        }));

    let apply_btn = button(("custom-apply", i), "Set")
        .px(px(10.))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.apply_custom_minutes(i, window, cx);
        }))
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
            match ev.keystroke.key.as_str() {
                "enter" | "space" => {
                    cx.stop_propagation();
                    this.apply_custom_minutes(i, window, cx);
                }
                "escape" => {
                    cx.stop_propagation();
                    this.close_menu(window, cx);
                }
                _ => {}
            }
        }));

    let panel = div()
        .id("duration-dropdown-menu")
        .role(Role::Menu)
        .aria_label("Buffer duration options")
        .track_focus(menu_focus)
        .absolute()
        .left(px(placement.x))
        .top(px(placement.y))
        .w(px(MENU_W))
        .p(px(4.0))
        .flex()
        .flex_col()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::SURFACE_RAISED))
        .border_1()
        .border_color(rgb(theme::BORDER_STRONG))
        .shadow(theme::floating_shadow())
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
            if this.custom_focus.is_focused(window) {
                if ev.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close_menu(window, cx);
                }
                return;
            }
            match ev.keystroke.key.as_str() {
                "escape" => {
                    cx.stop_propagation();
                    this.close_menu(window, cx);
                }
                "down" => {
                    cx.stop_propagation();
                    let next = match this.menu_highlight {
                        Some(idx) if idx + 1 < PRESETS.len() => idx + 1,
                        _ => 0,
                    };
                    this.menu_highlight = Some(next);
                    cx.notify();
                }
                "up" => {
                    cx.stop_propagation();
                    let prev = match this.menu_highlight {
                        Some(0) | None => PRESETS.len() - 1,
                        Some(idx) => idx - 1,
                    };
                    this.menu_highlight = Some(prev);
                    cx.notify();
                }
                "home" => {
                    cx.stop_propagation();
                    this.menu_highlight = Some(0);
                    cx.notify();
                }
                "end" => {
                    cx.stop_propagation();
                    this.menu_highlight = Some(PRESETS.len() - 1);
                    cx.notify();
                }
                "enter" | "space" => {
                    cx.stop_propagation();
                    if let Some(preset) = this.menu_highlight.and_then(|k| PRESETS.get(k)).copied()
                    {
                        this.close_menu(window, cx);
                        this.edit(cx, |app| set_secs(app, i, preset));
                    }
                }
                _ => {}
            }
        }))
        .child(
            div()
                .id("duration-presets-scroll")
                .max_h(px(placement.presets_max_h))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(MENU_ITEM_GAP))
                .children(presets),
        )
        .child(div().h(px(1.)).my_1().bg(rgb(theme::BORDER_SUBTLE)))
        .child(
            div()
                .min_h(px(MENU_CUSTOM_H))
                .px(px(6.))
                .py(px(4.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(theme::TEXT_CAPTION))
                                .text_color(rgb(theme::MUTED_FG))
                                .child("Custom"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.))
                                .child(custom_field)
                                .child(
                                    div()
                                        .text_size(px(theme::TEXT_CAPTION))
                                        .text_color(rgb(theme::MUTED_FG))
                                        .child("min"),
                                )
                                .child(apply_btn),
                        ),
                )
                .children(custom_error.map(|err| {
                    div()
                        .text_size(px(theme::TEXT_MICRO))
                        .line_height(px(15.))
                        .text_color(rgb(theme::DANGER))
                        .child(err)
                })),
        );

    deferred(
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .w(view.width)
                .h(view.height)
                .relative()
                // Occlude so clicking outside closes the menu without triggering controls behind it.
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_menu(window, cx);
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_menu(window, cx);
                    }),
                )
                .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                    this.close_menu(window, cx);
                }))
                .child(panel),
        ),
    )
    .with_priority(1)
    .into_any_element()
}

/// Row 0 is the default profile; rows 1.. are `games[i - 1]`.
fn profile_mut(profiles: &mut ProfileSet, i: usize) -> &mut GameProfile {
    if i == 0 { &mut profiles.default } else { &mut profiles.games[i - 1] }
}

fn set_secs(app: &mut ZeroClipApp, i: usize, secs: u32) {
    profile_mut(&mut app.draft.settings.profiles, i).buffer_secs = secs;
}

fn memory_note(
    secs: u32,
    dvr_enabled: bool,
    storage: DvrStorage,
    max_kbps: u32,
    audio_kbps: u32,
    ceiling_mib: u32,
) -> AnyElement {
    if !dvr_enabled {
        return div()
            .text_size(px(theme::TEXT_CAPTION))
            .line_height(px(17.))
            .text_color(rgb(theme::MUTED_FG))
            .child("Rolling DVR buffer is off in Settings")
            .into_any_element();
    }
    if storage == DvrStorage::Disk {
        let mb = dvr_ring_megabytes(secs, max_kbps, audio_kbps);
        return div()
            .text_size(px(theme::TEXT_CAPTION))
            .line_height(px(17.))
            .text_color(rgb(theme::MUTED_FG))
            .child(format!("Uses about {mb} MB on disk (ring file)"))
            .into_any_element();
    }
    let mb = dvr_megabytes(secs, max_kbps);
    let ceiling_bytes = u64::from(ceiling_mib) * 1_048_576;
    let ceiling_mb = ceiling_bytes / 1_000_000;
    let clamped =
        clamp_window(Hns::from_secs(i64::from(secs)), u64::from(max_kbps) * 1_000, ceiling_bytes);
    if clamped.clamped {
        div()
            .px_2()
            .py(px(3.))
            .rounded(px(theme::RADIUS_SM))
            .bg(rgb(theme::WARN_SOFT))
            .border_1()
            .border_color(rgb(theme::WARN_BORDER))
            .text_size(px(theme::TEXT_CAPTION))
            .line_height(px(17.))
            .text_color(rgb(theme::WARN))
            .child(format!(
                "~{mb} MB exceeds the {ceiling_mb} MB RAM limit \u{2014} shortened to {}",
                human_duration(clamped.window)
            ))
            .into_any_element()
    } else {
        div()
            .text_size(px(theme::TEXT_CAPTION))
            .line_height(px(17.))
            .text_color(rgb(theme::MUTED_FG))
            .child(format!("Uses about {mb} MB of RAM"))
            .into_any_element()
    }
}

/// Quiet destructive ghost button for removing a per-game profile.
fn remove_button(i: usize, target_name: &str, focus: &FocusHandle) -> gpui::Stateful<gpui::Div> {
    div()
        .id(("remove", i))
        .tab_stop(true)
        .track_focus(focus)
        .role(Role::Button)
        .aria_label(format!("Remove profile for {target_name}"))
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .h(px(theme::CONTROL_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .rounded(px(theme::RADIUS_MD))
        .text_size(px(theme::TEXT_CAPTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::MUTED_FG))
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(theme::DANGER_SOFT))
                .border_color(rgb(theme::DANGER_BORDER))
                .text_color(rgb(theme::DANGER))
        })
        .active(|s| s.bg(rgb(theme::DANGER_SOFT)).opacity(0.82))
        .child("Remove")
}

/// Calm empty state shown when no per-game profiles exist yet.
fn empty_games_card(cx: &mut Context<ProfilesView>) -> impl IntoElement {
    let add_btn = on_activate_key(
        button("add_game_empty", "Add game")
            .on_click(cx.listener(|this, _, window, cx| this.add_game(window, cx))),
        cx.listener(|this, _, window, cx| this.add_game(window, cx)),
    );
    div()
        .p_6()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .items_center()
        .gap_3()
        .child(
            div()
                .size(px(40.))
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .flex()
                .items_center()
                .justify_center()
                .child(icon("gamepad-2", theme::MUTED_FG)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .child(theme::section_heading("No per-game overrides yet"))
                .child(
                    theme::caption(
                        "Add a game for its own buffer length or full-session recording, or toggle a running app below.",
                    )
                    .text_center(),
                ),
        )
        .child(add_btn)
}

/// Empty state shown when the profile filter hides all per-game overrides.
fn no_matching_games_card(query: &str, cx: &mut Context<ProfilesView>) -> impl IntoElement {
    let trimmed = query.trim();
    let clear_btn = on_activate_key(
        button("clear_profile_filter", "Clear filter").on_click(cx.listener(|this, _, _, cx| {
            this.filter.clear();
            cx.notify();
        })),
        cx.listener(|this, _, _, cx| {
            this.filter.clear();
            cx.notify();
        }),
    );
    div()
        .p_6()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(theme::section_heading("No matching profiles"))
        .child(
            theme::caption(format!("No per-game overrides match \u{201c}{trimmed}\u{201d}."))
                .text_center(),
        )
        .child(div().pt_1().child(clear_btn))
}

impl Render for ProfilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app_entity) = self.app.upgrade() else { return div().into_any_element() };

        // Copy what the screen needs, so the app isn't borrowed while the listeners are built.
        let (
            profiles,
            tracks,
            max_kbps,
            audio_kbps,
            dvr_enabled,
            dvr_storage,
            ceiling_mib,
            dirty,
            profiles_changed,
            other_changed,
            draft_problems,
            icons_dir,
            active_game,
        ) = {
            let app = app_entity.read(cx);
            let d = &app.draft;
            let profiles_changed = d.settings.profiles != app.settings.profiles
                || d.settings.audio.app_tracks != app.settings.audio.app_tracks;
            let mut baseline = app.settings.clone();
            baseline.profiles = d.settings.profiles.clone();
            baseline.audio.app_tracks = d.settings.audio.app_tracks.clone();
            let other_changed = d.is_dirty(&baseline);
            (
                d.settings.profiles.clone(),
                d.settings.audio.app_tracks.clone(),
                d.settings.video.max_bitrate_kbps,
                d.settings.audio.bitrate_kbps,
                d.settings.dvr.enabled,
                d.settings.dvr.storage,
                effective_ceiling(d.settings.dvr.ram_ceiling_mib).mib,
                d.is_dirty(&app.settings),
                profiles_changed,
                other_changed,
                d.problems.clone(),
                app.paths.icons_dir.clone(),
                app.status.game.clone(),
            )
        };

        self.sync_rows(profiles.games.len(), cx);

        if self.pending_scroll_to_new {
            self.pending_scroll_to_new = false;
            // Direct children of `profiles_scroll`:
            // 0 = default section, 1 = games heading, 2.. = each game card, last = Running now.
            if !profiles.games.is_empty() {
                self.scroll.scroll_to_item(profiles.games.len() + 1);
            }
        }

        let wide = f32::from(window.viewport_size().width) > WIDE_MIN_W;
        let pad_x = if wide { 24.0 } else { 16.0 };

        let profile_problems = blocking_profile_problems(&profiles);

        let visible_games = filter_game_indices(&profiles.games, &self.filter);

        let default_card = self.render_profile_card(
            0,
            &profiles,
            active_game.as_deref(),
            &icons_dir,
            dvr_enabled,
            dvr_storage,
            max_kbps,
            audio_kbps,
            ceiling_mib,
            window,
            cx,
        );

        let game_cards: Vec<AnyElement> = visible_games
            .iter()
            .map(|&game_idx| {
                self.render_profile_card(
                    game_idx + 1,
                    &profiles,
                    active_game.as_deref(),
                    &icons_dir,
                    dvr_enabled,
                    dvr_storage,
                    max_kbps,
                    audio_kbps,
                    ceiling_mib,
                    window,
                    cx,
                )
            })
            .collect();

        let default_section = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(theme::section_heading("Default profile"))
                    .child(theme::caption("Used for any game without a per-game override.")),
            )
            .child(default_card);

        let show_filter = profiles.games.len() >= FILTER_THRESHOLD || !self.filter.is_empty();
        let games_count = if self.filter.trim().is_empty() {
            profiles.games.len().to_string()
        } else {
            format!("{} of {}", game_cards.len(), profiles.games.len())
        };
        let filter_controls = show_filter.then(|| {
            let filter_input = text_field(
                "profiles-filter",
                &self.filter_focus,
                window,
                &self.filter,
                "Filter games",
                180.,
                cx.listener(|this, v: &String, _, cx| {
                    this.filter = v.clone();
                    cx.notify();
                }),
            )
            .tab_stop(true)
            .role(Role::TextInput)
            .aria_label("Filter per-game profiles")
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    cx.stop_propagation();
                    if !this.filter.is_empty() {
                        this.filter.clear();
                        cx.notify();
                    } else {
                        window.blur(cx);
                    }
                }
            }));

            let clear_btn = (!self.filter.is_empty()).then(|| {
                on_activate_key(
                    button_ghost("profiles-filter-clear", "Clear").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.filter.clear();
                            cx.notify();
                        },
                    )),
                    cx.listener(|this, _, _, cx| {
                        this.filter.clear();
                        cx.notify();
                    }),
                )
            });

            div().flex().flex_row().items_center().gap_1().child(filter_input).children(clear_btn)
        });

        let games_header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(theme::section_heading("Per-game overrides"))
                    .child(count_pill(games_count, false)),
            )
            .children(filter_controls);

        let game_rows: Vec<AnyElement> = if profiles.games.is_empty() {
            vec![empty_games_card(cx).into_any_element()]
        } else if game_cards.is_empty() {
            vec![no_matching_games_card(&self.filter, cx).into_any_element()]
        } else {
            game_cards
        };

        let apps = self.open_apps_section(&app_entity, &profiles, &tracks, wide, cx);

        let open_dropdown = self.menu.map(|open_row| {
            let current_secs = if open_row == 0 {
                profiles.default.buffer_secs
            } else {
                profiles.games[open_row - 1].buffer_secs
            };
            duration_dropdown_layer(
                open_row,
                current_secs,
                self.menu_highlight,
                self.triggers[open_row].get(),
                &self.custom,
                self.custom_error,
                &self.menu_focus,
                &self.custom_focus,
                window,
                cx,
            )
        });

        let add_btn = on_activate_key(
            button_primary("add_game", "Add game")
                .on_click(cx.listener(|this, _, window, cx| this.add_game(window, cx))),
            cx.listener(|this, _, window, cx| this.add_game(window, cx)),
        );

        let page_header = div()
            .px(px(pad_x))
            .py(px(14.))
            .bg(rgb(theme::BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                div()
                    .w_full()
                    .max_w(px(PAGE_MAX_W))
                    .mx_auto()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(theme::page_title("Game profiles"))
                            .child(theme::caption(
                                "Set buffer length and full-session recording per game.",
                            )),
                    )
                    .child(add_btn),
            );

        // Show profile validation errors in the save bar once Save is attempted, plus any
        // non-empty draft problems from Settings/validation.
        let mut bar_problems = draft_problems;
        if self.save_attempted {
            for problem in &profile_problems {
                if !bar_problems.contains(problem) {
                    bar_problems.push(problem.clone());
                }
            }
        }
        let saved_recently =
            !dirty && self.saved_notice_at.is_some_and(|t| t.elapsed() < SAVED_NOTICE_DURATION);
        let summary = dirty_summary(dirty, profiles_changed, other_changed);

        let section_row = |inner: AnyElement, pt: f32, pb: f32| {
            div()
                .w_full()
                .pt(px(pt))
                .pb(px(pb))
                .px(px(pad_x))
                .child(div().w_full().max_w(px(PAGE_MAX_W)).mx_auto().child(inner))
                .into_any_element()
        };

        let mut scroll_items: Vec<AnyElement> = Vec::with_capacity(game_rows.len() + 3);
        scroll_items.push(section_row(default_section.into_any_element(), 20.0, 12.0));
        scroll_items.push(section_row(games_header.into_any_element(), 12.0, 6.0));
        for row in game_rows {
            scroll_items.push(section_row(row, 6.0, 6.0));
        }
        scroll_items.push(section_row(apps, 18.0, 24.0));

        div()
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                let k = &ev.keystroke;
                if k.key == "escape" {
                    if this.menu.is_some() {
                        cx.stop_propagation();
                        this.close_menu(window, cx);
                    } else if this.confirm_remove.is_some() {
                        cx.stop_propagation();
                        this.cancel_remove(window, cx);
                    }
                } else if k.modifiers.control && k.key == "s" && dirty {
                    cx.stop_propagation();
                    this.save_changes(window, cx);
                }
            }))
            .child(page_header)
            .child(
                // Direct section/card children so `scroll.scroll_to_item` targets a newly added
                // profile card directly, and padding lives on the children rather than the scroll
                // container (GPUI drops bottom padding on scroll boxes).
                div()
                    .id("profiles_scroll")
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(scroll_items),
            )
            .child(save_bar(
                dirty,
                summary,
                saved_recently,
                profile_problems.len(),
                bar_problems,
                pad_x,
                cx,
            ))
            .children(open_dropdown)
            .into_any_element()
    }
}

impl ProfilesView {
    #[expect(
        clippy::too_many_arguments,
        reason = "card renderer receives snapshot data to avoid borrowing ZeroClipApp during listener wiring"
    )]
    fn render_profile_card(
        &mut self,
        i: usize,
        profiles: &ProfileSet,
        active_game: Option<&str>,
        icons_dir: &std::path::Path,
        dvr_enabled: bool,
        dvr_storage: DvrStorage,
        max_kbps: u32,
        audio_kbps: u32,
        ceiling_mib: u32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let profile = if i == 0 { &profiles.default } else { &profiles.games[i - 1] };
        let target_name = profile_target_name(profiles, i);
        let active_now = is_active_profile(profiles, active_game, i);

        let exe_icon_file =
            (i > 0).then(|| exe_icon::cached(icons_dir, &profile.exe)).flatten().filter(|file| {
                // ponytail: looks again every few seconds while the row shows, not on game
                // start. Add that when the app watches for games.
                let due = self
                    .icon_tried
                    .get(&profile.exe)
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(3));
                if !file.is_file() && due {
                    self.icon_tried.insert(profile.exe.clone(), Instant::now());
                    exe_icon::capture(icons_dir, &profile.exe);
                }
                file.is_file()
            });

        let validation =
            if i == 0 { ExeValidation::Valid } else { validate_game_exe(profiles, i - 1) };
        let row_touched = i > 0 && (self.touched[i - 1] || self.save_attempted);
        let show_blocking_error = match validation {
            ExeValidation::Empty => row_touched,
            ExeValidation::InvalidChars | ExeValidation::Duplicate { .. } => true,
            ExeValidation::Valid | ExeValidation::MissingExeExtension => false,
        };

        let (title_block, helper_line): (AnyElement, AnyElement) = if i == 0 {
            let title = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(theme::TEXT_HEADING))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::FG))
                        .child("Any other game"),
                )
                .child(status_badge(
                    "Default",
                    theme::BRAND_SOFT,
                    theme::BRAND_BORDER,
                    theme::BRAND,
                ))
                .children(active_now.then(|| {
                    status_badge("Active", theme::BRAND_SOFT, theme::BRAND_BORDER, theme::BRAND)
                }))
                .into_any_element();
            let helper =
                theme::caption("Fallback for any game without its own override").into_any_element();
            (title, helper)
        } else {
            let game_idx = i - 1;
            let handle = self.fields[game_idx].clone();
            let focused = handle.is_focused(window);
            let exe = profile.exe.clone();
            let empty = exe.is_empty();
            let aria = if empty {
                format!("Executable name for game profile {i}")
            } else {
                format!("Executable name for {exe}")
            };

            let border_color = if show_blocking_error {
                if focused { theme::DANGER } else { theme::DANGER_BORDER }
            } else if focused {
                theme::RING
            } else {
                theme::INPUT
            };

            let field = div()
                .id(("exe", i))
                .tab_stop(true)
                .role(Role::TextInput)
                .aria_label(aria)
                .track_focus(&handle)
                .focus_visible(|s| s.border_color(rgb(theme::RING)))
                .flex_1()
                .min_w(px(140.0))
                .max_w(px(340.0))
                .h(px(theme::CONTROL_H))
                .px(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::SURFACE_INSET))
                .border_1()
                .border_color(rgb(border_color))
                .text_size(px(theme::TEXT_HEADING))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_text()
                .hover(move |s| {
                    if focused {
                        s
                    } else if show_blocking_error {
                        s.border_color(rgb(theme::DANGER))
                    } else {
                        s.border_color(rgb(theme::BORDER_STRONG))
                    }
                })
                .active(|s| s.border_color(rgb(theme::RING)))
                .on_click({
                    let handle = handle.clone();
                    move |_, window, cx| window.focus(&handle, cx)
                })
                .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    match k.key.as_str() {
                        "enter" | "escape" => {
                            cx.stop_propagation();
                            if let Some(t) = this.touched.get_mut(game_idx) {
                                *t = true;
                            }
                            window.blur(cx);
                            cx.notify();
                        }
                        "backspace" => {
                            cx.stop_propagation();
                            this.confirm_remove = None;
                            if let Some(t) = this.touched.get_mut(game_idx) {
                                *t = true;
                            }
                            let clear_all = k.modifiers.control;
                            this.edit(cx, |app| {
                                let text =
                                    &mut profile_mut(&mut app.draft.settings.profiles, i).exe;
                                if clear_all {
                                    text.clear();
                                } else {
                                    text.pop();
                                }
                            });
                        }
                        "v" if k.modifiers.control => {
                            cx.stop_propagation();
                            this.confirm_remove = None;
                            if let Some(t) = this.touched.get_mut(game_idx) {
                                *t = true;
                            }
                            if let Some(paste) = cx.read_from_clipboard().and_then(|c| c.text()) {
                                let clean = sanitize_pasted_exe(&paste);
                                if !clean.is_empty() {
                                    this.edit(cx, |app| {
                                        let slot =
                                            &mut profile_mut(&mut app.draft.settings.profiles, i)
                                                .exe;
                                        if slot.trim().is_empty() {
                                            *slot = clean;
                                        } else {
                                            slot.push_str(&clean);
                                        }
                                    });
                                }
                            }
                        }
                        _ if k.modifiers.control || k.modifiers.alt || k.modifiers.platform => {}
                        _ => {
                            if let Some(c) = k
                                .key_char
                                .clone()
                                .filter(|c| !c.is_empty() && !c.chars().any(|ch| ch.is_control()))
                            {
                                cx.stop_propagation();
                                this.confirm_remove = None;
                                if let Some(t) = this.touched.get_mut(game_idx) {
                                    *t = true;
                                }
                                this.edit(cx, |app| {
                                    profile_mut(&mut app.draft.settings.profiles, i)
                                        .exe
                                        .push_str(&c);
                                });
                            }
                        }
                    }
                }))
                .child(if empty {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(rgb(theme::MUTED_FG))
                        .child("game.exe")
                        .into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme::FG))
                        .child(exe)
                        .into_any_element()
                });

            let title = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(field)
                .children(active_now.then(|| {
                    status_badge("Active", theme::BRAND_SOFT, theme::BRAND_BORDER, theme::BRAND)
                }))
                .into_any_element();

            let helper: AnyElement = match validation {
                ExeValidation::Empty if show_blocking_error => div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .line_height(px(17.))
                    .text_color(rgb(theme::DANGER))
                    .child("Enter an executable file name, such as cs2.exe.")
                    .into_any_element(),
                ExeValidation::InvalidChars => div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .line_height(px(17.))
                    .text_color(rgb(theme::DANGER))
                    .child(
                        "Remove invalid filename characters (< > : \" / \\ | ? *) or trailing dot.",
                    )
                    .into_any_element(),
                ExeValidation::Duplicate { first_row } => div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .line_height(px(17.))
                    .text_color(rgb(theme::DANGER))
                    .child(format!(
                        "Duplicate of profile {first_row} above \u{2014} rename or remove this row."
                    ))
                    .into_any_element(),
                ExeValidation::MissingExeExtension => div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .line_height(px(17.))
                    .text_color(rgb(theme::WARN))
                    .child("Windows executables usually end in .exe.")
                    .into_any_element(),
                ExeValidation::Valid | ExeValidation::Empty => {
                    theme::caption("Matched by executable file name (paste a path or file name)")
                        .into_any_element()
                }
            };

            (title, helper)
        };

        let icon_tile = div()
            .size(px(36.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::SURFACE_INSET))
            .border_1()
            .border_color(rgb(if active_now { theme::BRAND_BORDER } else { theme::BORDER_SUBTLE }))
            .child(match exe_icon_file {
                Some(file) => exe_icon::view(file).into_any_element(),
                // No icon yet: the game has not run while ZeroClip was open.
                None => icon(
                    "gamepad-2",
                    if i == 0 || active_now { theme::BRAND } else { theme::MUTED_FG },
                )
                .into_any_element(),
            });

        let action_area: Option<AnyElement> = (i > 0).then(|| {
            if self.confirm_remove == Some(i) {
                let cancel_btn = button_ghost(("cancel-remove", i), "Cancel")
                    .track_focus(&self.confirm_cancel_focus)
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_remove(window, cx)))
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if matches!(ev.keystroke.key.as_str(), "enter" | "space" | "escape") {
                            cx.stop_propagation();
                            this.cancel_remove(window, cx);
                        }
                    }));
                let confirm_btn = button_danger(("confirm-remove", i), "Remove")
                    .aria_label(format!("Confirm removing profile for {target_name}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_remove_row(i, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                        match ev.keystroke.key.as_str() {
                            "enter" | "space" => {
                                cx.stop_propagation();
                                this.confirm_remove_row(i, window, cx);
                            }
                            "escape" => {
                                cx.stop_propagation();
                                this.cancel_remove(window, cx);
                            }
                            _ => {}
                        }
                    }));
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::DANGER))
                            .child("Remove profile?"),
                    )
                    .child(cancel_btn)
                    .child(confirm_btn)
                    .into_any_element()
            } else {
                on_activate_key(
                    remove_button(i, &target_name, &self.remove_focus[i - 1]).flex_none().on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.begin_remove(i, window, cx);
                        }),
                    ),
                    cx.listener(move |this, _, window, cx| {
                        this.begin_remove(i, window, cx);
                    }),
                )
                .into_any_element()
            }
        });

        let head = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_3()
            .child(icon_tile)
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(title_block)
                    .child(helper_line),
            )
            .children(action_area);

        let buffer = control_block("Keep the last")
            .flex_1()
            .min_w(px(260.0))
            .child(duration_control(
                i,
                profile.buffer_secs,
                &target_name,
                self.menu == Some(i),
                &self.sliders[i],
                &self.slider_focus[i],
                &self.trigger_focus[i],
                &self.triggers[i],
                cx,
            ))
            .child(memory_note(
                profile.buffer_secs,
                dvr_enabled,
                dvr_storage,
                max_kbps,
                audio_kbps,
                ceiling_mib,
            ));

        let session = profile.session_recording;
        let session_switch = labeled_toggle(
            ("session", i),
            if session { "On" } else { "Off" },
            session,
            true,
            cx.listener(move |this, on: &bool, _, cx| {
                let on = *on;
                this.edit(cx, |app| {
                    profile_mut(&mut app.draft.settings.profiles, i).session_recording = on;
                });
            }),
        )
        .aria_label(format!("Record whole session for {target_name}"))
        .text_color(rgb(if session { theme::FG } else { theme::MUTED_FG }))
        .font_weight(FontWeight::MEDIUM)
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
            if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                cx.stop_propagation();
                this.edit(cx, |app| {
                    let p = profile_mut(&mut app.draft.settings.profiles, i);
                    p.session_recording = !p.session_recording;
                });
            }
        }));

        let session_block = control_block("Whole session")
            .w(px(220.0))
            .flex_none()
            .child(session_switch)
            .child(theme::caption("Automatically record the full session to disk"));

        div()
            .flex()
            .flex_col()
            .rounded(px(theme::RADIUS))
            .border_1()
            .border_color(rgb(if show_blocking_error {
                theme::DANGER_BORDER
            } else if active_now {
                theme::BRAND_BORDER
            } else {
                theme::BORDER
            }))
            .bg(rgb(theme::CARD))
            .child(div().p_4().child(head))
            .child(div().h(px(1.)).bg(rgb(theme::BORDER_SUBTLE)))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_start()
                    .gap_x_6()
                    .gap_y_4()
                    .child(buffer)
                    .child(session_block),
            )
            .into_any_element()
    }

    /// Windows running now, each with switches to make it a game profile or a recorded audio track.
    fn open_apps_section(
        &self,
        app_entity: &gpui::Entity<ZeroClipApp>,
        profiles: &ProfileSet,
        tracks: &[zc_core::settings::AppTrack],
        wide: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let listed: Vec<(String, String)> = app_entity.update(cx, |app, _| {
            app.open_apps.apps().iter().map(|a| (a.exe.clone(), a.title.clone())).collect()
        });
        let app_col_w = if wide { 164.0 } else { 148.0 };
        let used_slots = tracks.len();
        let active_tracks = tracks.iter().filter(|t| t.enabled).count();
        let slots_full = used_slots >= MAX_APP_TRACKS;

        let mut rows: Vec<AnyElement> = Vec::new();
        for (n, (exe, title)) in listed.iter().enumerate() {
            let is_game = profiles.find(exe).is_some();
            let live = has_live_track(tracks, exe);
            let free = track_slot_free(tracks, exe) || live;
            let (g_exe, a_exe) = (exe.clone(), exe.clone());

            let game_switch = table_switch(
                ("game", n),
                format!("Enable game profile for {exe}"),
                is_game,
                true,
                cx.listener(move |this, _, window, cx| {
                    this.close_menu(window, cx);
                    this.reset_row_state();
                    this.edit(cx, |app| {
                        let on = app.draft.settings.profiles.find(&g_exe).is_none();
                        set_game_profile(&mut app.draft.settings.profiles, &g_exe, on);
                    });
                }),
            );

            let audio_aria = if free {
                format!("Record separate audio track for {exe}")
            } else {
                format!(
                    "Separate audio track for {exe} unavailable: all {MAX_APP_TRACKS} slots are used"
                )
            };
            let audio_switch = table_switch(
                ("audio", n),
                audio_aria,
                live,
                free,
                cx.listener(move |this, _, _, cx| {
                    this.edit(cx, |app| {
                        let on = !has_live_track(&app.draft.settings.audio.app_tracks, &a_exe);
                        set_audio_track(&mut app.draft.settings.audio.app_tracks, &a_exe, on);
                    });
                }),
            );

            if n > 0 {
                rows.push(div().h(px(1.)).bg(rgb(theme::BORDER_SUBTLE)).into_any_element());
            }
            rows.push(
                div()
                    .id(("open-app-row", n))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .py(px(6.))
                    .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)))
                    .child(
                        div()
                            .w(px(app_col_w))
                            .flex_shrink_0()
                            .truncate()
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .child(exe.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme::TEXT_CAPTION))
                            .text_color(rgb(theme::MUTED_FG))
                            .child(if title.is_empty() {
                                "(no window title)".to_owned()
                            } else {
                                title.clone()
                            }),
                    )
                    .child(
                        div()
                            .w(px(64.0))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child(game_switch),
                    )
                    .child(
                        div()
                            .w(px(84.0))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child(audio_switch),
                    )
                    .into_any_element(),
            );
        }

        let missing: Vec<String> = tracks
            .iter()
            .filter(|t| t.enabled && !listed.iter().any(|(e, _)| same_exe(e, &t.exe)))
            .map(|t| t.exe.clone())
            .collect();

        let table_card = div()
            .flex()
            .flex_col()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .py_2()
                    .bg(rgb(theme::SURFACE_INSET))
                    .border_b_1()
                    .border_color(rgb(theme::BORDER_SUBTLE))
                    .text_size(px(theme::TEXT_MICRO))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::MUTED_FG))
                    .child(div().w(px(app_col_w)).flex_shrink_0().child("App"))
                    .child(div().flex_1().min_w_0().child("Window"))
                    .child(
                        div().w(px(64.0)).flex_shrink_0().flex().justify_center().child("Profile"),
                    )
                    .child(
                        div()
                            .w(px(84.0))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child("Audio track"),
                    ),
            )
            .children(if rows.is_empty() {
                vec![
                    div()
                        .px_4()
                        .py_4()
                        .text_size(px(theme::TEXT_CAPTION))
                        .text_color(rgb(theme::MUTED_FG))
                        .child("No open windows found. Launch a game or app and click Refresh.")
                        .into_any_element(),
                ]
            } else {
                rows
            })
            .children(slots_full.then(|| {
                div()
                    .border_t_1()
                    .border_color(rgb(theme::WARN_BORDER))
                    .bg(rgb(theme::WARN_SOFT))
                    .px_4()
                    .py_2()
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::WARN))
                    .child(format!(
                        "All {MAX_APP_TRACKS} audio track slots are allocated. Remove an unused track in Settings \u{2192} Audio to add another."
                    ))
            }))
            .children((!missing.is_empty()).then(|| {
                div()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER_SUBTLE))
                    .bg(rgb(theme::SURFACE_INSET))
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(field_caption("Saved audio tracks (not running)"))
                    .children(missing.into_iter().enumerate().map(|(n, exe)| {
                        let name = exe_name(&exe).to_owned();
                        let aria = format!("Stop recording audio track for {name}");
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(theme::TEXT_BODY))
                                    .text_color(rgb(theme::FG_SOFT))
                                    .child(name),
                            )
                            .child(table_switch(
                                ("stale", n),
                                aria,
                                true,
                                true,
                                cx.listener(move |this, _, _, cx| {
                                    this.edit(cx, |app| {
                                        set_audio_track(
                                            &mut app.draft.settings.audio.app_tracks,
                                            &exe,
                                            false,
                                        );
                                    });
                                }),
                            ))
                    }))
            }));

        let refresh_btn = on_activate_key(
            button("refresh_apps", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                this.edit(cx, |app| app.open_apps.refresh());
            })),
            cx.listener(|this, _, _, cx| {
                this.edit(cx, |app| app.open_apps.refresh());
            }),
        );

        let track_badge_text = format!("{active_tracks} / {MAX_APP_TRACKS} audio tracks");

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(theme::section_heading("Running now"))
                                    .child(count_pill(track_badge_text, slots_full)),
                            )
                            .child(theme::caption(format!(
                                "Turn on Profile for per-game settings, or Audio track to isolate an app's sound (up to {MAX_APP_TRACKS})."
                            ))),
                    )
                    .child(refresh_btn),
            )
            .child(table_card)
            .into_any_element()
    }
}

/// Save and Revert, enabled while the draft differs from what's saved, plus validation problems.
fn save_bar(
    dirty: bool,
    summary: Option<&'static str>,
    saved_recently: bool,
    profile_error_count: usize,
    problems: Vec<String>,
    pad_x: f32,
    cx: &mut Context<ProfilesView>,
) -> impl IntoElement {
    let save: AnyElement = if dirty {
        on_activate_key(
            button_primary_if("save", "Save changes", true)
                .on_click(cx.listener(|this, _, window, cx| this.save_changes(window, cx))),
            cx.listener(|this, _, window, cx| this.save_changes(window, cx)),
        )
        .into_any_element()
    } else {
        button_primary_if("save", "Save changes", false).tab_stop(false).into_any_element()
    };

    let revert: AnyElement = if dirty {
        on_activate_key(
            button("revert", "Revert")
                .on_click(cx.listener(|this, _, window, cx| this.revert_changes(window, cx))),
            cx.listener(|this, _, window, cx| this.revert_changes(window, cx)),
        )
        .into_any_element()
    } else {
        div()
            .id("revert")
            .tab_stop(false)
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

    let status_note: Option<AnyElement> =
        if dirty && profile_error_count > 0 && !problems.is_empty() {
            let label = if profile_error_count == 1 {
                "1 issue to fix before saving".to_owned()
            } else {
                format!("{profile_error_count} issues to fix before saving")
            };
            Some(
                div()
                    .ml_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(status_dot(rgb(theme::DANGER)))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .text_color(rgb(theme::DANGER))
                            .child(label),
                    )
                    .into_any_element(),
            )
        } else if let Some(text) = summary {
            Some(
                div()
                    .ml_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(status_dot(rgb(theme::BRAND)))
                    .child(theme::caption(text))
                    .into_any_element(),
            )
        } else if saved_recently {
            Some(
                div()
                    .ml_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(icon("check", theme::BRAND))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .text_color(rgb(theme::FG_SOFT))
                            .child("Changes saved"),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

    div()
        .flex()
        .flex_col()
        .gap_2()
        .px(px(pad_x))
        .py_3()
        .bg(rgb(theme::BG))
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .children((!problems.is_empty()).then(|| {
            div()
                .w_full()
                .max_w(px(PAGE_MAX_W))
                .mx_auto()
                .px_3()
                .py_2()
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::DANGER_SOFT))
                .border_1()
                .border_color(rgb(theme::DANGER_BORDER))
                .flex()
                .flex_col()
                .gap_1()
                .children(problems.into_iter().map(|p| {
                    div()
                        .text_size(px(theme::TEXT_CAPTION))
                        .line_height(px(17.))
                        .text_color(rgb(theme::DANGER))
                        .child(p)
                }))
        }))
        .child(
            div()
                .w_full()
                .max_w(px(PAGE_MAX_W))
                .mx_auto()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(save)
                .child(revert)
                .children(status_note),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(exe: &str) -> GameProfile {
        GameProfile { exe: exe.to_owned(), buffer_secs: 60, session_recording: false }
    }

    fn set_with(exes: &[&str]) -> ProfileSet {
        ProfileSet {
            default: GameProfile { exe: "*".into(), buffer_secs: 60, session_recording: false },
            games: exes.iter().map(|e| profile(e)).collect(),
        }
    }

    #[test]
    fn slider_maps_to_snapped_seconds_and_back() {
        assert_eq!(secs_at(0.0), MIN_SECS);
        assert_eq!(secs_at(1.0), MAX_SECS);
        assert_eq!(secs_at(fraction_of(60)), 60);
        assert_eq!(secs_at(fraction_of(600)), 600);
        assert_eq!(secs_at(fraction_of(300)), 300);
        // Every snapped value survives a round trip, so the thumb never jumps after a drag.
        for f in 0..=1000 {
            let secs = secs_at(f as f32 / 1000.0);
            assert_eq!(secs_at(fraction_of(secs)), secs);
        }
    }

    #[test]
    fn keyboard_duration_stepping_follows_slider_grid_and_bounds() {
        assert_eq!(step_duration_secs(MIN_SECS, -1), MIN_SECS);
        assert_eq!(step_duration_secs(10, 1), 20);
        assert_eq!(step_duration_secs(110, 1), 120);
        assert_eq!(step_duration_secs(120, 1), 150);
        assert_eq!(step_duration_secs(120, -1), 110);
        assert_eq!(step_duration_secs(570, 1), 600);
        assert_eq!(step_duration_secs(600, 1), 660);
        assert_eq!(step_duration_secs(600, -1), 570);
        assert_eq!(step_duration_secs(MAX_SECS, 1), MAX_SECS);
        // Unaligned values snap cleanly to the nearest grid point in the direction of motion.
        assert_eq!(step_duration_secs(135, -1), 120);
        assert_eq!(step_duration_secs(135, 1), 150);
    }

    #[test]
    fn custom_minutes_validation_enforces_range() {
        assert_eq!(parse_custom_minutes("5"), Ok(300));
        assert_eq!(parse_custom_minutes(" 1 "), Ok(60));
        assert_eq!(parse_custom_minutes("30"), Ok(1_800));
        assert!(parse_custom_minutes("").is_err());
        assert!(parse_custom_minutes("0").is_err());
        assert!(parse_custom_minutes("31").is_err());
        assert!(parse_custom_minutes("abc").is_err());
    }

    #[test]
    fn pasted_exe_strips_paths_quotes_and_newlines() {
        assert_eq!(sanitize_pasted_exe(r#"  "C:\Program Files\Steam\cs2.exe"  "#), "cs2.exe");
        assert_eq!(
            sanitize_pasted_exe("D:/Games/League of Legends.exe\nother"),
            "League of Legends.exe"
        );
        assert_eq!(sanitize_pasted_exe("  valorant.exe "), "valorant.exe");
    }

    #[test]
    fn exe_validation_catches_empty_illegal_duplicates_and_missing_extension() {
        let profiles = set_with(&[
            "cs2.exe",
            "League of Legends.exe",
            "CS2.EXE ",
            "   ",
            "bad/path.exe",
            "bad?.exe",
            "trailing.",
            "valorant",
        ]);
        assert_eq!(validate_game_exe(&profiles, 0), ExeValidation::Valid);
        assert_eq!(validate_game_exe(&profiles, 1), ExeValidation::Valid);
        assert_eq!(validate_game_exe(&profiles, 2), ExeValidation::Duplicate { first_row: 1 });
        assert_eq!(validate_game_exe(&profiles, 3), ExeValidation::Empty);
        assert_eq!(validate_game_exe(&profiles, 4), ExeValidation::InvalidChars);
        assert_eq!(validate_game_exe(&profiles, 5), ExeValidation::InvalidChars);
        assert_eq!(validate_game_exe(&profiles, 6), ExeValidation::InvalidChars);
        assert_eq!(validate_game_exe(&profiles, 7), ExeValidation::MissingExeExtension);

        // Missing `.exe` is advisory only; empty, illegal chars, and duplicates block saving.
        let problems = blocking_profile_problems(&profiles);
        assert_eq!(problems.len(), 5, "{problems:#?}");
    }

    #[test]
    fn filter_preserves_original_indices_case_insensitively() {
        let games = vec![profile("cs2.exe"), profile("Valorant.exe"), profile("RainbowSix.exe")];
        assert_eq!(filter_game_indices(&games, ""), vec![0, 1, 2]);
        assert_eq!(filter_game_indices(&games, "  val "), vec![1]);
        assert_eq!(filter_game_indices(&games, ".EXE"), vec![0, 1, 2]);
        assert_eq!(filter_game_indices(&games, "apex"), Vec::<usize>::new());
    }

    #[test]
    fn active_profile_matches_first_override_or_falls_back_to_default() {
        let profiles = set_with(&["cs2.exe", "RainbowSix.exe"]);
        assert!(!is_active_profile(&profiles, None, 0));
        assert!(is_active_profile(&profiles, Some("notepad.exe"), 0));
        assert!(!is_active_profile(&profiles, Some("notepad.exe"), 1));
        assert!(!is_active_profile(&profiles, Some(r"C:\Games\CS2.EXE"), 0));
        assert!(is_active_profile(&profiles, Some(r"C:\Games\CS2.EXE"), 1));
        assert!(!is_active_profile(&profiles, Some(r"C:\Games\CS2.EXE"), 2));
    }

    #[test]
    fn dirty_summary_distinguishes_profile_and_settings_changes() {
        assert_eq!(dirty_summary(false, false, false), None);
        assert_eq!(dirty_summary(true, true, false), Some("Unsaved changes"));
        assert_eq!(dirty_summary(true, false, true), Some("Unsaved changes in Settings"));
        assert_eq!(dirty_summary(true, true, true), Some("Unsaved profile and Settings changes"));
    }

    #[test]
    fn dropdown_flips_upward_and_never_overflows_760x500() {
        // Trigger near the bottom of a 760x500 window must open upward inside the viewport.
        let near_bottom = dropdown_placement(380.0, 412.0, 720.0, 760.0, 500.0, false);
        assert!(near_bottom.open_upward);
        assert!(near_bottom.y >= MENU_MARGIN);
        assert!(near_bottom.y + near_bottom.height <= 500.0 - MENU_MARGIN + 0.01);
        assert!(near_bottom.x >= MENU_MARGIN);
        assert!(near_bottom.x + MENU_W <= 760.0 - MENU_MARGIN + 0.01);

        // Trigger in the middle of 760x500 caps height so it never overflows the bottom edge,
        // including when the inline custom-duration error is visible.
        for has_error in [false, true] {
            let middle = dropdown_placement(220.0, 252.0, 500.0, 760.0, 500.0, has_error);
            assert!(middle.y >= MENU_MARGIN);
            assert!(middle.y + middle.height <= 500.0 - MENU_MARGIN + 0.01);
            assert!(middle.presets_max_h >= MENU_ITEM_H * 2.0);
        }
    }
}
