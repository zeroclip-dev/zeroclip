//! Design tokens and shared typography/elevation helpers.
//!
//! Layered cool-charcoal surfaces (`BG` < `CARD` < `SURFACE_RAISED` < `ACCENT` < `SURFACE_ACTIVE`),
//! subtle low-contrast 1 px borders, a single refined teal brand hue (selected/active and safe
//! recording states only), and red reserved strictly for recording and destructive actions.
//! All colours live here so GPUI code uses `rgb(theme::X)` / `rgba(theme::X)` and never a raw hex.

use gpui::{
    BoxShadow, Div, FontWeight, ParentElement, SharedString, Styled, div, point, px, rgb, rgba,
};

// ---------------------------------------------------------------------------
// Surfaces (cool dark charcoal ladder)
// ---------------------------------------------------------------------------

/// Deepest window canvas.
pub const BG: u32 = 0x0b0b0e;
/// Recessed wells inside cards (text inputs, segmented tracks, steppers).
pub const SURFACE_INSET: u32 = 0x0e0e12;
/// Primary card and panel surface.
pub const CARD: u32 = 0x121216;
/// Elevated surfaces (floating menus, active pills, resting secondary buttons).
pub const SURFACE_RAISED: u32 = 0x18181e;
/// Quiet neutral fill (disabled controls, secondary chips).
pub const MUTED: u32 = 0x1a1a20;
/// Interactive hover surface (buttons, tabs, menu rows).
pub const ACCENT: u32 = 0x22222a;
/// Interactive pressed/active surface.
pub const SURFACE_ACTIVE: u32 = 0x2a2a34;

// ---------------------------------------------------------------------------
// Borders (subtle ~6-16% white-on-dark hairlines, expressed as solid hex)
// ---------------------------------------------------------------------------

/// Whisper-quiet hairline (shell chrome dividers, field rows, menu rules).
pub const BORDER_SUBTLE: u32 = 0x1e1e25;
/// Standard 1 px surface border (cards, pills).
pub const BORDER: u32 = 0x26262f;
/// Control and input resting border.
pub const INPUT: u32 = 0x30303b;
/// Emphasised border (floating menus, hovered inputs and outline buttons).
pub const BORDER_STRONG: u32 = 0x3e3e4c;
/// Keyboard focus-visible ring and focused input border.
pub const RING: u32 = 0x2cc4b2;

// ---------------------------------------------------------------------------
// Text hierarchy
// ---------------------------------------------------------------------------

/// Primary text (headings, values, active labels).
pub const FG: u32 = 0xf3f3f6;
/// Secondary readable text (field labels, resting ghost buttons, body copy).
pub const FG_SOFT: u32 = 0xc6c6d0;
/// Tertiary text (descriptions, inactive tabs, placeholders).
pub const MUTED_FG: u32 = 0x8a8a98;
/// Quietest metadata (status bar labels/separators, version stamp, disabled text).
pub const TEXT_DIM: u32 = 0x60606d;

// ---------------------------------------------------------------------------
// Primary CTA (high-contrast light surface on dark)
// ---------------------------------------------------------------------------

pub const PRIMARY: u32 = 0xf3f3f6;
pub const PRIMARY_HOVER: u32 = 0xe2e2e8;
pub const PRIMARY_ACTIVE: u32 = 0xcfcfd8;
pub const PRIMARY_FG: u32 = 0x0b0b0e;

// ---------------------------------------------------------------------------
// Brand & semantic hues
// ---------------------------------------------------------------------------

/// Single teal brand hue: active/selected indicators and recording-safe states only.
pub const BRAND: u32 = 0x2dd4bf;
/// Soft dark-teal fill (buffering/capturing status pill, subtle active tint).
pub const BRAND_SOFT: u32 = 0x102928;
/// Subtle teal hairline border paired with `BRAND_SOFT`.
pub const BRAND_BORDER: u32 = 0x1d4b48;

