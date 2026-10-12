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
}
