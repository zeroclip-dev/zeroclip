//! Hotkey capture button: idle it displays the bound chord as crisp, tabular key caps (for
//! example `[Ctrl] [Shift] [F9]`); click, Enter, or Space switches it to listening mode where it
//! captures the next keyboard chord or supported mouse button (`Mouse3`, `Mouse4`, `Mouse5`).
//!
//! Stateless: the caller owns the draft string, a [`FocusHandle`], and the active listening state.
//! All state transitions come back through a single callback with [`HotkeyFieldEvent`].

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, FocusHandle, FontFeatures, FontWeight, InteractiveElement, KeyDownEvent, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, NavigationDirection, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px, rgb,
};
use zc_core::hotkey::{Chord, Key};

use crate::ui::components::status_dot::status_dot;
use crate::ui::theme;

/// Short hint shown when the user presses a key that cannot be bound.
pub const UNSUPPORTED_KEY_HINT: &str = "That key can't be used";

/// Keyboard modifiers held during hotkey capture.
///
/// Kept as plain booleans so [`gpui_key_to_chord`] and [`gpui_mouse_to_chord`] stay pure and
/// unit-testable without a GPUI window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModifierFlags {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

impl ModifierFlags {
    /// Whether any modifier is currently held.
    #[must_use]
    pub const fn any(self) -> bool {
        self.ctrl || self.shift || self.alt || self.win
    }

    /// Held modifier keys in canonical `zc_core::hotkey::Chord` order.
    #[must_use]
    pub fn keys(self) -> Vec<Key> {
        let mut keys = Vec::with_capacity(4);
        if self.ctrl {
            keys.push(Key::CTRL);
        }
        if self.shift {
            keys.push(Key::SHIFT);
        }
        if self.alt {
            keys.push(Key::ALT);
        }
        if self.win {
            keys.push(Key::WIN);
        }
        keys
    }
}

impl From<Modifiers> for ModifierFlags {
    fn from(m: Modifiers) -> Self {
        Self { ctrl: m.control, shift: m.shift, alt: m.alt, win: m.platform }
    }
}

/// Events emitted by [`hotkey_field`] to its owning view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyFieldEvent {
    /// User clicked the idle control or activated it with Enter/Space (`held` is that key, whose
    /// auto-repeat the raw capture must ignore).
    StartListening { held: Option<Key> },
    /// A GPUI key-down while listening, for the toolkit fallback when no raw events arrive.
    GpuiKey { key: String, mods: ModifierFlags, is_held: bool },
    /// User pressed a valid key chord or supported mouse button (`Chord::to_string`).
    Captured(String),
    /// User pressed Backspace or Delete while listening on an optional binding.
    Cleared,
    /// User cancelled listening (Esc, click outside, or focus lost).
    Cancelled,
    /// User pressed a key or mouse button that cannot be mapped to a chord.
    InvalidKey(&'static str),
    /// Held modifiers changed while listening (for live preview caps).
    ModifiersChanged(ModifierFlags),
}

/// Display and state props for [`hotkey_field`].
pub struct HotkeyFieldProps<'a> {
    pub id: &'static str,
    /// Human action name used in accessibility labels (e.g. `"Save clip"`).
    pub action_label: &'static str,
    /// Current draft string (e.g. `"Ctrl+Shift+F9"` or `""`).
    pub value: &'a str,
    /// Whether the binding may be cleared with Backspace/Delete (e.g. push-to-talk).
    pub optional: bool,
    /// Whether this field is actively listening for input.
    pub listening: bool,
    /// Modifiers held down while `listening` is true.
    pub held_modifiers: ModifierFlags,
    /// Highlight the control with a warning border when another action shares this chord.
    pub duplicate: bool,
    /// Optional short hint shown beneath the control (e.g. unsupported key notice).
    pub hint: Option<&'a str>,
}

/// Whether `key` is a modifier-only key name that should not complete a chord on its own.
#[must_use]
pub fn is_modifier_key_name(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "control"
            | "ctrl"
            | "shift"
            | "alt"
            | "win"
            | "windows"
            | "super"
            | "meta"
            | "platform"
            | "cmd"
            | "command"
    )
}

