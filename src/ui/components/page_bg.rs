//! Shared atmospheric page background: a whisper-quiet top teal wash, soft Gaussian crown halo,
//! and barely-visible fading architectural grid over `theme::BG`.
//!
//! Pure GPUI paint — no per-frame animation, no allocations in the paint pass, and calibrated low
//! alpha so every foreground surface and text token retains full WCAG contrast. Any screen can
//! drop `page_background()` as the first child of a `.relative().size_full().bg(rgb(theme::BG))`
//! container so content scrolls cleanly over it.

use gpui::{
    Bounds, BoxShadow, Div, ParentElement, Styled, canvas, div, fill, linear_color_stop,
    linear_gradient, point, px, rgb, size,
};

use crate::ui::theme;

/// Distance between architectural grid hairlines, in logical pixels.
const GRID_STEP: f32 = 48.0;
/// Vertical span over which the top grid fades to transparent, in logical pixels.
const GRID_FADE_H: f32 = 320.0;
/// Peak opacity of the grid hairlines at the very top edge (`~2.2%`).
const GRID_PEAK_ALPHA: f32 = 0.022;

/// Non-interactive backdrop layer for top-level screens.
///
/// Place as the first child inside a `.relative().size_full().bg(rgb(theme::BG)).overflow_hidden()`
/// wrapper, followed by the screen's scrollable `.relative().size_full()` content.
pub fn page_background() -> Div {
    let top_wash = div().absolute().top_0().left_0().right_0().h(px(280.)).bg(linear_gradient(
        180.,
        linear_color_stop(rgb(theme::BRAND).alpha(0.05), 0.0),
        linear_color_stop(rgb(theme::BRAND).alpha(0.0), 1.0),
    ));

    // A wide zero-height pill just above the top edge with a large Gaussian BoxShadow produces a
    // smooth, radial-feeling teal crown glow directly in GPUI's fragment shader.
    let crown_glow =
        div().absolute().top(px(-28.)).left_0().right_0().flex().justify_center().child(
            div().w(px(460.)).h(px(28.)).rounded_full().shadow(vec![BoxShadow {
                color: rgb(theme::BRAND).alpha(0.08).into(),
                offset: point(px(0.), px(12.)),
                blur_radius: px(150.),
                spread_radius: px(16.),
                inset: false,
            }]),
        );

    let subtle_grid = canvas(
        |_, _, _| (),
        |bounds, (), window, _| {
            let fade_h = f32::from(bounds.size.height).min(GRID_FADE_H);
            if fade_h <= 1.0 || f32::from(bounds.size.width) <= 1.0 {
                return;
            }
            let width = f32::from(bounds.size.width);
            let origin_x = f32::from(bounds.origin.x);
            let origin_y = f32::from(bounds.origin.y);

            // Vertical hairlines: each line is one GPU-dithered gradient quad fading down to 0.
            let v_grad = linear_gradient(
                180.,
                linear_color_stop(rgb(theme::FG).alpha(GRID_PEAK_ALPHA), 0.0),
                linear_color_stop(rgb(theme::FG).alpha(0.0), 1.0),
            );
            let mut x = GRID_STEP;
            while x < width {
                window.paint_quad(fill(
                    Bounds {
                        origin: point(px(origin_x + x), px(origin_y)),
                        size: size(px(1.0), px(fade_h)),
                    },
                    v_grad,
                ));
                x += GRID_STEP;
            }

            // Horizontal hairlines: opacity attenuates quadratically with depth so the grid
            // dissolves imperceptibly into `theme::BG`.
            let mut y = GRID_STEP;
            while y < fade_h {
                let t = 1.0 - (y / fade_h);
                let alpha = GRID_PEAK_ALPHA * t * t;
                if alpha > 0.002 {
                    window.paint_quad(fill(
                        Bounds {
                            origin: point(px(origin_x), px(origin_y + y)),
                            size: size(px(width), px(1.0)),
                        },
                        rgb(theme::FG).alpha(alpha),
                    ));
                }
                y += GRID_STEP;
            }
        },
    )
    .absolute()
    .inset_0();

    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .child(top_wash)
        .child(crown_glow)
        .child(subtle_grid)
}
