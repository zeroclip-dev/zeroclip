//! Game profiles (DESIGN §7).
//!
//! When the foreground window changes, the input adapter reports the executable;
//! the engine resolves a profile for it and applies it: DVR window length, and
//! whether to start session recording automatically.

use serde::{Deserialize, Serialize};

use crate::time::Hns;

/// Settings applied while a particular game has focus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameProfile {
    /// Executable file name, e.g. `cs2.exe`. Matched case-insensitively.
    pub exe: String,
    /// How much footage the DVR buffer keeps, in seconds.
    pub buffer_secs: u32,
    /// Start session recording automatically while the game runs.
    pub session_recording: bool,
}

impl GameProfile {
    /// The DVR window as [`Hns`].
    #[must_use]
    pub fn buffer(&self) -> Hns {
        Hns::from_secs(i64::from(self.buffer_secs))
    }
}

/// All profiles, plus the fallback used for unknown executables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSet {
    /// Used when no profile matches.
    pub default: GameProfile,
    /// Per-game profiles.
    pub games: Vec<GameProfile>,
}

impl Default for ProfileSet {
    /// The examples from DESIGN §7.
    fn default() -> Self {
        Self {
            default: GameProfile { exe: "*".into(), buffer_secs: 60, session_recording: false },
            games: vec![
                GameProfile { exe: "cs2.exe".into(), buffer_secs: 120, session_recording: true },
                GameProfile {
                    exe: "RainbowSix.exe".into(),
                    buffer_secs: 300,
                    session_recording: false,
                },
            ],
        }
    }
}

/// The file name of an executable, given as a bare name or a full path.
///
/// Both `\` and `/` separate path components, on any OS, so a path copied from a
/// Windows machine works everywhere. Returns `exe` itself when it has no separator,
/// and an empty string for an empty input.
#[must_use]
pub fn exe_name(exe: &str) -> &str {
    exe.rsplit(['\\', '/']).next().unwrap_or(exe)
}

impl ProfileSet {
    /// The explicit profile for an executable, or `None` if no game profile matches.
    ///
    /// Matching is on the file name only and ignores ASCII case (Windows paths are
    /// case-insensitive). `None` means "no explicit profile", so the engine keeps the
    /// current profile in that case (sticky profiles, see P4.6).
    #[must_use]
    pub fn find(&self, exe: &str) -> Option<&GameProfile> {
        let name = exe_name(exe);
        if name.is_empty() {
            return None;
        }
        self.games.iter().find(|g| g.exe.eq_ignore_ascii_case(name))
    }

    /// The profile for an executable, given as a bare file name or a full path.
    ///
    /// Unknown executables get [`ProfileSet::default`]. See [`ProfileSet::find`]
    /// for the matching rules.
    #[must_use]
    pub fn resolve(&self, exe: &str) -> &GameProfile {
        self.find(exe).unwrap_or(&self.default)
    }

    /// Adds a profile, or replaces the existing one for the same executable
    /// (used to promote a game from the "recent games" list).
    pub fn upsert(&mut self, profile: GameProfile) {
        match self.games.iter_mut().find(|g| g.exe.eq_ignore_ascii_case(&profile.exe)) {
            Some(slot) => *slot = profile,
            None => self.games.push(profile),
        }
    }
}

/// The result of [`clamp_window`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowClamp {
    /// The window to actually use.
    pub window: Hns,
    /// Whether it was reduced to respect the RAM ceiling (tell the user).
    pub clamped: bool,
}

/// Limits a requested DVR window so the video ring fits under `ram_ceiling_bytes`
/// at `max_video_bitrate_bps` (DESIGN §7: "clamp it and tell the user rather than
/// silently allocating 2 GB").
#[must_use]
pub fn clamp_window(
    requested: Hns,
    max_video_bitrate_bps: u64,
    ram_ceiling_bytes: u64,
) -> WindowClamp {
    if max_video_bitrate_bps == 0 {
        // No bitrate means no estimate of the ring's size, so nothing to clamp against.
        return WindowClamp { window: requested, clamped: false };
    }
    // bytes * 8 bits / (bits per second) = seconds, then to ticks. The u128 holds
    // u64::MAX * 8 * 10^7 without overflow, and the quotient saturates into i64.
    let ticks_per_sec = u128::from(Hns::PER_SECOND.unsigned_abs());
    let max_ticks =
        u128::from(ram_ceiling_bytes) * 8 * ticks_per_sec / u128::from(max_video_bitrate_bps);
    let max = Hns(i64::try_from(max_ticks).unwrap_or(i64::MAX));
    if requested > max {
        WindowClamp { window: max, clamped: true }
    } else {
        WindowClamp { window: requested, clamped: false }
    }
}

/// The share of physical RAM the DVR's RAM part may use, in percent (D32).
const RAM_CEILING_PERCENT: u64 = 5;

/// The lowest ceiling the RAM share can set, in MiB. The settings check refuses less too.
const RAM_CEILING_FLOOR_MIB: u64 = 64;

/// The DVR RAM ceiling the engine runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamCeiling {
    /// The ceiling to use, in MiB.
    pub mib: u32,
    /// Whether the RAM share cut the setting down, so the UI can say where the number comes from.
    pub from_system_ram: bool,
}