/// Maps a GPUI key name (from `Keystroke.key`) to its Windows virtual-key [`Key`], plus whether
/// GPUI's Windows backend consumed `Shift` to produce a shifted symbol (e.g. `Shift+4` arrives as
/// `"$"` with `modifiers.shift == false`).
fn gpui_key_name_to_key(key: &str) -> Option<(Key, bool)> {
    if key.is_empty() {
        return None;
    }
    if key.len() == 1 {
        let ch = key.chars().next()?;
        if let Some(letter) = Key::letter(ch) {
            return Some((letter, false));
        }
        if ch.is_ascii_digit() {
            return Some((Key(ch as u16), false));
        }
        return match ch {
            ' ' => Some((Key(0x20), false)),
            // Unshifted US OEM punctuation.
            ';' => Some((Key(0xBA), false)),
            '=' => Some((Key(0xBB), false)),
            ',' => Some((Key(0xBC), false)),
            '-' => Some((Key(0xBD), false)),
            '.' => Some((Key(0xBE), false)),
            '/' => Some((Key(0xBF), false)),
            '`' => Some((Key(0xC0), false)),
            '[' => Some((Key(0xDB), false)),
            '\\' => Some((Key(0xDC), false)),
            ']' => Some((Key(0xDD), false)),
            '\'' => Some((Key(0xDE), false)),
            // Shifted US digits and punctuation: GPUI Windows clears `modifiers.shift` when it
            // converts these keys, so restore Shift and map back to the base VK code.
            '!' => Some((Key(b'1' as u16), true)),
            '@' => Some((Key(b'2' as u16), true)),
            '#' => Some((Key(b'3' as u16), true)),
            '$' => Some((Key(b'4' as u16), true)),
            '%' => Some((Key(b'5' as u16), true)),
            '^' => Some((Key(b'6' as u16), true)),
            '&' => Some((Key(b'7' as u16), true)),
            '*' => Some((Key(b'8' as u16), true)),
            '(' => Some((Key(b'9' as u16), true)),
            ')' => Some((Key(b'0' as u16), true)),
            '~' => Some((Key(0xC0), true)),
            '_' => Some((Key(0xBD), true)),
            '+' => Some((Key(0xBB), true)),
            '{' => Some((Key(0xDB), true)),
            '}' => Some((Key(0xDD), true)),
            '|' => Some((Key(0xDC), true)),
            ':' => Some((Key(0xBA), true)),
            '"' => Some((Key(0xDE), true)),
            '<' => Some((Key(0xBC), true)),
            '>' => Some((Key(0xBE), true)),
            '?' => Some((Key(0xBF), true)),
            _ => None,
        };
    }

    let lower = key.to_ascii_lowercase();
    if is_modifier_key_name(&lower) || lower == "escape" {
        return None;
    }

    // Function keys F1..=F24.
    if let Some(rest) = lower.strip_prefix('f')
        && !rest.is_empty()
        && rest.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = rest.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Some((Key::f(n), false));
    }

    // Numpad digits Numpad0..=Numpad9 (if the platform distinguishes them).
    if let Some(rest) = lower.strip_prefix("numpad")
        && rest.len() == 1
        && let Some(d) = rest.chars().next().and_then(|c| c.to_digit(10))
    {
        return Some((Key(0x60 + d as u16), false));
    }

    let vk = match lower.as_str() {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" | "numpadenter" => 0x0D,
        "backspace" => 0x08,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "pause" => 0x13,
        "printscreen" => 0x2C,
        "numlock" => 0x90,
        "scrolllock" => 0x91,
        "capslock" => 0x14,
        "numpadmultiply" | "numpad*" => 0x6A,
        "numpadadd" | "numpad+" => 0x6B,
        "numpadsubtract" | "numpad-" => 0x6D,
        "numpaddecimal" | "numpad." => 0x6E,
        "numpaddivide" | "numpad/" => 0x6F,
        _ => return None,
    };
    Some((Key(vk), false))
}

/// Pure mapping from a GPUI `Keystroke.key` name and held modifiers to a [`Chord`].
///
/// Returns `None` for modifier-only keys, `Escape`, unknown/unmapped keys, or combinations that
/// exceed [`Chord::MAX_KEYS`].
#[must_use]
pub fn gpui_key_to_chord(key: &str, modifiers: ModifierFlags) -> Option<Chord> {
    let (main_key, implied_shift) = gpui_key_name_to_key(key)?;
    let mut keys = Vec::with_capacity(5);
    if modifiers.ctrl {
        keys.push(Key::CTRL);
    }
    if modifiers.shift || implied_shift {
        keys.push(Key::SHIFT);
    }
    if modifiers.alt {
        keys.push(Key::ALT);
    }
    if modifiers.win {
        keys.push(Key::WIN);
    }
    keys.push(main_key);
    Chord::new(keys).ok()
}

