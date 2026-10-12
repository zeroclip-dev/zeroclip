//! Drive-type detection (DESIGN §4, "Detect drive type at startup").

use std::path::Path;
use std::ptr;

use windows::Win32::Foundation::{ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};
use windows::core::{HSTRING, Owned, PCWSTR};
use zc_core::ports::SourceError;

use crate::error::{WinResultExt, platform_error};

/// Whether the drive holding `path` has a seek penalty, i.e. is a spinning disk.
///
/// `Ok(None)` when the device doesn't say (some RAID and USB bridges). Feed the
/// answer to `zc_mux::pacing::DriveKind::from_seek_penalty`. Also compare against
/// the game's drive and warn when they're the same spindle.
///
/// Three Windows calls, none of which needs admin rights:
///
/// 1. [`GetVolumePathNameW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumepathnamew)
///    finds the mount point of `path`, which may be a folder mount point.
/// 2. [`GetVolumeNameForVolumeMountPointW`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumenameforvolumemountpointw)
///    turns that into the volume GUID path. `CreateFileW` opens it with no access rights.
/// 3. [`IOCTL_STORAGE_QUERY_PROPERTY`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-ioctl_storage_query_property)
///    with `StorageDeviceSeekPenaltyProperty` fills a
///    [`DEVICE_SEEK_PENALTY_DESCRIPTOR`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-device_seek_penalty_descriptor).
///
/// # Errors
/// If the volume can't be opened or queried.
pub fn seek_penalty(path: &Path) -> Result<Option<bool>, SourceError> {
    let volume = volume_device_path(path)?;
    // Closed when `handle` drops, on every path out of this function.
    let handle = open_volume(&volume)?;
    query_seek_penalty(*handle)
}

/// The volume GUID path (`\\?\Volume{...}`) of the volume holding `path`, nul-terminated.
fn volume_device_path(path: &Path) -> Result<[u16; 64], SourceError> {
    let mut mount = [0_u16; 261];
    // SAFETY: `mount` is a writable buffer of MAX_PATH + 1 (261) u16s, the size the call
    // requires. The HSTRING holds a nul-terminated copy of `path` and lives for the call.
    unsafe { GetVolumePathNameW(&HSTRING::from(path), &mut mount) }
        .context("GetVolumePathNameW")?;

    let mut volume = [0_u16; 64];
    // SAFETY: `mount` holds the nul-terminated mount point written by the call above.
    // `volume` is a writable 64-u16 buffer, more than the 50 u16s a volume GUID path needs.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume) }
        .context("GetVolumeNameForVolumeMountPointW")?;

    let Some(end) = volume.iter().position(|&c| c == 0) else {
        return Err(SourceError::Other(
            "GetVolumeNameForVolumeMountPointW returned an unterminated path".to_owned(),
        ));
    };
    // The GUID path ends in a backslash. With it, CreateFileW opens the root directory;
    // without it, CreateFileW opens the volume device.
    if end > 0 && volume[end - 1] == u16::from(b'\\') {
        volume[end - 1] = 0;
    }
    Ok(volume)
}

/// Opens the volume device with no access rights, so no admin is needed.
fn open_volume(volume: &[u16; 64]) -> Result<Owned<HANDLE>, SourceError> {
    // SAFETY: `volume` is nul-terminated (checked in `volume_device_path`). Desired access
    // 0 asks for no rights, and sharing read and write leaves other handles alone.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(volume.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .context("CreateFileW")?;
    // SAFETY: `handle` was just returned by a successful CreateFileW and nothing else
    // holds it, so `Owned` is the only owner and closes it exactly once.
    Ok(unsafe { Owned::new(handle) })
}

/// Asks the volume whether it incurs a seek penalty. `handle` is an open volume.
fn query_seek_penalty(handle: HANDLE) -> Result<Option<bool>, SourceError> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut desc = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0_u32;
    // SAFETY: `query` and `desc` are initialised locals, and the sizes passed are exactly
    // their sizes, so the driver reads and writes only inside them. `returned` is a valid
    // u32 for `lpBytesReturned`. `handle` stays open for the call, and no overlapped I/O is
    // used, so nothing outlives this frame.
    let result = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(ptr::from_ref(&query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(ptr::from_mut(&mut desc).cast()),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&raw mut returned),
            None,
        )
    };
    match result {
        // A driver that fills less than the whole descriptor gave no usable answer.
        Ok(()) if returned as usize >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() => {
            Ok(Some(desc.IncursSeekPenalty))
        }
        Ok(()) => Ok(None),
        // RAID controllers and some USB bridges don't implement this property.
        Err(e)
            if e.code() == ERROR_INVALID_FUNCTION.to_hresult()
                || e.code() == ERROR_NOT_SUPPORTED.to_hresult() =>
        {
            Ok(None)
        }
        Err(e) => Err(platform_error("DeviceIoControl(IOCTL_STORAGE_QUERY_PROPERTY)", &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_drive_answers() {
        assert!(seek_penalty(&std::env::temp_dir()).is_ok());
    }
}
