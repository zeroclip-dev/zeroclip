//! Converting Windows errors into [`SourceError`].

use zc_core::ports::SourceError;

/// Wraps a Windows API error with what was being attempted.
#[must_use]
pub fn platform_error(context: &str, error: &windows::core::Error) -> SourceError {
    SourceError::Platform {
        context: context.to_owned(),
        code: error.code().0,
        message: error.message(),
    }
}

/// `.context("D3D11CreateDevice")?` for `windows::core::Result`.
pub trait WinResultExt<T> {
    /// Maps the error into a [`SourceError::Platform`] naming `context`.
    ///
    /// # Errors
    /// The original error, wrapped.
    fn context(self, context: &str) -> Result<T, SourceError>;
}

impl<T> WinResultExt<T> for windows::core::Result<T> {
    fn context(self, context: &str) -> Result<T, SourceError> {
        self.map_err(|e| platform_error(context, &e))
    }
}
