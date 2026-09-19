//! Drive-type detection (DESIGN §4, "Detect drive type at startup").

use std::path::Path;

use zc_core::ports::SourceError;

/// Whether the drive holding `path` has a seek penalty, i.e. is a spinning disk.
///
/// `Ok(None)` when the device doesn't say (some RAID and USB bridges). Feed the
/// answer to `zc_mux::pacing::DriveKind::from_seek_penalty`. Also compare against
/// the game's drive and warn when they're the same spindle.
///
/// # Errors
/// If the volume can't be opened or queried.
pub fn seek_penalty(path: &Path) -> Result<Option<bool>, SourceError> {
    let _ = path;
    todo!(
        "Phase 3: GetVolumePathNameW(path) → open \\\\.\\X: with CreateFileW (no access \
         rights needed) → DeviceIoControl(IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY \
         {{ StorageDeviceSeekPenaltyProperty, PropertyStandardQuery }}) → \
         DEVICE_SEEK_PENALTY_DESCRIPTOR.IncursSeekPenalty"
    )
}
