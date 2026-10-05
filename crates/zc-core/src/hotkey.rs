//! Keys, chords and hotkey matching (DESIGN §2 "Input", §8 "Raw Input needs a message pump").
//!
//! The input adapter (`zc-input`) reads the keyboard and mouse through the Raw
//! Input API — invisible to anti-cheat, no `SetWindowsHookEx` — and reports every
//! transition as a [`Key`] (a Windows virtual-key code) going up or down. The
//! [`ChordMatcher`] turns that stream into hotkey actions.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A key or mouse button, identified by its Windows virtual-key code (`VK_*`).
///
/// Mouse buttons are keys too: Raw Input reports them, and side buttons
/// (Mouse4/Mouse5) are popular push-to-talk keys.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(pub u16);

impl Key {
    /// `VK_CONTROL` (either side).
    pub const CTRL: Self = Self(0x11);
    /// `VK_SHIFT` (either side).
    pub const SHIFT: Self = Self(0x10);
    /// `VK_MENU` — Alt (either side).
    pub const ALT: Self = Self(0x12);
    /// `VK_LWIN` — the Windows key (either side).
    pub const WIN: Self = Self(0x5B);
    /// `VK_MBUTTON` — middle mouse button.
    pub const MOUSE3: Self = Self(0x04);
    /// `VK_XBUTTON1` — back side button.
    pub const MOUSE4: Self = Self(0x05);
    /// `VK_XBUTTON2` — forward side button.
    pub const MOUSE5: Self = Self(0x06);

    /// Function key `F1`..=`F24`.
    ///
    /// # Panics
    /// If `n` is not in `1..=24`.
    #[must_use]
    pub const fn f(n: u8) -> Self {
        assert!(n >= 1 && n <= 24, "function keys are F1..=F24");
        Self(0x70 + n as u16 - 1)
    }

    /// A letter key `A`..=`Z` (case-insensitive).
    #[must_use]
    pub const fn letter(c: char) -> Option<Self> {
        let upper = c.to_ascii_uppercase();
        if upper.is_ascii_uppercase() { Some(Self(upper as u16)) } else { None }
    }

    /// Collapses left/right variants onto the generic key, so "Ctrl" matches either Ctrl.
    #[must_use]
    pub const fn normalized(self) -> Self {
        match self.0 {
            0xA0 | 0xA1 => Self::SHIFT,
            0xA2 | 0xA3 => Self::CTRL,
            0xA4 | 0xA5 => Self::ALT,
            0x5C => Self::WIN,
            _ => self,
        }
    }

    /// Whether this is a modifier (Ctrl, Shift, Alt, Win), after normalisation.
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        matches!(self.normalized().0, 0x10 | 0x11 | 0x12 | 0x5B)
    }

    /// Sort rank: modifiers first (Ctrl, Shift, Alt, Win), then everything else by code.
    const fn rank(self) -> (u8, u16) {
        match self.0 {
            0x11 => (0, 0),
            0x10 => (0, 1),
            0x12 => (0, 2),
            0x5B => (0, 3),
            code => (1, code),
        }
    }
}

/// Names for keys that aren't letters, digits or F-keys.
const NAMED: &[(u16, &str)] = &[
    (0x01, "Mouse1"),
    (0x02, "Mouse2"),
    (0x04, "Mouse3"),
    (0x05, "Mouse4"),
    (0x06, "Mouse5"),
    (0x08, "Backspace"),
    (0x09, "Tab"),
    (0x0D, "Enter"),
    (0x10, "Shift"),
    (0x11, "Ctrl"),
    (0x12, "Alt"),
    (0x13, "Pause"),
    (0x14, "CapsLock"),
    (0x1B, "Esc"),
    (0x20, "Space"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x2C, "PrintScreen"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x5B, "Win"),
    (0x6A, "NumpadMultiply"),
    (0x6B, "NumpadAdd"),
    (0x6D, "NumpadSubtract"),
    (0x6E, "NumpadDecimal"),
    (0x6F, "NumpadDivide"),
    (0x90, "NumLock"),
    (0x91, "ScrollLock"),
    (0xBA, "Semicolon"),
    (0xBB, "Equals"),
    (0xBC, "Comma"),
    (0xBD, "Minus"),
    (0xBE, "Period"),
    (0xBF, "Slash"),
    (0xC0, "Backtick"),
    (0xDB, "LeftBracket"),
    (0xDC, "Backslash"),
    (0xDD, "RightBracket"),
    (0xDE, "Quote"),
];

