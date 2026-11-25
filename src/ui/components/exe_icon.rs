//! A program's icon, cached as a PNG the first time it is seen running. The game list shows it
//! beside the executable name; a game that has never run on this machine shows none.

use std::path::{Path, PathBuf};

use gpui::{IntoElement, ObjectFit, ParentElement, Styled, StyledImage, div, img, px, rgb};
use zc_core::profile::exe_name;

use crate::ui::theme;

/// Where the icon of `exe` is (or will be) cached, or `None` for a name that isn't a plain file
/// name.
pub fn cached(dir: &Path, exe: &str) -> Option<PathBuf> {
    let name = exe_name(exe).to_lowercase();
    let plain = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '));
    plain.then(|| dir.join(format!("{name}.png")))
}

/// Saves the icon of the running program `exe`, unless it is cached. Returns whether it is now.
pub fn capture(dir: &Path, exe: &str) -> bool {
    let Some(target) = cached(dir, exe) else { return false };
    if target.is_file() {
        return true;
    }
    let Some(path) = zc_win::process::exe_path(exe) else { return false };
    let Some(icon) = zc_win::icon::icon_of(Path::new(&path)) else { return false };
    std::fs::create_dir_all(dir).is_ok()
        && image::save_buffer(
            &target,
            &icon.rgba,
            icon.width,
            icon.height,
            image::ExtendedColorType::Rgba8,
        )
        .is_ok()
}

/// Finds an existing non-empty cached icon PNG for `game`, trying both the raw name and with a
/// `.exe` suffix (clip game tags strip `.exe`, whereas profiles cache icons as `<name>.exe.png`).
pub fn find_cached(dir: &Path, game: &str) -> Option<PathBuf> {
    let trimmed = game.trim();
    if trimmed.is_empty() {
        return None;
    }
    let has_png = |path: &Path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0);
    if let Some(exact) = cached(dir, trimmed).filter(|p| has_png(p)) {
        return Some(exact);
    }
    if !trimmed.to_ascii_lowercase().ends_with(".exe") {
        let with_exe = format!("{trimmed}.exe");
        if let Some(path) = cached(dir, &with_exe).filter(|p| has_png(p)) {
            return Some(path);
        }
    }
    None
}

/// The icon at `file` inside a crisp 24 px rounded frame.
pub fn view(file: PathBuf) -> impl IntoElement {
    div()
        .size(px(24.0))
        .flex_none()
        .p(px(2.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_SM))
        .bg(rgb(theme::SURFACE_RAISED))
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .overflow_hidden()
        .child(img(file).size_full().object_fit(ObjectFit::Contain))
}

/// Compact 16 px icon for inline chips and pills.
pub fn chip_view(file: PathBuf) -> impl IntoElement {
    div()
        .size(px(16.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .overflow_hidden()
        .child(img(file).size_full().object_fit(ObjectFit::Contain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_cached_matches_both_raw_and_exe_suffixed_icons() {
        let dir = std::env::temp_dir().join(format!("zc-exe-icon-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp icon dir");

        let cs2 = dir.join("cs2.exe.png");
        let empty = dir.join("empty.exe.png");
        let raw = dir.join("valorant.png");
        std::fs::write(&cs2, b"png").expect("write cs2");
        std::fs::write(&empty, b"").expect("write empty");
        std::fs::write(&raw, b"png").expect("write valorant");

        assert_eq!(find_cached(&dir, "cs2"), Some(cs2.clone()));
        assert_eq!(find_cached(&dir, "CS2.EXE"), Some(cs2));
        assert_eq!(find_cached(&dir, "Valorant"), Some(raw));
        assert_eq!(find_cached(&dir, "empty"), None, "zero-byte files are ignored");
        assert_eq!(find_cached(&dir, "missing"), None);
        assert_eq!(find_cached(&dir, ""), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
