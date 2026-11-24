//! Drag a file out of the app, so it can be dropped on a browser's upload box or Explorer.
//!
//! The Shell builds the data object for a file: [`IShellItem::BindToHandler`] with
//! `BHID_DataObject` gives an `IDataObject` that carries it the way Explorer does (file drop,
//! shell id list), which browsers accept. [`SHDoDragDrop`] runs the drag with the Shell's own drop
//! source, so no COM class is implemented here. It needs the calling thread to be in an OLE
//! apartment, which GPUI's UI thread is.
//!
//! <https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shdodragdrop>

use std::path::Path;

use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK};
use windows::Win32::UI::Shell::{
    BHID_DataObject, IShellItem, SHCreateItemFromParsingName, SHDoDragDrop,
};
use windows::core::HSTRING;

/// Runs a drag of `file` and returns when it is dropped or cancelled. Blocks, so call it from a
/// mouse-move handler while the button is down.
///
/// # Errors
/// The file can't be found, or the Shell refused the drag.
pub fn drag_file(file: &Path) -> windows::core::Result<()> {
    let path = HSTRING::from(file.as_os_str());
    // SAFETY: `path` is a valid wide string that lives across the call.
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(&path, None) }?;
    // SAFETY: `item` is a live shell item; no bind context is needed.
    let data: IDataObject = unsafe { item.BindToHandler(None, &BHID_DataObject) }?;
    // SAFETY: `data` is live for the whole call. A null drop source makes the Shell use its own.
    unsafe { SHDoDragDrop(None, &data, None, DROPEFFECT_COPY | DROPEFFECT_LINK) }?;
    Ok(())
}