/// Alternative spellings accepted when parsing.
const ALIASES: &[(&str, u16)] = &[
    ("control", 0x11),
    ("escape", 0x1B),
    ("return", 0x0D),
    ("del", 0x2E),
    ("ins", 0x2D),
    ("pgup", 0x21),
    ("pgdn", 0x22),
    ("prtsc", 0x2C),
    ("windows", 0x5B),
    ("super", 0x5B),
    ("grave", 0xC0),
];

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.normalized().0;
        match code {
            0x30..=0x39 | 0x41..=0x5A => {
                // ASCII digits and letters map to themselves.
                let c = char::from_u32(u32::from(code)).unwrap_or('?');
                write!(f, "{c}")
            }
            0x60..=0x69 => write!(f, "Numpad{}", code - 0x60),
            0x70..=0x87 => write!(f, "F{}", code - 0x70 + 1),
            _ => match NAMED.iter().find(|(c, _)| *c == code) {
                Some((_, name)) => f.write_str(name),
                None => write!(f, "VK_{code:#04X}"),
            },
        }
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({self})")
    }
}

/// Why a key or chord string didn't parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChordError {
    /// Nothing to parse.
    #[error("empty hotkey")]
    Empty,
    /// A segment isn't a key name.
    #[error("unknown key {0:?}")]
    UnknownKey(String),
    /// More keys than anyone can press at once.
    #[error("a hotkey can have at most {max} keys", max = Chord::MAX_KEYS)]
    TooManyKeys,
}

impl FromStr for Key {
    type Err = ChordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ChordError::Empty);
        }
        let lower = s.to_ascii_lowercase();
        let unknown = || ChordError::UnknownKey(s.to_owned());

        if s.len() == 1 {
            let c = s.chars().next().ok_or_else(unknown)?;
            if let Some(k) = Key::letter(c) {
                return Ok(k);
            }
            if c.is_ascii_digit() {
                return Ok(Key(c as u16));
            }
        }
        if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
            && (1..=24).contains(&n)
        {
            return Ok(Key::f(n));
        }
        if let Some(n) = lower.strip_prefix("numpad").and_then(|n| n.parse::<u16>().ok())
            && n <= 9
        {
            return Ok(Key(0x60 + n));
        }
        if let Some(hex) = lower.strip_prefix("vk_0x") {
            return u16::from_str_radix(hex, 16).map(Key).map_err(|_| unknown());
        }
        NAMED
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(s))
            .map(|&(code, _)| Key(code))
            .or_else(|| ALIASES.iter().find(|(a, _)| *a == lower).map(|&(_, code)| Key(code)))
            .ok_or_else(unknown)
    }
}

/// A key combination such as `Alt+F10`.
///
/// Stored normalised (left/right modifiers collapsed), de-duplicated and sorted
/// with modifiers first, so `F10+Alt` and `alt + f10` are the same chord and both
/// display as `Alt+F10`. Serialises as that string.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    keys: Vec<Key>,
}

impl Chord {
    /// Maximum number of keys in one chord.
    pub const MAX_KEYS: usize = 4;

    /// Builds a chord from keys in any order.
    ///
    /// # Errors
    /// [`ChordError::Empty`] for no keys, [`ChordError::TooManyKeys`] for more than [`Self::MAX_KEYS`].
    pub fn new(keys: impl IntoIterator<Item = Key>) -> Result<Self, ChordError> {
        let mut keys: Vec<Key> = keys.into_iter().map(Key::normalized).collect();
        keys.sort_by_key(|k| k.rank());
        keys.dedup();
        match keys.len() {
            0 => Err(ChordError::Empty),
            n if n > Self::MAX_KEYS => Err(ChordError::TooManyKeys),
            _ => Ok(Self { keys }),
        }
    }

