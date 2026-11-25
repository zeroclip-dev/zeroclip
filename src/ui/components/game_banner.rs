//! Hero game banner for the top of the Home screen.
//!
//! Displays the active or most recent game with offline artwork (Steam librarycache hero/header,
//! latest clip thumbnail, or exe icon over a teal-tinted charcoal gradient), a bottom-to-top
//! contrast scrim so foreground text stays WCAG AA legible over bright artwork, and a live status
//! chip (`Recording` with elapsed time, `Armed`, or `Idle`).

use gpui::{
    Div, FontWeight, ObjectFit, ParentElement, Styled, StyledImage as _, div, img,
    linear_color_stop, linear_gradient, px, rgb,
};
use zc_core::Hns;

use super::status_dot::status_dot;
use crate::ui::art::ArtSource;
use crate::ui::{icons, theme};

/// Fixed banner height in logical pixels: tall enough for hero art, compact enough that both the
/// banner and the Replay Buffer card stay visible inside the minimum `760x500` window.
pub const BANNER_HEIGHT: f32 = 168.0;

/// Live capture state shown in the banner's top-right status chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerStatus {
    /// Actively writing a recording file, with elapsed recording duration.
    Recording {
        /// Time elapsed since recording began.
        elapsed: Hns,
    },
    /// DVR buffer is capturing and ready to save on hotkey, with optional session duration.
    Armed {
        /// How long the current game session has been active, if a game is detected.
        elapsed: Option<Hns>,
    },
    /// Engine is idle (no active capture).
    Idle,
}

/// Content and artwork inputs for [`game_banner`].
pub struct GameBannerProps {
    /// Primary title (Steam display title, cleaned exe stem, or ready-state title).
    pub title: String,
    /// Quiet context eyebrow in the top-left pill (e.g. `"Now playing"`, `"Last played"`).
    pub eyebrow: &'static str,
    /// Live capture / recording state for the status chip.
    pub status: BannerStatus,
    /// Secondary metadata line (clip count today, session stats, or empty-state hint).
    pub meta: String,
    /// Optional compact counter shown on the bottom right (e.g. `"5 clips"`).
    pub badge_right: Option<String>,
    /// Resolved offline artwork, or `None` while still loading (renders the teal gradient fallback).
    pub art: Option<ArtSource>,
    /// True when no game has ever been detected or clipped yet (shows ZeroClip mark in fallback).
    pub empty_state: bool,
}

