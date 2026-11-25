//! Explorer actions: show a file in its folder, move it to the Recycle Bin.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::Path;

use windows::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
    SHFileOperationW,
};
use windows::core::PCWSTR;

/// Opens an Explorer window with `path` selected.
///
/// `explorer /select,"<path>"` is the documented way. The quotes go around the path only, so the
/// argument is passed raw instead of through Rust's own quoting.
///
/// # Errors
/// Explorer couldn't be started.
pub fn reveal(path: &Path) -> io::Result<()> {
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(drop)
}

/// Moves `path` to the Recycle Bin, so a mistaken delete can be undone from Explorer.
/// [SHFileOperationW](https://learn.microsoft.com/windows/win32/api/shellapi/nf-shellapi-shfileoperationw)
/// with `FOF_ALLOWUNDO` is what Explorer itself uses.
///
/// # Errors
/// The path isn't absolute, or the shell refused (the file is open elsewhere, or is gone).
pub fn recycle(path: &Path) -> io::Result<()> {
    // The shell resolves a relative path against its own current folder and, without a full
    // path, can delete outright instead of recycling.
    if !path.is_absolute() {
        return Err(io::Error::other("only a full path can go to the Recycle Bin"));
    }
    // `pFrom` is a list of paths, each ending in a NUL, with one more NUL after the last.
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.extend([0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
        ..Default::default()
    };
    // SAFETY: `op` and the `from` buffer it points at are alive for the whole call; the shell
    // only reads them.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err(io::Error::other(format!("couldn't move it to the Recycle Bin (code {code})")));
    }
    Ok(())
}
