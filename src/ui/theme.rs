//! Design tokens. These mirror the dark `:root[data-theme=dark]` tokens of the shadcn-style design
//! preview (zinc greys, one teal brand hue, red only for recording and errors), ported 1:1 so
//! GPUI code uses `rgb(theme::X)` and never a raw hex. The app is dark-only for now.

pub const BG: u32 = 0x09090b;
pub const FG: u32 = 0xfafafa;
pub const CARD: u32 = 0x101012;
pub const MUTED: u32 = 0x27272a;
pub const MUTED_FG: u32 = 0xa1a1aa;
pub const BORDER: u32 = 0x27272a;
pub const INPUT: u32 = 0x3f3f46;
pub const RING: u32 = 0x71717a;
pub const PRIMARY: u32 = 0xfafafa;
pub const PRIMARY_FG: u32 = 0x18181b;
pub const ACCENT: u32 = 0x1f1f23;
pub const BRAND: u32 = 0x2dd4bf;
#[allow(dead_code)] // token mirrors the preview; banners and status pill use the soft fills
pub const BRAND_SOFT: u32 = 0x0f2f2c;
pub const WARN: u32 = 0xfbbf24;
pub const WARN_SOFT: u32 = 0x2a2108;
pub const DANGER: u32 = 0xf87171;
#[allow(dead_code)]
pub const DANGER_SOFT: u32 = 0x301212;

/// Corner radii in px: cards, controls, small chips (`--r`, `--r-md`, `--r-sm`).
pub const RADIUS: f32 = 10.0;
pub const RADIUS_MD: f32 = 8.0;
pub const RADIUS_SM: f32 = 6.0;
