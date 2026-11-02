//! Media Foundation lifetime.

use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use zc_core::ports::SourceError;

use crate::error::WinResultExt;

/// Keeps Media Foundation started until dropped. `MFStartup` is reference-counted,
/// so the video and audio pipelines can each hold one. See
/// [`MFStartup`](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mfstartup).
#[derive(Debug)]
pub struct MediaFoundation(());

impl MediaFoundation {
    /// Calls `MFStartup(MF_VERSION, MFSTARTUP_FULL)`. `LITE` would only skip the sockets library,
    /// which we never use, so `FULL` is just the documented default.
    /// See [`MFStartup`](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mfstartup).
    ///
    /// # Errors
    /// If Media Foundation is missing — e.g. Windows "N" editions without the
    /// Media Feature Pack.
    pub fn start() -> Result<Self, SourceError> {
        // SAFETY: plain FFI call with documented constants; balanced by
        // `MFShutdown` in `Drop`.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;
        Ok(Self(()))
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: balances the successful `MFStartup` in `start`. If shutdown fails
        // there is nothing left to clean up, so the result is ignored.
        let _ = unsafe { MFShutdown() };
    }
}