/// Caution / non-modal notice accent.
pub const WARN: u32 = 0xf5b82e;
/// Calm dark-amber fill for non-modal banners.
pub const WARN_SOFT: u32 = 0x221b0c;
/// Subtle amber hairline border paired with `WARN_SOFT`.
pub const WARN_BORDER: u32 = 0x453514;

/// Recording and destructive actions ONLY.
pub const DANGER: u32 = 0xf25f5c;
/// Soft dark-crimson fill (recording status pill, destructive hover).
pub const DANGER_SOFT: u32 = 0x2a1417;
/// Subtle crimson hairline border paired with `DANGER_SOFT`.
pub const DANGER_BORDER: u32 = 0x522227;

// ---------------------------------------------------------------------------
// Shadows & scrims (`0xRRGGBBAA` for `gpui::rgba`)
// ---------------------------------------------------------------------------

/// Primary drop-shadow colour for floating menus.
pub const SHADOW_COLOR: u32 = 0x00000088;
/// Ambient halo shadow colour for floating menus.
pub const SHADOW_AMBIENT: u32 = 0x00000044;
/// Translucent dark scrim for media overlays.
pub const SCRIM: u32 = 0x000000a6;
/// Pure black letterbox canvas behind video playback.
pub const MEDIA_BG: u32 = 0x000000;

// ---------------------------------------------------------------------------
// Geometry & typography scale (4 / 8 px rhythm)
// ---------------------------------------------------------------------------

/// Corner radii in px: large panels, cards, controls, small inner chips.
pub const RADIUS_LG: f32 = 12.0;
pub const RADIUS: f32 = 10.0;
pub const RADIUS_MD: f32 = 8.0;
pub const RADIUS_SM: f32 = 6.0;

/// Standard control height (`32 px`) and primary hero CTA height (`36 px`).
pub const CONTROL_H: f32 = 32.0;
pub const CONTROL_H_LG: f32 = 36.0;

/// Type scale in px: page titles, section headings, body/controls, captions, micro metadata.
pub const TEXT_TITLE: f32 = 20.0;
pub const TEXT_HEADING: f32 = 14.0;
pub const TEXT_BODY: f32 = 13.0;
pub const TEXT_CAPTION: f32 = 12.0;
pub const TEXT_MICRO: f32 = 11.0;

/// Page-level title with tight leading.
pub fn page_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(TEXT_TITLE))
        .line_height(px(26.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(FG))
        .child(text.into())
}

/// Section or card heading.
pub fn section_heading(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(TEXT_HEADING))
        .line_height(px(20.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(FG))
        .child(text.into())
}

/// Quiet secondary caption or helper copy.
pub fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(TEXT_CAPTION))
        .line_height(px(17.))
        .text_color(rgb(MUTED_FG))
        .child(text.into())
}

/// Soft elevation shadow reserved for floating menus and popovers.
pub fn floating_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: rgba(SHADOW_COLOR).into(),
            offset: point(px(0.), px(10.)),
            blur_radius: px(28.),
            spread_radius: px(-4.),
            inset: false,
        },
        BoxShadow {
            color: rgba(SHADOW_AMBIENT).into(),
            offset: point(px(0.), px(2.)),
            blur_radius: px(8.),
            spread_radius: px(0.),
            inset: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// Editor & timeline tokens
// ---------------------------------------------------------------------------

/// Dark letterbox stage behind video playback.
pub const VIDEO_BG: u32 = 0x050507;
/// Alternating kept-piece fill paired with `BRAND_SOFT` so neighbours read as separate.
pub const PIECE_ALT: u32 = 0x16212e;
/// Brightened brand teal for hovered trim handles.
pub const BRAND_HOVER: u32 = 0x5eead4;
/// Muted neutral fill for trimmed-off timeline ghosts.
pub const GHOST_BG: u32 = 0x15151a;

// ---------------------------------------------------------------------------
// Video player scrubber tokens
// ---------------------------------------------------------------------------

/// Loaded/buffered track fill behind the played progress bar.
pub const SCRUB_BUFFERED: u32 = 0x3b3b4a;
/// Hover-preview fill showing where a click on the scrubber will seek.
pub const SCRUB_PREVIEW: u32 = 0x56566a;
