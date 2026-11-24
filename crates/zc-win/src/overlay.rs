//! Whether the user wants motion. The toast itself is in [`crate::toast`].

use std::ffi::c_void;

use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::BOOL;

/// Whether the user has "Show animations in Windows" on
/// ([`SPI_GETCLIENTAREAANIMATION`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-systemparametersinfow)).
/// Returns `true` if the query fails, so motion is the default.
pub fn system_animations_enabled() -> bool {
    let mut on = BOOL(1);
    // SAFETY: the buffer is a BOOL that lives across the call, and the action's documented
    // output is one BOOL. The pointer is only written to during the call.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(std::ptr::from_mut(&mut on).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_err() || on.as_bool()
}
