//! COM / WinRT apartment lifetime.

use std::marker::PhantomData;

use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use zc_core::ports::SourceError;

use crate::error::WinResultExt;

/// Keeps the current thread in the multithreaded apartment (MTA) until dropped.
///
/// Create one at the top of every thread that touches COM, WinRT
/// (Windows.Graphics.Capture) or Media Foundation. `RoInitialize` initialises both
/// COM and WinRT. Not `Send`: apartment membership belongs to the thread.
#[derive(Debug)]
pub struct Apartment {
    _not_send: PhantomData<*const ()>,
}

impl Apartment {
    /// Joins the MTA.
    ///
    /// # Errors
    /// If the thread already belongs to a single-threaded apartment
    /// (`RPC_E_CHANGED_MODE`) — e.g. a UI thread.
    pub fn multithreaded() -> Result<Self, SourceError> {
        // SAFETY: plain FFI call with a valid enum value; balanced by `RoUninitialize`
        // in `Drop`, on the same thread (the guard is !Send).
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("RoInitialize")?;
        Ok(Self { _not_send: PhantomData })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `RoInitialize` in `multithreaded`, on the
        // same thread.
        unsafe { RoUninitialize() };
    }
}
