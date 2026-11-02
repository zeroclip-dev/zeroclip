//! Focus prompt (D35): what a focus change to another app means while a game is tracked.
//!
//! Pure decision logic. The engine owns the 3 s debounce and the banner state; this module says
//! what each focus change means, and which pairs have been asked about.

use std::time::Duration;

use crate::profile::exe_name;

/// How long a candidate must keep focus before the user is asked (D35), so alt-tab flicks don't
/// prompt.
pub const DEBOUNCE: Duration = Duration::from_secs(3);

/// The taskbar, Start menu and desktop belong to explorer.exe. They are never a candidate.
#[must_use]
pub fn is_shell(exe: &str) -> bool {
    exe_name(exe).eq_ignore_ascii_case("explorer.exe")
}

/// The app that took focus, as the decision needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focused<'a> {
    /// The executable: a bare file name or a full path.
    pub exe: &'a str,
    /// The profile set has an explicit profile for it.
    pub profiled: bool,
    /// The window covers its monitor, or is borderless on it (zc-input reports this).
    pub fullscreen: bool,
    /// ZeroClip's own window. Never prompts.
    pub ours: bool,
}

/// What a focus change does to the tracked game (D35).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusAction {
    /// Nothing changes: not a candidate, the tracked game itself, or a pair already asked.
    Ignore,
    /// Nothing is tracked and a profiled game took focus, so it is tracked at once (D33).
    Track,
    /// A candidate for a game other than the tracked one: ask once it keeps focus for
    /// [`DEBOUNCE`].
    Ask,
}

/// Decides what a focus change means. `tracked` is the tracked game's exe, if any, and
/// `already_asked` says the (tracked, candidate) pair has prompted this session.
#[must_use]
pub fn on_focus(tracked: Option<&str>, focused: Focused<'_>, already_asked: bool) -> FocusAction {
    if focused.ours || is_shell(focused.exe) || exe_name(focused.exe).is_empty() {
        return FocusAction::Ignore;
    }
    match tracked {
        None if focused.profiled => FocusAction::Track,
        None => FocusAction::Ignore,
        Some(current) if same_game(current, focused.exe) => {
            if focused.profiled {
                FocusAction::Track
            } else {
                FocusAction::Ignore
            }
        }
        Some(_) if already_asked => FocusAction::Ignore,
        Some(_) if focused.profiled || focused.fullscreen => FocusAction::Ask,
        Some(_) => FocusAction::Ignore,
    }
}

/// Whether two executables are the same game: equal file names, ignoring ASCII case.
#[must_use]
pub fn same_game(a: &str, b: &str) -> bool {
    exe_name(a).eq_ignore_ascii_case(exe_name(b))
}

/// The key a (current, candidate) pair is remembered by: lowercase file names.
#[must_use]
pub fn pair_key(current: &str, candidate: &str) -> (String, String) {
    (exe_name(current).to_ascii_lowercase(), exe_name(candidate).to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS2: &str = r"C:\Games\cs2.exe";
    const R6: &str = r"D:\Games\RainbowSix.exe";
    const DISCORD: &str = r"C:\Apps\Discord.exe";
    const GAME: &str = r"E:\Games\Windowed.exe";

    fn app(exe: &str) -> Focused<'_> {
        Focused { exe, profiled: false, fullscreen: false, ours: false }
    }

    fn profiled(exe: &str) -> Focused<'_> {
        Focused { profiled: true, ..app(exe) }
    }

    fn fullscreen(exe: &str) -> Focused<'_> {
        Focused { fullscreen: true, ..app(exe) }
    }

    #[test]
    fn focus_prompt_profiled_game_is_tracked_when_nothing_is() {
        assert_eq!(on_focus(None, profiled(CS2), false), FocusAction::Track);
    }

    #[test]
    fn focus_prompt_unprofiled_app_is_never_tracked() {
        assert_eq!(on_focus(None, app(DISCORD), false), FocusAction::Ignore);
        assert_eq!(on_focus(None, fullscreen(GAME), false), FocusAction::Ignore, "D33");
    }

    #[test]
    fn focus_prompt_same_game_is_tracked_again_not_asked() {
        assert_eq!(on_focus(Some("cs2.exe"), profiled(CS2), false), FocusAction::Track);
        assert_eq!(on_focus(Some(CS2), profiled(r"X:\cs2.exe"), false), FocusAction::Track);
        assert_eq!(on_focus(Some("cs2.exe"), app(CS2), false), FocusAction::Ignore);
    }

    #[test]
    fn focus_prompt_profiled_second_game_is_a_candidate() {
        assert_eq!(on_focus(Some(CS2), profiled(R6), false), FocusAction::Ask);
    }

    #[test]
    fn focus_prompt_fullscreen_unprofiled_app_is_a_candidate() {
        assert_eq!(on_focus(Some(CS2), fullscreen(GAME), false), FocusAction::Ask);
    }

    #[test]
    fn focus_prompt_windowed_unprofiled_app_never_prompts() {
        assert_eq!(on_focus(Some(CS2), app(DISCORD), false), FocusAction::Ignore);
    }

    #[test]
    fn focus_prompt_shell_and_own_window_never_prompt() {
        assert_eq!(
            on_focus(Some(CS2), app(r"C:\Windows\explorer.EXE"), false),
            FocusAction::Ignore
        );
        assert_eq!(
            on_focus(Some(CS2), fullscreen(r"C:\Windows\explorer.exe"), false),
            FocusAction::Ignore
        );
        let own = Focused { ours: true, ..fullscreen(GAME) };
        assert_eq!(on_focus(Some(CS2), own, false), FocusAction::Ignore);
    }

    #[test]
    fn focus_prompt_empty_exe_is_ignored() {
        assert_eq!(on_focus(Some(CS2), profiled(""), false), FocusAction::Ignore);
        assert_eq!(on_focus(None, profiled(""), false), FocusAction::Ignore);
    }

    #[test]
    fn focus_prompt_repeat_pair_does_not_ask_again() {
        assert_eq!(on_focus(Some(CS2), profiled(R6), true), FocusAction::Ignore);
        assert_eq!(on_focus(Some(CS2), fullscreen(GAME), true), FocusAction::Ignore);
    }

    #[test]
    fn focus_prompt_pair_key_ignores_case_and_path() {
        assert_eq!(pair_key(CS2, R6), ("cs2.exe".into(), "rainbowsix.exe".into()));
        assert_eq!(pair_key("CS2.EXE", "cs2.exe"), pair_key("cs2.exe", "CS2.exe"));
        assert!(same_game(CS2, "CS2.exe"));
        assert!(!same_game(CS2, R6));
    }
}