/// Pure mapping from a bindable GPUI [`MouseButton`] and held modifiers to a [`Chord`].
///
/// Only middle mouse (`Mouse3`) and navigation side buttons (`Mouse4` / `Mouse5`) are bindable;
/// left and right clicks return `None` so they remain normal UI interactions.
#[must_use]
pub fn gpui_mouse_to_chord(button: MouseButton, modifiers: ModifierFlags) -> Option<Chord> {
    let main_key = match button {
        MouseButton::Middle => Key::MOUSE3,
        MouseButton::Navigate(NavigationDirection::Back) => Key::MOUSE4,
        MouseButton::Navigate(NavigationDirection::Forward) => Key::MOUSE5,
        MouseButton::Left | MouseButton::Right => return None,
    };
    let mut keys = modifiers.keys();
    keys.push(main_key);
    Chord::new(keys).ok()
}

/// Decides what a key-down event should do while a hotkey field is actively listening.
#[must_use]
pub fn handle_listening_key(
    key: &str,
    modifiers: ModifierFlags,
    is_held: bool,
    optional: bool,
) -> Option<HotkeyFieldEvent> {
    if is_held {
        return None;
    }
    let lower = key.to_ascii_lowercase();
    if lower == "escape" {
        return Some(HotkeyFieldEvent::Cancelled);
    }
    if matches!(lower.as_str(), "backspace" | "delete") && !modifiers.any() {
        return optional.then_some(HotkeyFieldEvent::Cleared);
    }
    if is_modifier_key_name(&lower) {
        return None;
    }
    match gpui_key_to_chord(key, modifiers) {
        Some(chord) => Some(HotkeyFieldEvent::Captured(chord.to_string())),
        None => Some(HotkeyFieldEvent::InvalidKey(UNSUPPORTED_KEY_HINT)),
    }
}

/// Builds a chord from the engine's raw key stream (`EngineEvent::CaptureKey`) while a field is
/// listening. Raw Input carries true virtual-key codes, so Numpad1 and `1` stay distinct, which
/// GPUI's layout-translated keystrokes cannot promise. Pure: no GPUI, unit-tested below.
#[derive(Clone, Copy, Debug, Default)]
pub struct RawCapture {
    /// A raw event arrived this listening session, so the stream is alive and owns the chord.
    pub seen: bool,
    mods: ModifierFlags,
    /// Key that started listening (Enter/Space): its auto-repeat must not bind itself.
    blocked: Option<Key>,
}

impl RawCapture {
    /// A fresh session; `blocked` is the key whose press started listening, if any.
    #[must_use]
    pub fn begin(blocked: Option<Key>) -> Self {
        Self { seen: false, mods: ModifierFlags::default(), blocked }
    }

    /// Takes the modifiers GPUI saw held before the first raw event arrived.
    pub fn seed(&mut self, mods: ModifierFlags) {
        self.mods = mods;
    }

    /// Feeds one raw transition. `optional` allows Backspace/Delete to clear. Returns the field
    /// event to apply: `ModifiersChanged` for the preview, or a commit/cancel.
    pub fn push(&mut self, key: Key, down: bool, optional: bool) -> Option<HotkeyFieldEvent> {
        self.seen = true;
        let key = key.normalized();
        if key.is_modifier() {
            let before = self.mods;
            match key {
                Key::CTRL => self.mods.ctrl = down,
                Key::SHIFT => self.mods.shift = down,
                Key::ALT => self.mods.alt = down,
                _ => self.mods.win = down,
            }
            return (self.mods != before).then_some(HotkeyFieldEvent::ModifiersChanged(self.mods));
        }
        if self.blocked == Some(key) {
            if !down {
                self.blocked = None;
            }
            return None;
        }
        // Left/right click stay with GPUI (they must still cancel or focus); 0 and 0xFF are fake.
        if !down || matches!(key.0, 0x00..=0x02 | 0xFF) {
            return None;
        }
        match key.0 {
            0x1B => Some(HotkeyFieldEvent::Cancelled),
            0x08 | 0x2E if !self.mods.any() => optional.then_some(HotkeyFieldEvent::Cleared),
            _ => {
                let mut keys = self.mods.keys();
                keys.push(key);
                Some(match Chord::new(keys) {
                    Ok(chord) => HotkeyFieldEvent::Captured(chord.to_string()),
                    Err(_) => HotkeyFieldEvent::InvalidKey(UNSUPPORTED_KEY_HINT),
                })
            }
        }
    }
}

