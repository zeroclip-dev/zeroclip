//! Lucide icons (<https://lucide.dev>, ISC licence), embedded in the exe.
//!
//! To add one: put `<name>.svg` from lucide-static in `assets/icons/`, add it to `ICONS`, then
//! call `icon("<name>", colour)`.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString, Styled, Svg, px, rgb, svg};

const ICONS: [(&str, &[u8]); 28] = [
    ("icons/zc-ring.svg", include_bytes!("../../assets/icons/zc-ring.svg")),
    ("icons/zc-play.svg", include_bytes!("../../assets/icons/zc-play.svg")),
    ("icons/check.svg", include_bytes!("../../assets/icons/check.svg")),
    ("icons/chevron-left.svg", include_bytes!("../../assets/icons/chevron-left.svg")),
    ("icons/chevron-right.svg", include_bytes!("../../assets/icons/chevron-right.svg")),
    ("icons/copy.svg", include_bytes!("../../assets/icons/copy.svg")),
    ("icons/download.svg", include_bytes!("../../assets/icons/download.svg")),
    ("icons/ellipsis.svg", include_bytes!("../../assets/icons/ellipsis.svg")),
    ("icons/film.svg", include_bytes!("../../assets/icons/film.svg")),
    ("icons/folder-open.svg", include_bytes!("../../assets/icons/folder-open.svg")),
    ("icons/gamepad-2.svg", include_bytes!("../../assets/icons/gamepad-2.svg")),
    ("icons/headphones.svg", include_bytes!("../../assets/icons/headphones.svg")),
    ("icons/house.svg", include_bytes!("../../assets/icons/house.svg")),
    ("icons/image.svg", include_bytes!("../../assets/icons/image.svg")),
    ("icons/minus.svg", include_bytes!("../../assets/icons/minus.svg")),
    ("icons/pause.svg", include_bytes!("../../assets/icons/pause.svg")),
    ("icons/play.svg", include_bytes!("../../assets/icons/play.svg")),
    ("icons/plus.svg", include_bytes!("../../assets/icons/plus.svg")),
    ("icons/save.svg", include_bytes!("../../assets/icons/save.svg")),
    ("icons/scissors.svg", include_bytes!("../../assets/icons/scissors.svg")),
    ("icons/settings.svg", include_bytes!("../../assets/icons/settings.svg")),
    ("icons/sliders-horizontal.svg", include_bytes!("../../assets/icons/sliders-horizontal.svg")),
    ("icons/square.svg", include_bytes!("../../assets/icons/square.svg")),
    ("icons/trash-2.svg", include_bytes!("../../assets/icons/trash-2.svg")),
    ("icons/volume-1.svg", include_bytes!("../../assets/icons/volume-1.svg")),
    ("icons/volume-2.svg", include_bytes!("../../assets/icons/volume-2.svg")),
    ("icons/volume-x.svg", include_bytes!("../../assets/icons/volume-x.svg")),
    ("icons/x.svg", include_bytes!("../../assets/icons/x.svg")),
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

/// A 16 px icon by file name, without folder or extension, in colour `color`. GPUI paints an SVG
/// with its own text colour, not the parent's, so the colour is given here.
pub fn icon(name: &str, color: u32) -> Svg {
    svg().path(format!("icons/{name}.svg")).size(px(16.)).flex_none().text_color(rgb(color))
}

/// The ZeroClip mark (`assets/brand/logo-mark.svg`) as two tinted layers on the app background,
/// since GPUI paints an SVG in one colour.
pub fn logo(size: f32) -> gpui::Div {
    use gpui::{ParentElement, div};
    let layer = |name: &str, color: u32| {
        svg().path(format!("icons/{name}.svg")).size(px(size)).flex_none().text_color(rgb(color))
    };
    div()
        .size(px(size))
        .flex_none()
        .relative()
        .rounded(px(size * 0.22))
        .bg(rgb(super::theme::BG))
        .child(layer("zc-ring", super::theme::BRAND))
        .child(div().absolute().top_0().left_0().child(layer("zc-play", super::theme::FG)))
}
