//! Page-locking the DVR buffer.
//!
//! The DVR rings are large and mostly cold, so Windows may page them out when memory runs short.
//! A dump would then fault them back in from the pagefile at the moment the user wants the clip.
//! [`lock`] pins a region in RAM with
//! [`VirtualLock`](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtuallock).
//!
//! `VirtualLock` pins only within the process's *minimum* working set, which defaults to about
//! 200 KB. So [`lock`] first raises the minimum and the maximum by the region's size, with
//! [`SetProcessWorkingSetSize`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessworkingsetsize).
//! That needs `SeIncreaseWorkingSetPrivilege`, which ordinary users hold. Dropping the
//! [`PageLock`] unpins the pages and takes the raise back, so restarts don't pile it up.

use std::ffi::c_void;
use std::ptr;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{VirtualLock, VirtualUnlock};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
};
use zc_core::ports::SourceError;

use crate::error::WinResultExt;

/// Slack added to each raise. A region can start and end partway through a page, so the locked
/// pages can be two more than its length needs.
const PAGE_SLACK: usize = 2 * 4096;

/// A region pinned in RAM. Dropping it unpins the region and returns the working-set room that
/// [`lock`] raised for it.
#[derive(Debug)]
pub struct PageLock {
    /// Start of the pinned region. Only handed back to `VirtualUnlock`, never dereferenced.
    ptr: *const c_void,
    /// Length of the pinned region, in bytes. Zero for an empty region, which pins nothing.
    len: usize,
}

// SAFETY: the pointer is never read through. It is only passed back to `VirtualUnlock`, which
// may be called from any thread, so a guard can be dropped on whichever thread owns it.
unsafe impl Send for PageLock {}

impl Drop for PageLock {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        // SAFETY: `ptr` and `len` are the region the matching `VirtualLock` call pinned. The
        // memory is still allocated, because the owner drops this guard before it frees the
        // region (the DVR's `Recorder` declares its locks ahead of its rings). `VirtualUnlock`
        // only unpins the pages and never writes to them.
        let _ = unsafe { VirtualUnlock(self.ptr, self.len) };
        lower_working_set(raise_for(self.len));
    }
}

/// Pins `region` in RAM until the returned guard is dropped. An empty region pins nothing.
///
/// # Errors
/// If the working-set quota can't be raised, or the pages can't be locked (for example, the
/// region is larger than Windows will pin). On error nothing stays pinned or raised, so the
/// caller can carry on unpinned.
pub fn lock(region: &[u8]) -> Result<PageLock, SourceError> {
    if region.is_empty() {
        return Ok(PageLock { ptr: ptr::null(), len: 0 });
    }
    let extra = raise_for(region.len());
    raise_working_set(extra)?;
    let ptr = region.as_ptr().cast::<c_void>();
    // SAFETY: `region` is a live slice for the whole call, so its pages stay mapped. VirtualLock
    // only pins them and never writes through the pointer.
    unsafe { VirtualLock(ptr, region.len()) }
        .context("VirtualLock")
        .inspect_err(|_| lower_working_set(extra))?;
    Ok(PageLock { ptr, len: region.len() })
}

/// This machine's physical RAM in MiB, or `None` if Windows doesn't say. The engine caps the DVR's
/// RAM ceiling at a share of it.
///
/// Reads `ullTotalPhys` from
/// [`GlobalMemoryStatusEx`](https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-globalmemorystatusex).
#[must_use]
pub fn total_physical_mib() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?,
        ..MEMORYSTATUSEX::default()
    };
    // SAFETY: `status` is a live local for the whole call, and `dwLength` holds its size, which is
    // the only field the API reads before it writes the struct.
    unsafe { GlobalMemoryStatusEx(&raw mut status) }.ok()?;
    Some(status.ullTotalPhys >> 20)
}

/// The working-set raise a region of `len` bytes needs.
fn raise_for(len: usize) -> usize {
    len.saturating_add(PAGE_SLACK)
}

/// Raises the process's minimum and maximum working set by `extra` bytes each.
fn raise_working_set(extra: usize) -> Result<(), SourceError> {
    let (min, max) = working_set()?;
    set_working_set(min.saturating_add(extra), max.saturating_add(extra))
}

/// Takes back `extra` bytes of an earlier raise. Best effort: if the call fails, the minimum
/// stays higher, which only keeps more of the process resident.
fn lower_working_set(extra: usize) {
    if let Ok((min, max)) = working_set() {
        let _ = set_working_set(min.saturating_sub(extra), max.saturating_sub(extra));
    }
}

/// The process's minimum and maximum working-set sizes, in bytes.
fn working_set() -> Result<(usize, usize), SourceError> {
    let (mut min, mut max) = (0_usize, 0_usize);
    // SAFETY: `current_process` is this process's pseudo-handle, and `min` and `max` are
    // writable locals that live for the call, which is all the API writes through them.
    unsafe { GetProcessWorkingSetSize(current_process(), &raw mut min, &raw mut max) }
        .context("GetProcessWorkingSetSize")?;
    Ok((min, max))
}

/// Sets the process's minimum and maximum working-set sizes, in bytes.
fn set_working_set(min: usize, max: usize) -> Result<(), SourceError> {
    // SAFETY: `current_process` is this process's pseudo-handle. The two sizes are plain integers.
    unsafe { SetProcessWorkingSetSize(current_process(), min, max) }
        .context("SetProcessWorkingSetSize")
}

/// This process's pseudo-handle. It needs no `CloseHandle` and stays valid for the process's life.
fn current_process() -> HANDLE {
    // SAFETY: `GetCurrentProcess` takes no arguments and has no preconditions.
    unsafe { GetCurrentProcess() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_a_small_buffer_and_gives_the_working_set_back() {
        // The working-set quota belongs to the whole process, so this is the only test here that
        // touches it: a parallel test would skew the before-and-after comparison.
        let before = working_set().expect("query the working set");
        let buf = vec![0u8; 1 << 20];
        let pin = lock(&buf).expect("a 1 MiB buffer should lock");
        drop(pin);
        assert_eq!(working_set().expect("query the working set"), before);
    }

    #[test]
    fn total_memory_is_reported() {
        let mib = total_physical_mib().expect("Windows reports physical memory");
        assert!(mib >= 256, "{mib} MiB is implausibly little RAM");
    }
}
