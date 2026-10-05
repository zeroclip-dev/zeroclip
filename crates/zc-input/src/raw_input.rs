//! Raw Input: keyboard and mouse transitions as [`Key`]s.

use windows::Win32::Foundation::{HWND, LPARAM};
use zc_core::hotkey::Key;
use zc_core::ports::{OsEvent, SourceError};

/// Registers the keyboard (and mouse, for side-button hotkeys) with the window.
///
/// Phase 2 plan: `RegisterRawInputDevices` with two `RAWINPUTDEVICE`s — usage page
/// `0x01` (generic desktop), usage `0x06` (keyboard) and `0x02` (mouse) — both
/// with `RIDEV_INPUTSINK` and `hwndTarget = hwnd`, so input arrives while ZeroClip
/// is in the background.
///
/// # Errors
/// Registration failures.
pub fn register(hwnd: HWND) -> Result<(), SourceError> {
    let _ = hwnd;
    todo!("Phase 2: RegisterRawInputDevices (see the plan above)")
}

/// Decodes a `WM_INPUT` message into key events.
///
/// Phase 2 plan: `GetRawInputData(HRAWINPUT(lparam), RID_INPUT, …)` into a
/// `RAWINPUT`. Keyboard: `VKey`, down unless `Flags & RI_KEY_BREAK`; keep the
/// generic `VK_CONTROL`/`VK_MENU` (the matcher normalises left/right anyway).
/// Mouse: see [`mouse_buttons`]. A single mouse report can carry several
/// transitions, hence the callback.
pub fn decode(lparam: LPARAM, emit: impl FnMut(OsEvent)) {
    let _ = (lparam, emit);
    todo!("Phase 2: GetRawInputData → Key events")
}

/// The button transitions in a `RAWMOUSE::usButtonFlags` value, as virtual keys:
/// middle → [`Key::MOUSE3`], back → [`Key::MOUSE4`], forward → [`Key::MOUSE5`].
/// Left/right clicks are ignored — nobody binds a hotkey to them.
///
/// (`RI_MOUSE_MIDDLE_BUTTON_DOWN` = 0x0010, `_UP` = 0x0020, `RI_MOUSE_BUTTON_4_DOWN`
/// = 0x0040, `_UP` = 0x0080, `RI_MOUSE_BUTTON_5_DOWN` = 0x0100, `_UP` = 0x0200.)
#[must_use]
pub fn mouse_buttons(button_flags: u16) -> Vec<(Key, bool)> {
    const TABLE: [(u16, Key, bool); 6] = [
        (0x0010, Key::MOUSE3, true),
        (0x0020, Key::MOUSE3, false),
        (0x0040, Key::MOUSE4, true),
        (0x0080, Key::MOUSE4, false),
        (0x0100, Key::MOUSE5, true),
        (0x0200, Key::MOUSE5, false),
    ];
    // Mouse moves (no button bits) give an empty Vec, which doesn't allocate.
    TABLE.iter().filter(|(bit, ..)| button_flags & bit != 0).map(|&(_, k, d)| (k, d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_side_buttons_map_to_virtual_keys() {
        assert_eq!(mouse_buttons(0x0040), [(Key::MOUSE4, true)]);
        assert_eq!(mouse_buttons(0x0200), [(Key::MOUSE5, false)]);
        assert_eq!(mouse_buttons(0x0010 | 0x0080), [(Key::MOUSE3, true), (Key::MOUSE4, false)]);
        assert!(mouse_buttons(0x0001 | 0x0002).is_empty(), "left clicks are ignored");
    }
}