/// Splits a draft hotkey string into ordered key-cap labels (e.g. `"Ctrl+Shift+F9"` ->
/// `["Ctrl", "Shift", "F9"]`). Empty input yields an empty list.
#[must_use]
pub fn chord_cap_labels(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(chord) = trimmed.parse::<Chord>() {
        return chord.keys().iter().map(Key::to_string).collect();
    }
    trimmed.split('+').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// Accessible label for a hotkey field button.
///
/// Example (idle): `"Save clip hotkey, Ctrl+Shift+F9. Press to change"`.
#[must_use]
pub fn hotkey_aria_label(
    action_label: &str,
    value: &str,
    listening: bool,
    optional: bool,
) -> String {
    if listening {
        return if optional {
            format!(
                "{action_label} hotkey, listening for keys. Press a key combination, Backspace to clear, or Escape to cancel"
            )
        } else {
            format!(
                "{action_label} hotkey, listening for keys. Press a key combination, or Escape to cancel"
            )
        };
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        format!("{action_label} hotkey, not set. Press to change")
    } else {
        let display =
            trimmed.parse::<Chord>().map_or_else(|_| trimmed.to_owned(), |c| c.to_string());
        format!("{action_label} hotkey, {display}. Press to change")
    }
}

fn tabular_features() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

fn key_cap(label: impl Into<SharedString>, active_tint: bool) -> gpui::Div {
    div()
        .h(px(22.))
        .min_w(px(22.))
        .px(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_SM))
        .border_1()
        .border_color(rgb(if active_tint { theme::BRAND_BORDER } else { theme::BORDER_STRONG }))
        .bg(rgb(if active_tint { theme::BRAND_SOFT } else { theme::SURFACE_RAISED }))
        .text_size(px(theme::TEXT_MICRO))
        .line_height(px(14.))
        .font_weight(FontWeight::MEDIUM)
        .font_features(tabular_features())
        .text_color(rgb(if active_tint { theme::BRAND } else { theme::FG }))
        .child(label.into())
}