    /// The keys, modifiers first.
    #[must_use]
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// Whether `key` (after normalisation) is part of this chord.
    #[must_use]
    pub fn contains(&self, key: Key) -> bool {
        self.keys.contains(&key.normalized())
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, key) in self.keys.iter().enumerate() {
            if i > 0 {
                f.write_str("+")?;
            }
            write!(f, "{key}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Chord({self})")
    }
}

impl FromStr for Chord {
    type Err = ChordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let keys = s.split('+').map(str::parse).collect::<Result<Vec<Key>, _>>()?;
        Self::new(keys)
    }
}

impl Serialize for Chord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Chord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// How a binding fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Fires [`HotkeyEvent::Pressed`] once when the chord completes (save clip, screenshot…).
    Press,
    /// Fires `Pressed` when the chord completes and `Released` when any of its keys
    /// is released (push-to-talk).
    Hold,
}

/// A chord bound to an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding<A> {
    /// The keys.
    pub chord: Chord,
    /// What it does.
    pub action: A,
    /// Press or hold.
    pub trigger: Trigger,
}

/// Output of the [`ChordMatcher`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent<A> {
    /// A binding's chord completed.
    Pressed(A),
    /// A [`Trigger::Hold`] binding was let go.
    Released(A),
}

/// Turns raw key transitions into hotkey events.
///
/// Matching rules:
///
/// - Keys are normalised first: left and right modifiers are the same key.
/// - A binding fires when the key that just went down completes its chord: all of
///   the chord's keys are held, and the held *modifiers* are exactly the chord's
///   modifiers (`Ctrl+Alt+F10` does not trigger `Alt+F10`). Other held keys —
///   WASD while moving, say — never block a hotkey.
/// - [`Trigger::Hold`] bindings (push-to-talk) only need their own modifiers: extra
///   held modifiers don't block them, because players hold Shift or Ctrl to sprint
///   or crouch while they talk.
/// - Auto-repeat is ignored: a key-down for a key that is already down does nothing.
/// - If several bindings complete at once, the one with the most keys wins.
/// - `on_key` runs on the input thread for every key press, so it must not
///   allocate: size any state in [`new`](Self::new).
#[derive(Clone, Debug)]
pub struct ChordMatcher<A> {
    bindings: Vec<Binding<A>>,
    /// Held keys: a 256-bit set indexed by normalised virtual-key code.
    // ponytail: one bit per normalised key, so pressing both Ctrls and releasing one
    // clears Ctrl. Rare enough not to matter.
    held: [u64; 4],
    /// Per binding: a [`Trigger::Hold`] binding that is currently pressed.
    active: Vec<bool>,
}

impl<A: Copy> ChordMatcher<A> {
    /// A matcher for these bindings.
    #[must_use]
    pub fn new(bindings: Vec<Binding<A>>) -> Self {
        let active = vec![false; bindings.len()];
        Self { bindings, held: [0; 4], active }
    }

    fn is_held(&self, key: Key) -> bool {
        let k = usize::from(key.0);
        self.held[k / 64] & (1 << (k % 64)) != 0
    }

    fn set_held(&mut self, key: Key, down: bool) {
        let k = usize::from(key.0);
        if down {
            self.held[k / 64] |= 1 << (k % 64);
        } else {
            self.held[k / 64] &= !(1 << (k % 64));
        }
    }

    /// The bindings, as given.
    #[must_use]
    pub fn bindings(&self) -> &[Binding<A>] {
        &self.bindings
    }

