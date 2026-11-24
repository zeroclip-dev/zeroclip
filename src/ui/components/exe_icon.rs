//! A program's icon, cached as a PNG the first time it is seen running. The game list shows it
//! beside the executable name; a game that has never run on this machine shows none.

use std::path::{Path, PathBuf};

use gpui::{IntoElement, ObjectFit, Styled, StyledImage, img, px};
use zc_core::profile::exe_name;

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

/// The icon at `file` as a 24 px square.
pub fn view(file: PathBuf) -> impl IntoElement {
    img(file).size(px(24.0)).flex_none().object_fit(ObjectFit::Contain)
}