/// Renders a reusable hotkey capture button (and optional hint below it).
pub fn hotkey_field(
    props: HotkeyFieldProps<'_>,
    focus: &FocusHandle,
    on_event: impl Fn(HotkeyFieldEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let on_event = Rc::new(on_event);
    let listening = props.listening;
    let optional = props.optional;
    let aria = hotkey_aria_label(props.action_label, props.value, listening, optional);

    let mut button = div()
        .id(props.id)
        .track_focus(focus)
        .tab_stop(true)
        .role(Role::Button)
        .aria_label(aria)
        .min_w(px(190.))
        .h(px(theme::CONTROL_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .rounded(px(theme::RADIUS_MD))
        .border_1()
        .cursor_pointer()
        .focus_visible(|s| s.border_color(rgb(theme::RING)));

    if listening {
        button = button.bg(rgb(theme::BRAND_SOFT)).border_color(rgb(theme::RING));
        let mod_keys = props.held_modifiers.keys();
        if mod_keys.is_empty() {
            button = button.gap(px(6.)).child(status_dot(rgb(theme::BRAND))).child(
                div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::BRAND))
                    .child("Press keys..."),
            );
        } else {
            button =
                button.children(mod_keys.into_iter().map(|k| key_cap(k.to_string(), true))).child(
                    div()
                        .pl(px(2.))
                        .text_size(px(theme::TEXT_CAPTION))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(theme::BRAND))
                        .child("..."),
                );
        }
    } else {
        let border = if props.duplicate { theme::WARN_BORDER } else { theme::INPUT };
        button = button
            .bg(rgb(theme::SURFACE_INSET))
            .border_color(rgb(border))
            .hover(|s| s.bg(rgb(theme::CARD)).border_color(rgb(theme::BORDER_STRONG)))
            .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)).border_color(rgb(theme::BORDER_STRONG)));
        let caps = chord_cap_labels(props.value);
        if caps.is_empty() {
            button = button.child(
                div()
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::MUTED_FG))
                    .child("Not set"),
            );
        } else {
            button = button.children(caps.into_iter().map(|label| key_cap(label, false)));
        }
    }

    if listening {
        let on_key = Rc::clone(&on_event);
        let on_mods = Rc::clone(&on_event);
        let on_mouse_in = Rc::clone(&on_event);
        let on_mouse_out = Rc::clone(&on_event);
        let focus_keep = focus.clone();

        button = button
            .on_key_down(move |ev: &KeyDownEvent, window, cx| {
                cx.stop_propagation();
                let mods = ModifierFlags::from(ev.keystroke.modifiers);
                let (key, is_held) = (ev.keystroke.key.clone(), ev.is_held);
                on_key(HotkeyFieldEvent::GpuiKey { key, mods, is_held }, window, cx);
            })
            .on_modifiers_changed(move |ev: &ModifiersChangedEvent, window, cx| {
                on_mods(
                    HotkeyFieldEvent::ModifiersChanged(ModifierFlags::from(ev.modifiers)),
                    window,
                    cx,
                );
            })
            .on_any_mouse_down(move |ev: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let mods = ModifierFlags::from(ev.modifiers);
                if let Some(chord) = gpui_mouse_to_chord(ev.button, mods) {
                    on_mouse_in(HotkeyFieldEvent::Captured(chord.to_string()), window, cx);
                } else if ev.button == MouseButton::Right {
                    on_mouse_in(HotkeyFieldEvent::InvalidKey(UNSUPPORTED_KEY_HINT), window, cx);
                } else {
                    // Left-clicking the active field keeps focus on it rather than blurring.
                    focus_keep.focus(window, cx);
                }
            })
            .on_mouse_down_out(move |ev: &MouseDownEvent, window, cx| {
                let mods = ModifierFlags::from(ev.modifiers);
                if let Some(chord) = gpui_mouse_to_chord(ev.button, mods) {
                    // Side/middle mouse buttons work even if the cursor drifted off the control.
                    cx.stop_propagation();
                    on_mouse_out(HotkeyFieldEvent::Captured(chord.to_string()), window, cx);
                } else {
                    on_mouse_out(HotkeyFieldEvent::Cancelled, window, cx);
                }
            });
    } else {
        let on_click = Rc::clone(&on_event);
        let on_key = Rc::clone(&on_event);
        let focus_click = focus.clone();
        button = button
            .on_click(move |_, window, cx| {
                focus_click.focus(window, cx);
                on_click(HotkeyFieldEvent::StartListening { held: None }, window, cx);
            })
            .on_key_down(move |ev: &KeyDownEvent, window, cx| {
                if !ev.is_held && matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    let held = Key(if ev.keystroke.key == "space" { 0x20 } else { 0x0D });
                    on_key(HotkeyFieldEvent::StartListening { held: Some(held) }, window, cx);
                }
            });
    }

    div().flex().flex_col().items_end().gap(px(4.)).child(button).children(props.hint.map(|hint| {
        div()
            .text_size(px(theme::TEXT_MICRO))
            .line_height(px(15.))
            .text_color(rgb(theme::WARN))
            .child(hint.to_owned())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifierFlags = ModifierFlags { ctrl: false, shift: false, alt: false, win: false };

    fn mods(ctrl: bool, shift: bool, alt: bool, win: bool) -> ModifierFlags {
        ModifierFlags { ctrl, shift, alt, win }
    }

    #[test]
    fn maps_letters_digits_and_function_keys() {
        assert_eq!(gpui_key_to_chord("a", NONE).unwrap().to_string(), "A");
        assert_eq!(
            gpui_key_to_chord("Z", mods(true, true, false, false)).unwrap().to_string(),
            "Ctrl+Shift+Z"
        );
        assert_eq!(gpui_key_to_chord("0", NONE).unwrap().to_string(), "0");
        assert_eq!(
            gpui_key_to_chord("9", mods(false, false, true, false)).unwrap().to_string(),
            "Alt+9"
        );
        for n in 1..=24u8 {
            let name = format!("f{n}");
            assert_eq!(
                gpui_key_to_chord(&name, mods(true, false, false, false)).unwrap().to_string(),
                format!("Ctrl+F{n}"),
            );
        }
        assert_eq!(gpui_key_to_chord("f0", NONE), None);
        assert_eq!(gpui_key_to_chord("f25", NONE), None);
        assert_eq!(gpui_key_to_chord("f+1", NONE), None);
    }

    #[test]
    fn maps_navigation_editing_whitespace_and_arrows() {
        let cases = [
            ("left", "Left"),
            ("right", "Right"),
            ("up", "Up"),
            ("down", "Down"),
            ("space", "Space"),
            (" ", "Space"),
            ("tab", "Tab"),
            ("enter", "Enter"),
            ("return", "Enter"),
            ("home", "Home"),
            ("end", "End"),
            ("pageup", "PageUp"),
            ("pagedown", "PageDown"),
            ("insert", "Insert"),
            ("delete", "Delete"),
            ("backspace", "Backspace"),
            ("pause", "Pause"),
            ("printscreen", "PrintScreen"),
            ("numlock", "NumLock"),
            ("scrolllock", "ScrollLock"),
            ("capslock", "CapsLock"),
        ];
        for (gpui_name, expected) in cases {
            assert_eq!(
                gpui_key_to_chord(gpui_name, NONE).unwrap().to_string(),
                expected,
                "{gpui_name}"
            );
        }
    }

    #[test]
    fn maps_numpad_keys_when_distinguished() {
        for d in 0..=9u8 {
            assert_eq!(
                gpui_key_to_chord(&format!("numpad{d}"), NONE).unwrap().to_string(),
                format!("Numpad{d}"),
            );
        }
        let ops = [
            ("numpadmultiply", "NumpadMultiply"),
            ("numpad*", "NumpadMultiply"),
            ("numpadadd", "NumpadAdd"),
            ("numpad+", "NumpadAdd"),
            ("numpadsubtract", "NumpadSubtract"),
            ("numpad-", "NumpadSubtract"),
            ("numpaddecimal", "NumpadDecimal"),
            ("numpad.", "NumpadDecimal"),
            ("numpaddivide", "NumpadDivide"),
            ("numpad/", "NumpadDivide"),
            ("numpadenter", "Enter"),
        ];
        for (gpui_name, expected) in ops {
            assert_eq!(
                gpui_key_to_chord(gpui_name, NONE).unwrap().to_string(),
                expected,
                "{gpui_name}"
            );
        }
    }

    #[test]
    fn maps_unshifted_and_shifted_punctuation_and_digits() {
        let unshifted = [
            (";", "Semicolon"),
            ("=", "Equals"),
            (",", "Comma"),
            ("-", "Minus"),
            (".", "Period"),
            ("/", "Slash"),
            ("`", "Backtick"),
            ("[", "LeftBracket"),
            ("\\", "Backslash"),
            ("]", "RightBracket"),
            ("'", "Quote"),
        ];
        for (sym, expected) in unshifted {
            assert_eq!(gpui_key_to_chord(sym, NONE).unwrap().to_string(), expected, "{sym}");
        }

        // GPUI Windows converts Shift+digit/punctuation to the shifted symbol and clears `shift`.
        let shifted = [
            ("!", "Ctrl+Shift+1"),
            ("@", "Ctrl+Shift+2"),
            ("#", "Ctrl+Shift+3"),
            ("$", "Ctrl+Shift+4"),
            ("%", "Ctrl+Shift+5"),
            ("^", "Ctrl+Shift+6"),
            ("&", "Ctrl+Shift+7"),
            ("*", "Ctrl+Shift+8"),
            ("(", "Ctrl+Shift+9"),
            (")", "Ctrl+Shift+0"),
            ("~", "Ctrl+Shift+Backtick"),
            ("_", "Ctrl+Shift+Minus"),
            ("+", "Ctrl+Shift+Equals"),
            ("{", "Ctrl+Shift+LeftBracket"),
            ("}", "Ctrl+Shift+RightBracket"),
            ("|", "Ctrl+Shift+Backslash"),
            (":", "Ctrl+Shift+Semicolon"),
            ("\"", "Ctrl+Shift+Quote"),
            ("<", "Ctrl+Shift+Comma"),
            (">", "Ctrl+Shift+Period"),
            ("?", "Ctrl+Shift+Slash"),
        ];
        for (sym, expected) in shifted {
            assert_eq!(
                gpui_key_to_chord(sym, mods(true, false, false, false)).unwrap().to_string(),
                expected,
                "{sym}"
            );
        }
    }

    #[test]
    fn rejects_unknown_modifier_only_escape_and_overlong_chords() {
        for bad in [
            "",
            "escape",
            "control",
            "ctrl",
            "shift",
            "alt",
            "win",
            "super",
            "menu",
            "back",
            "forward",
            "é",
            "unknown_key",
        ] {
            assert_eq!(gpui_key_to_chord(bad, NONE), None, "{bad:?}");
        }
        // 4 modifiers + 1 main key = 5 keys > Chord::MAX_KEYS (4).
        assert_eq!(gpui_key_to_chord("f9", mods(true, true, true, true)), None);
        // 3 modifiers + 1 main key = 4 keys (allowed).
        assert_eq!(
            gpui_key_to_chord("f9", mods(true, true, true, false)).unwrap().to_string(),
            "Ctrl+Shift+Alt+F9"
        );
    }

    #[test]
    fn maps_supported_mouse_buttons_only() {
        assert_eq!(gpui_mouse_to_chord(MouseButton::Left, NONE), None);
        assert_eq!(gpui_mouse_to_chord(MouseButton::Right, NONE), None);
        assert_eq!(gpui_mouse_to_chord(MouseButton::Middle, NONE).unwrap().to_string(), "Mouse3");
        assert_eq!(
            gpui_mouse_to_chord(MouseButton::Navigate(NavigationDirection::Back), NONE)
                .unwrap()
                .to_string(),
            "Mouse4"
        );
        assert_eq!(
            gpui_mouse_to_chord(
                MouseButton::Navigate(NavigationDirection::Forward),
                mods(false, true, false, false),
            )
            .unwrap()
            .to_string(),
            "Shift+Mouse5"
        );
    }

    #[test]
    fn listening_key_handles_cancel_clear_modifiers_and_invalid_keys() {
        assert_eq!(
            handle_listening_key("escape", NONE, false, false),
            Some(HotkeyFieldEvent::Cancelled)
        );
        assert_eq!(
            handle_listening_key("escape", mods(true, false, false, false), false, true),
            Some(HotkeyFieldEvent::Cancelled)
        );

        // Optional bindings clear on bare Backspace/Delete; required bindings ignore them.
        assert_eq!(
            handle_listening_key("backspace", NONE, false, true),
            Some(HotkeyFieldEvent::Cleared)
        );
        assert_eq!(
            handle_listening_key("delete", NONE, false, true),
            Some(HotkeyFieldEvent::Cleared)
        );
        assert_eq!(handle_listening_key("backspace", NONE, false, false), None);
        assert_eq!(handle_listening_key("delete", NONE, false, false), None);

        // Modified Delete/Backspace still binds a chord.
        assert_eq!(
            handle_listening_key("delete", mods(true, false, false, false), false, false),
            Some(HotkeyFieldEvent::Captured("Ctrl+Delete".to_owned()))
        );

        // Auto-repeat and modifier-only keys are ignored.
        assert_eq!(handle_listening_key("f9", NONE, true, false), None);
        assert_eq!(
            handle_listening_key("control", mods(true, false, false, false), false, false),
            None
        );

        // Valid chord commits, unknown key produces the hint.
        assert_eq!(
            handle_listening_key("f9", mods(true, true, false, false), false, false),
            Some(HotkeyFieldEvent::Captured("Ctrl+Shift+F9".to_owned()))
        );
        assert_eq!(
            handle_listening_key("menu", NONE, false, false),
            Some(HotkeyFieldEvent::InvalidKey(UNSUPPORTED_KEY_HINT))
        );
    }

    #[test]
    fn formats_key_caps_and_aria_labels() {
        assert_eq!(chord_cap_labels("Ctrl+Shift+F9"), vec!["Ctrl", "Shift", "F9"]);
        assert_eq!(chord_cap_labels("f10 + alt"), vec!["Alt", "F10"]);
        assert!(chord_cap_labels("   ").is_empty());
        assert_eq!(
            hotkey_aria_label("Save clip", "Ctrl+Shift+F9", false, false),
            "Save clip hotkey, Ctrl+Shift+F9. Press to change"
        );
        assert_eq!(
            hotkey_aria_label("Push-to-talk", "", false, true),
            "Push-to-talk hotkey, not set. Press to change"
        );
        assert!(hotkey_aria_label("Save clip", "Alt+F10", true, false).contains("listening"));
        assert!(hotkey_aria_label("Push-to-talk", "", true, true).contains("Backspace to clear"));
    }

    fn raw(seq: &[(Key, bool)], optional: bool) -> Vec<HotkeyFieldEvent> {
        let mut cap = RawCapture::begin(None);
        seq.iter().filter_map(|&(k, d)| cap.push(k, d, optional)).collect()
    }

    fn captured(s: &str) -> HotkeyFieldEvent {
        HotkeyFieldEvent::Captured(s.to_owned())
    }

    #[test]
    fn raw_capture_builds_chords_from_true_vk_codes() {
        let ctrl = ModifierFlags { ctrl: true, ..NONE };
        let shift = ModifierFlags { shift: true, ..NONE };
        assert_eq!(
            raw(&[(Key::CTRL, true), (Key(0x61), true)], false),
            [HotkeyFieldEvent::ModifiersChanged(ctrl), captured("Ctrl+Numpad1")]
        );
        assert_eq!(
            raw(&[(Key::SHIFT, true), (Key(0x6B), true)], false),
            [HotkeyFieldEvent::ModifiersChanged(shift), captured("Shift+NumpadAdd")]
        );
        assert_eq!(raw(&[(Key::MOUSE4, true)], false), [captured("Mouse4")]);
        // Releases and plain clicks never commit.
        assert!(raw(&[(Key(0x61), false), (Key(0x01), true), (Key(0x02), true)], false).is_empty());
    }

    #[test]
    fn raw_capture_normalises_sided_modifiers_and_tracks_release() {
        let events = raw(
            &[
                (Key(0xA3), true),  // right Ctrl
                (Key(0xA0), true),  // left Shift
                (Key(0xA3), false), // Ctrl released again
                (Key(0x5C), true),  // right Win
                (Key(0xA5), true),  // right Alt
                (Key::f(9), true),
            ],
            false,
        );
        assert_eq!(events.last(), Some(&captured("Shift+Alt+Win+F9")));
    }

    #[test]
    fn raw_capture_escape_backspace_and_block() {
        assert_eq!(raw(&[(Key(0x1B), true)], false), [HotkeyFieldEvent::Cancelled]);
        assert_eq!(raw(&[(Key(0x08), true)], true), [HotkeyFieldEvent::Cleared]);
        assert_eq!(raw(&[(Key(0x2E), true)], true), [HotkeyFieldEvent::Cleared]);
        assert!(raw(&[(Key(0x08), true)], false).is_empty());
        // Too many keys is rejected with the hint, not dropped.
        let all = [Key::CTRL, Key::SHIFT, Key::ALT, Key::WIN];
        let mut seq: Vec<_> = all.iter().map(|&k| (k, true)).collect();
        seq.push((Key::f(9), true));
        assert_eq!(
            raw(&seq, false).last(),
            Some(&HotkeyFieldEvent::InvalidKey(UNSUPPORTED_KEY_HINT))
        );
        // The Enter that started listening cannot bind itself through auto-repeat.
        let mut cap = RawCapture::begin(Some(Key(0x0D)));
        assert_eq!(cap.push(Key(0x0D), true, false), None);
        assert_eq!(cap.push(Key(0x0D), false, false), None);
        assert_eq!(cap.push(Key(0x0D), true, false), Some(captured("Enter")));
    }

    #[test]
    fn gpui_fallback_still_commits_when_no_raw_events_arrive() {
        assert!(!RawCapture::begin(None).seen);
        assert_eq!(
            handle_listening_key("f9", mods(true, false, false, false), false, false),
            Some(captured("Ctrl+F9"))
        );
        let mut cap = RawCapture::begin(None);
        cap.push(Key::f(9), false, false);
        assert!(cap.seen, "any raw event hands the chord to the raw stream");
    }
}