    /// Feeds one key transition and returns the resulting hotkey event, if any.
    pub fn on_key(&mut self, key: Key, down: bool) -> Option<HotkeyEvent<A>> {
        let k = key.normalized();
        if usize::from(k.0) >= 256 {
            return None;
        }
        if !down {
            if !self.is_held(k) {
                return None;
            }
            self.set_held(k, false);
            let i = (0..self.bindings.len())
                .find(|&i| self.active[i] && self.bindings[i].chord.contains(k))?;
            self.active[i] = false;
            return Some(HotkeyEvent::Released(self.bindings[i].action));
        }
        if self.is_held(k) {
            return None; // auto-repeat
        }
        self.set_held(k, true);
        let modifier_held = |m: Key| self.is_held(m);
        let mut best: Option<usize> = None;
        for (i, b) in self.bindings.iter().enumerate() {
            let keys = b.chord.keys();
            if !b.chord.contains(k) || !keys.iter().all(|&c| self.is_held(c)) {
                continue;
            }
            // A press needs exactly the chord's modifiers; a hold only its own.
            let exact = [Key::CTRL, Key::SHIFT, Key::ALT, Key::WIN]
                .into_iter()
                .all(|m| modifier_held(m) == keys.contains(&m));
            if b.trigger == Trigger::Press && !exact {
                continue;
            }
            if best.is_none_or(|j| keys.len() > self.bindings[j].chord.keys().len()) {
                best = Some(i);
            }
        }
        let i = best?;
        if self.bindings[i].trigger == Trigger::Hold {
            self.active[i] = true;
        }
        Some(HotkeyEvent::Pressed(self.bindings[i].action))
    }

