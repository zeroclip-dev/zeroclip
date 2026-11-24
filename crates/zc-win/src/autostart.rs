//! "Start with Windows": the per-user Run key that starts ZeroClip at sign-in.
//!
//! Windows starts every value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` when the
//! user signs in. The key is per user, so no administrator rights are needed. See
//! [Run and RunOnce registry keys](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys).

use std::path::Path;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{PCWSTR, w};
use zc_core::ports::SourceError;

use crate::error::WinResultExt;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: PCWSTR = w!("ZeroClip");

/// The Run value for the executable at `exe`: the quoted path, then `--tray` so a sign-in start
/// stays hidden in the tray. The quotes matter: an unquoted path with a space would start the
/// wrong program.
#[must_use]
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\" --tray", exe.display())
}

/// Adds the Run value (`enabled`) or removes it. Removing a value that isn't there succeeds.
///
/// # Errors
/// The registry call failed, or the command is too long for a registry value.
pub fn set(enabled: bool, command: &str) -> Result<(), SourceError> {
    if enabled {
        let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
        let bytes = u32::try_from(wide.len() * 2).map_err(|_| SourceError::Platform {
            context: "RegSetKeyValueW".to_owned(),
            code: 0,
            message: "the start-with-Windows command is too long".to_owned(),
        })?;
        // SAFETY: `RUN_KEY` and `VALUE` are static NUL-terminated wide strings. `wide` is a
        // NUL-terminated UTF-16 buffer of `bytes` bytes that lives until the call returns, and
        // the API only reads it (REG_SZ data is copied into the registry).
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                bytes,
            )
        };
        return status.ok().context("RegSetKeyValueW");
    }
    // SAFETY: `RUN_KEY` and `VALUE` are static NUL-terminated wide strings; the call takes no
    // buffers and only deletes the value.
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status.ok().context("RegDeleteKeyValueW")
}

/// Whether the Run value exists. Any error counts as "not enabled".
#[must_use]
pub fn is_enabled() -> bool {
    // SAFETY: `RUN_KEY` and `VALUE` are static NUL-terminated wide strings. With no output
    // buffer, RegGetValueW only reports whether a REG_SZ value exists; it writes nothing.
    let status =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, None, None) };
    status == ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_quotes_the_path_and_starts_in_the_tray() {
        let exe = Path::new(r"C:\Program Files\ZeroClip\zeroclip.exe");
        assert_eq!(run_value(exe), r#""C:\Program Files\ZeroClip\zeroclip.exe" --tray"#);
    }
}
