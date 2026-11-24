//! Explorer actions: show a file in its folder, move it to the Recycle Bin, pick a folder.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

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
/// The shell refused (the file is open elsewhere, or is gone).
pub fn recycle(path: &Path) -> io::Result<()> {
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

/// Shows the standard Windows "choose a folder" dialog, starting in `start` when it exists.
/// Blocks until the user answers. `None` means they cancelled.
/// [IFileOpenDialog](https://learn.microsoft.com/windows/win32/api/shobjidl_core/nn-shobjidl_core-ifileopendialog)
/// with `FOS_PICKFOLDERS` is what Explorer-style apps use; it needs the calling thread's COM
/// apartment, which the UI thread already has.
///
/// # Errors
/// The dialog couldn't be created or its answer couldn't be read.
pub fn pick_folder(start: Option<&Path>) -> io::Result<Option<PathBuf>> {
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
        SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    use windows::core::HRESULT;

    let fail = |e: windows::core::Error| io::Error::other(e.to_string());
    // SAFETY: plain COM calls on an interface this function creates and owns. `wide` outlives the
    // `SHCreateItemFromParsingName` call that reads it, and the string the shell returns is freed
    // once, after it is copied out.
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        let options = dialog.GetOptions().map_err(fail)?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).map_err(fail)?;
        if let Some(start) = start.filter(|p| p.is_dir()) {
            let mut wide: Vec<u16> = start.as_os_str().encode_wide().collect();
            wide.push(0);
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
        }
        // Owned by the window in front, so the dialog opens above the app instead of behind it.
        let owner: HWND = GetForegroundWindow();
        if let Err(e) = dialog.Show(Some(owner)) {
            if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                return Ok(None);
            }
            return Err(fail(e));
        }
        let item = dialog.GetResult().map_err(fail)?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(fail)?;
        let path = name.to_string().map_err(|e| io::Error::other(e.to_string()));
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(|p| Some(PathBuf::from(p)))
    }
}