/// The DVR RAM ceiling: the user's setting, capped at 5% of physical RAM and never below 64 MiB
/// (D32). With no RAM reading (`None`), the setting stands unchanged.
#[must_use]
pub fn effective_ram_ceiling(setting_mib: u32, total_physical_mib: Option<u64>) -> RamCeiling {
    let Some(total) = total_physical_mib else {
        return RamCeiling { mib: setting_mib, from_system_ram: false };
    };
    let cap = (total.saturating_mul(RAM_CEILING_PERCENT) / 100).max(RAM_CEILING_FLOOR_MIB);
    if u64::from(setting_mib) > cap {
        // `cap` is below the setting, which is a u32, so it always fits.
        RamCeiling { mib: u32::try_from(cap).unwrap_or(u32::MAX), from_system_ram: true }
    } else {
        RamCeiling { mib: setting_mib, from_system_ram: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profiles_match_the_design_doc() {
        let set = ProfileSet::default();
        assert_eq!(set.default.buffer(), Hns::from_secs(60));
        assert_eq!(set.games.len(), 2);
    }

    #[test]
    fn profile_resolves_by_file_name_ignoring_case() {
        let set = ProfileSet::default();
        assert_eq!(set.resolve("cs2.exe").buffer_secs, 120);
        assert_eq!(
            set.resolve(r"C:\Games\Counter-Strike 2\game\bin\win64\CS2.EXE").buffer_secs,
            120
        );
        assert_eq!(set.resolve("D:/Ubisoft/RainbowSix.exe").buffer_secs, 300);
    }

    #[test]
    fn profile_falls_back_to_the_default() {
        let set = ProfileSet::default();
        assert_eq!(set.resolve("notepad.exe"), &set.default);
        assert_eq!(set.resolve(""), &set.default);
        assert_eq!(set.resolve(r"C:\cs2.exe.bak"), &set.default, "no partial matches");
    }

    #[test]
    fn profile_find_is_none_for_unknown_exes() {
        let set = ProfileSet::default();
        assert!(set.find("notepad.exe").is_none());
        assert!(set.find("").is_none());
        assert!(set.find("CS2.EXE").is_some());
    }

    #[test]
    fn profile_upsert_replaces_by_exe() {
        let mut set = ProfileSet::default();
        set.upsert(GameProfile {
            exe: "CS2.exe".into(),
            buffer_secs: 90,
            session_recording: false,
        });
        assert_eq!(set.games.len(), 2, "replaced, not added");
        assert_eq!(set.resolve("cs2.exe").buffer_secs, 90);
        set.upsert(GameProfile {
            exe: "valorant.exe".into(),
            buffer_secs: 45,
            session_recording: false,
        });
        assert_eq!(set.games.len(), 3);
    }

    #[test]
    fn profile_clamp_keeps_windows_that_fit() {
        // 2 min at 20 Mbps = 300 MB, under a 1 GiB ceiling.
        let c = clamp_window(Hns::from_secs(120), 20_000_000, 1 << 30);
        assert_eq!(c, WindowClamp { window: Hns::from_secs(120), clamped: false });
    }

    #[test]
    fn profile_clamp_shrinks_windows_over_the_ceiling() {
        // 5 min at 20 Mbps = 750 MB > 500 MB ceiling → 500 MB / 2.5 MB/s = 200 s.
        let c = clamp_window(Hns::from_secs(300), 20_000_000, 500_000_000);
        assert!(c.clamped);
        assert_eq!(c.window, Hns::from_secs(200));
    }

    #[test]
    fn ram_ceiling_takes_five_percent_of_a_16_gb_machine() {
        // 16 GiB = 16384 MiB; 5% is 819 MiB, under the 1024 MiB setting.
        let c = effective_ram_ceiling(1_024, Some(16_384));
        assert_eq!(c, RamCeiling { mib: 819, from_system_ram: true });
    }

    #[test]
    fn ram_ceiling_takes_five_percent_of_a_4_gb_machine() {
        // 4 GiB = 4096 MiB; 5% is 204 MiB.
        let c = effective_ram_ceiling(1_024, Some(4_096));
        assert_eq!(c, RamCeiling { mib: 204, from_system_ram: true });
    }

    #[test]
    fn ram_ceiling_keeps_the_setting_when_it_is_under_the_cap() {
        // 32 GiB: 5% is 1638 MiB, above the 1024 MiB setting.
        let c = effective_ram_ceiling(1_024, Some(32_768));
        assert_eq!(c, RamCeiling { mib: 1_024, from_system_ram: false });
    }

    #[test]
    fn ram_ceiling_keeps_the_setting_without_a_ram_reading() {
        let c = effective_ram_ceiling(1_024, None);
        assert_eq!(c, RamCeiling { mib: 1_024, from_system_ram: false });
    }

    #[test]
    fn ram_ceiling_never_drops_below_64_mib() {
        // 100 MiB of RAM: 5% is 5 MiB, so the floor applies.
        let c = effective_ram_ceiling(1_024, Some(100));
        assert_eq!(c, RamCeiling { mib: 64, from_system_ram: true });
    }
}
