//! Raw Input: keyboard and mouse transitions as [`Key`]s.
//!
//! Raw Input delivers `WM_INPUT` messages to the registered window, with the device's own
//! report instead of the translated character stream. See the
//! [Raw Input overview](https://learn.microsoft.com/en-us/windows/win32/inputdev/raw-input).

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::RI_KEY_BREAK;
use zc_core::hotkey::Key;
use zc_core::ports::{OsEvent, SourceError};
use zc_win::error::WinResultExt;

// HID usages for the generic-desktop page (the `HID_*` constants need another
// windows feature for three numbers).
const USAGE_PAGE_GENERIC: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
const USAGE_KEYBOARD: u16 = 0x06;

/// Registers the keyboard (and mouse, for side-button hotkeys) with the window.
///
/// Phase 2 plan: [`RegisterRawInputDevices`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerrawinputdevices)
/// with two [`RAWINPUTDEVICE`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-rawinputdevice)s — usage page
/// `0x01` (generic desktop), usage `0x06` (keyboard) and `0x02` (mouse) — both
/// with `RIDEV_INPUTSINK` and `hwndTarget = hwnd`, so input arrives while ZeroClip
/// is in the background. The usage IDs are the [HID generic desktop page](https://learn.microsoft.com/en-us/windows-hardware/drivers/hid/hid-usages).
/// `RIDEV_INPUTSINK` is the flag that delivers input while the window is not focused.
///
/// # Errors
/// Registration failures.
pub fn register(hwnd: HWND) -> Result<(), SourceError> {
    // Never RIDEV_NOLEGACY: the egui window still needs WM_KEYDOWN.
    let device = |usage| RAWINPUTDEVICE {
        usUsagePage: USAGE_PAGE_GENERIC,
        usUsage: usage,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    let devices = [device(USAGE_KEYBOARD), device(USAGE_MOUSE)];
    // SAFETY: a valid array of device descriptions and its element size.
    unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) }
        .context("RegisterRawInputDevices")
}

/// Decodes a `WM_INPUT` message into key events.
///
/// Phase 2 plan: [`GetRawInputData`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getrawinputdata)
/// (`HRAWINPUT(lparam)`, `RID_INPUT`) into a [`RAWINPUT`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-rawinput).
/// Keyboard: `VKey`, down unless `Flags & RI_KEY_BREAK` (see [`RAWKEYBOARD`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-rawkeyboard));
/// keep the generic `VK_CONTROL`/`VK_MENU` (the matcher normalises left/right anyway).
/// Mouse: see [`mouse_buttons`]. A single mouse report can carry several
/// transitions, hence the callback. Key codes are [virtual-key codes](https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes).
pub fn decode(lparam: LPARAM, mut emit: impl FnMut(OsEvent)) {
    // Runs inside the message loop for every key and mouse report: no panics, no
    // allocation for plain mouse moves, and keystrokes are never logged.
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: `raw` is a writable buffer of `size` bytes; the handle comes from the
    // WM_INPUT message being handled.
    let read = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut _),
            RID_INPUT,
            Some((&raw mut raw).cast()),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if read == 0 || read == u32::MAX {
        return;
    }
    if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
        // SAFETY: the header says this is keyboard data.
        let kb = unsafe { raw.data.keyboard };
        if kb.VKey == 0xFF {
            return; // fake keys from escape sequences
        }
        emit(OsEvent::Key { key: Key(kb.VKey), down: kb.Flags & RI_KEY_BREAK as u16 == 0 });
    } else if raw.header.dwType == RIM_TYPEMOUSE.0 {
        // SAFETY: the header says this is mouse data.
        let flags = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags };
        for (key, down) in mouse_buttons(flags) {
            emit(OsEvent::Key { key, down });
        }
    }
}

/// The button transitions in a `RAWMOUSE::usButtonFlags` value, as virtual keys:
/// middle → [`Key::MOUSE3`], back → [`Key::MOUSE4`], forward → [`Key::MOUSE5`].
/// Left/right clicks are ignored — nobody binds a hotkey to them.
///
/// (`RI_MOUSE_MIDDLE_BUTTON_DOWN` = 0x0010, `_UP` = 0x0020, `RI_MOUSE_BUTTON_4_DOWN`
/// = 0x0040, `_UP` = 0x0080, `RI_MOUSE_BUTTON_5_DOWN` = 0x0100, `_UP` = 0x0200; see
/// [`RAWMOUSE`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-rawmouse).)
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
