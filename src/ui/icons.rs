//! Lucide icons (<https://lucide.dev>, ISC licence), embedded in the exe.
//!
//! To add one: put `<name>.svg` from lucide-static in `assets/icons/`, add it to `ICONS`, then
//! call `icon("<name>")`. The colour comes from the parent's `text_color`.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString, Styled, Svg, px, svg};

const ICONS: [(&str, &[u8]); 11] = [
    ("icons/house.svg", include_bytes!("../../assets/icons/house.svg")),
    ("icons/film.svg", include_bytes!("../../assets/icons/film.svg")),
    ("icons/sliders-horizontal.svg", include_bytes!("../../assets/icons/sliders-horizontal.svg")),
    ("icons/scissors.svg", include_bytes!("../../assets/icons/scissors.svg")),
    ("icons/settings.svg", include_bytes!("../../assets/icons/settings.svg")),
    ("icons/play.svg", include_bytes!("../../assets/icons/play.svg")),
    ("icons/save.svg", include_bytes!("../../assets/icons/save.svg")),
    ("icons/minus.svg", include_bytes!("../../assets/icons/minus.svg")),
    ("icons/square.svg", include_bytes!("../../assets/icons/square.svg")),
    ("icons/x.svg", include_bytes!("../../assets/icons/x.svg")),
    ("icons/folder-open.svg", include_bytes!("../../assets/icons/folder-open.svg")),
];

/// The app's asset source; pass it to `Application::with_assets`.
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(p, _)| *p == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().map(|(p, _)| SharedString::from(*p)).collect())
    }
}

/// A 16 px icon by file name, without folder or extension.
pub fn icon(name: &str) -> Svg {
    svg().path(format!("icons/{name}.svg")).size(px(16.)).flex_none()
}