/// Renders the fixed-height game artwork banner.
pub fn game_banner(props: GameBannerProps) -> Div {
    let art = props.art.unwrap_or(ArtSource::Fallback);

    // Base fallback canvas: always painted underneath so there is zero flash while GPUI decodes
    // the image off-thread, and so the exe-icon / empty fallback feels rich and intentional.
    let base_gradient = div().absolute().inset_0().bg(linear_gradient(
        135.,
        linear_color_stop(rgb(theme::BRAND_SOFT), 0.0),
        linear_color_stop(rgb(theme::CARD), 0.72),
    ));
    let right_accent = div().absolute().inset_0().bg(linear_gradient(
        270.,
        linear_color_stop(rgb(theme::BRAND).alpha(0.09), 0.0),
        linear_color_stop(rgb(theme::BRAND).alpha(0.0), 0.58),
    ));

    let mut banner = div()
        .relative()
        .w_full()
        .h(px(BANNER_HEIGHT))
        .rounded(px(theme::RADIUS_LG))
        .overflow_hidden()
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(base_gradient)
        .child(right_accent);

    // Artwork layer for full-bleed Steam hero/header or cropped clip thumbnail.
    match &art {
        ArtSource::SteamHero(path) => {
            banner = banner.child(
                div()
                    .absolute()
                    .inset_0()
                    .child(img(path.clone()).size_full().object_fit(ObjectFit::Cover)),
            );
        }
        ArtSource::ClipThumb(path) => {
            // Clip frames can be visually busy; darken slightly before applying the bottom scrim.
            banner = banner
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(img(path.clone()).size_full().object_fit(ObjectFit::Cover)),
                )
                .child(div().absolute().inset_0().bg(rgb(theme::BG).alpha(0.36)));
        }
        ArtSource::ExeIcon(_) | ArtSource::Fallback => {}
    }

    // Bottom-to-top scrim (`0.` = CSS `to top`, so stop `0.0` sits at the bottom edge): BG at 92%
    // fading cleanly to transparent so the game title and metadata pass WCAG AA over any image.
    let bottom_scrim = div().absolute().inset_0().bg(linear_gradient(
        0.,
        linear_color_stop(rgb(theme::BG).alpha(0.92), 0.0),
        linear_color_stop(rgb(theme::BG).alpha(0.0), 0.86),
    ));
    // Gentle left-to-right vignette reinforces contrast behind the title and optional icon badge.
    let left_scrim = div().absolute().inset_0().bg(linear_gradient(
        90.,
        linear_color_stop(rgb(theme::BG).alpha(0.55), 0.0),
        linear_color_stop(rgb(theme::BG).alpha(0.0), 0.55),
    ));
    // Crisp 1 px top edge highlight gives the card tactile depth on dark charcoal.
    let top_highlight =
        div().absolute().top_0().left_0().right_0().h(px(1.)).bg(rgb(theme::FG).alpha(0.07));

    let eyebrow_pill = div()
        .h(px(22.))
        .px(px(9.))
        .flex()
        .flex_row()
        .items_center()
        .rounded_full()
        .bg(rgb(theme::BG).alpha(0.72))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .text_size(px(theme::TEXT_MICRO))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::FG_SOFT))
        .child(props.eyebrow);

    let icon_badge = match &art {
        ArtSource::ExeIcon(path) => Some(
            div()
                .size(px(44.))
                .flex_none()
                .p(px(5.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_MD))
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER_STRONG))
                .overflow_hidden()
                .child(img(path.clone()).size_full().object_fit(ObjectFit::Contain)),
        ),
        ArtSource::Fallback => {
            let glyph = if props.empty_state {
                icons::logo(26.)
            } else {
                div()
                    .size(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icons::icon("gamepad-2", theme::BRAND))
            };
            Some(
                div()
                    .size(px(44.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::BRAND_SOFT))
                    .border_1()
                    .border_color(rgb(theme::BRAND_BORDER))
                    .child(glyph),
            )
        }
        ArtSource::SteamHero(_) | ArtSource::ClipThumb(_) => None,
    };

    let mut bottom_row = div().flex().flex_row().items_end().justify_between().gap_4().child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .children(icon_badge)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(22.))
                            .line_height(px(26.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::FG))
                            .truncate()
                            .child(props.title),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_CAPTION))
                            .line_height(px(17.))
                            .text_color(rgb(theme::FG_SOFT))
                            .truncate()
                            .child(props.meta),
                    ),
            ),
    );

    if let Some(right) = props.badge_right {
        bottom_row = bottom_row.child(
            div()
                .flex_none()
                .px(px(9.))
                .py(px(3.))
                .rounded_full()
                .bg(rgb(theme::BG).alpha(0.72))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .text_size(px(theme::TEXT_MICRO))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::FG_SOFT))
                .child(right),
        );
    }

    let foreground = div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .justify_between()
        .p_5()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(eyebrow_pill)
                .child(status_chip(props.status)),
        )
        .child(bottom_row);

    banner.child(bottom_scrim).child(left_scrim).child(top_highlight).child(foreground)
}

fn status_chip(status: BannerStatus) -> Div {
    let (dot, fg, bg, border, label) = match status {
        BannerStatus::Recording { elapsed } => (
            theme::DANGER,
            theme::DANGER,
            theme::DANGER_SOFT,
            theme::DANGER_BORDER,
            format!("Recording \u{b7} {}", format_elapsed(elapsed)),
        ),
        BannerStatus::Armed { elapsed } => {
            let text = match elapsed {
                Some(t) if t.as_millis() >= 1_000 => {
                    format!("Armed \u{b7} {}", crate::ui::human_duration(t))
                }
                _ => "Armed".to_owned(),
            };
            (theme::BRAND, theme::BRAND, theme::BRAND_SOFT, theme::BRAND_BORDER, text)
        }
        BannerStatus::Idle => {
            (theme::TEXT_DIM, theme::MUTED_FG, theme::SURFACE_RAISED, theme::BORDER, "Idle".into())
        }
    };
    div()
        .h(px(24.))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .rounded_full()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .text_size(px(theme::TEXT_CAPTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(fg))
        .child(status_dot(rgb(dot)))
        .child(label)
}

/// Formats an elapsed recording duration as `MM:SS` (or `H:MM:SS` past one hour).
#[must_use]
pub fn format_elapsed(t: Hns) -> String {
    let secs = t.as_millis().max(0) / 1_000;
    let hours = secs / 3_600;
    let mins = (secs % 3_600) / 60;
    let rem = secs % 60;
    if hours > 0 { format!("{hours}:{mins:02}:{rem:02}") } else { format!("{mins:02}:{rem:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_formats_as_clock_counter() {
        assert_eq!(format_elapsed(Hns(-10)), "00:00");
        assert_eq!(format_elapsed(Hns::from_secs(5)), "00:05");
        assert_eq!(format_elapsed(Hns::from_secs(95)), "01:35");
        assert_eq!(format_elapsed(Hns::from_secs(3_661)), "1:01:01");
    }
}