    /// Forgets every held key, e.g. after the input thread restarts. Holds that
    /// were active are *not* reported as released; the engine handles that.
    pub fn reset(&mut self) {
        self.held = [0; 4];
        self.active.fill(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Act {
        SaveClip,
        Record,
        Ptt,
    }

    const W: Key = Key(0x57);
    const A: Key = Key(0x41);

    fn chord(s: &str) -> Chord {
        s.parse().unwrap()
    }

    fn matcher() -> ChordMatcher<Act> {
        ChordMatcher::new(vec![
            Binding { chord: chord("Alt+F10"), action: Act::SaveClip, trigger: Trigger::Press },
            Binding { chord: chord("Alt+F9"), action: Act::Record, trigger: Trigger::Press },
            Binding { chord: chord("Mouse5"), action: Act::Ptt, trigger: Trigger::Hold },
        ])
    }

    #[test]
    fn keys_display_and_parse() {
        assert_eq!(Key::f(10).to_string(), "F10");
        assert_eq!(Key::ALT.to_string(), "Alt");
        assert_eq!(Key(0xA5).to_string(), "Alt", "right Alt displays as Alt");
        assert_eq!(Key(0x30).to_string(), "0");
        assert_eq!(Key(0x63).to_string(), "Numpad3");
        assert_eq!(Key(0xFF).to_string(), "VK_0xFF");
        for name in ["F24", "Mouse4", "PageUp", "Numpad7", "Backtick", "Q", "7", "VK_0xFF"] {
            let key: Key = name.parse().unwrap();
            assert_eq!(key.to_string(), name);
        }
        assert_eq!("escape".parse::<Key>(), Ok(Key(0x1B)));
        assert_eq!("f25".parse::<Key>(), Err(ChordError::UnknownKey("f25".into())));
    }

    #[test]
    fn chords_are_normalised_and_order_independent() {
        assert_eq!(chord("F10+Alt"), chord("alt + f10"));
        assert_eq!(chord("f10 + ALT").to_string(), "Alt+F10");
        assert_eq!(chord("Shift+S+Ctrl").to_string(), "Ctrl+Shift+S");
        assert_eq!(Chord::new([Key(0xA4), Key::f(1)]).unwrap(), chord("Alt+F1"));
        assert!(chord("Ctrl+Alt+F10").contains(Key(0xA3)), "right Ctrl is Ctrl");
    }

    #[test]
    fn bad_chords_are_rejected() {
        assert_eq!("".parse::<Chord>(), Err(ChordError::Empty));
        assert_eq!("Alt+".parse::<Chord>(), Err(ChordError::Empty));
        assert_eq!("Alt+Nope".parse::<Chord>(), Err(ChordError::UnknownKey("Nope".into())));
        assert_eq!("Ctrl+Shift+Alt+Win+F1".parse::<Chord>(), Err(ChordError::TooManyKeys));
    }

    #[test]
    fn chords_serialise_as_strings() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Hotkeys {
            save: Chord,
        }
        let text = toml::to_string(&Hotkeys { save: chord("Alt+F10") }).unwrap();
        assert_eq!(text.trim(), r#"save = "Alt+F10""#);
        let back: Hotkeys = toml::from_str(&text).unwrap();
        assert_eq!(back.save, chord("Alt+F10"));
        assert!(toml::from_str::<Hotkeys>(r#"save = "Alt+Nope""#).is_err());
    }

    #[test]
    fn hotkey_fires_when_the_chord_completes() {
        let mut m = matcher();
        assert_eq!(m.on_key(Key::ALT, true), None);
        assert_eq!(m.on_key(Key::f(10), true), Some(HotkeyEvent::Pressed(Act::SaveClip)));
        assert_eq!(m.on_key(Key::f(10), false), None);
        assert_eq!(m.on_key(Key::ALT, false), None);
    }

    #[test]
    fn hotkey_ignores_auto_repeat() {
        let mut m = matcher();
        m.on_key(Key::ALT, true);
        let events: Vec<_> = (0..5).filter_map(|_| m.on_key(Key::f(10), true)).collect();
        assert_eq!(events, [HotkeyEvent::Pressed(Act::SaveClip)]);
    }

    #[test]
    fn hotkey_is_not_blocked_by_held_movement_keys() {
        let mut m = matcher();
        m.on_key(W, true);
        m.on_key(A, true);
        m.on_key(Key::ALT, true);
        assert_eq!(m.on_key(Key::f(10), true), Some(HotkeyEvent::Pressed(Act::SaveClip)));
    }

    #[test]
    fn hotkey_requires_exactly_the_chords_modifiers() {
        let mut m = matcher();
        assert_eq!(m.on_key(Key::f(10), true), None, "F10 alone isn't Alt+F10");
        m.on_key(Key::f(10), false);
        m.on_key(Key::CTRL, true);
        m.on_key(Key::ALT, true);
        assert_eq!(m.on_key(Key::f(10), true), None, "Ctrl+Alt+F10 isn't Alt+F10");
    }

    #[test]
    fn hotkey_treats_left_and_right_modifiers_alike() {
        let mut m = matcher();
        m.on_key(Key(0xA5), true); // VK_RMENU, right Alt
        assert_eq!(m.on_key(Key::f(9), true), Some(HotkeyEvent::Pressed(Act::Record)));
    }

    #[test]
    fn hotkey_push_to_talk_works_while_holding_modifiers() {
        let mut m = matcher();
        m.on_key(Key::SHIFT, true); // sprinting
        assert_eq!(m.on_key(Key::MOUSE5, true), Some(HotkeyEvent::Pressed(Act::Ptt)));
        assert_eq!(m.on_key(Key::SHIFT, false), None, "releasing Shift doesn't end the hold");
        assert_eq!(m.on_key(Key::MOUSE5, false), Some(HotkeyEvent::Released(Act::Ptt)));
    }

    #[test]
    fn hotkey_hold_binding_reports_press_and_release() {
        let mut m = matcher();
        assert_eq!(m.on_key(Key::MOUSE5, true), Some(HotkeyEvent::Pressed(Act::Ptt)));
        assert_eq!(m.on_key(Key::MOUSE5, true), None, "repeat");
        assert_eq!(m.on_key(Key::MOUSE5, false), Some(HotkeyEvent::Released(Act::Ptt)));
        assert_eq!(m.on_key(Key::MOUSE5, false), None, "already released");
    }
}
