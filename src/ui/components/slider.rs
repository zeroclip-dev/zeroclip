//! A horizontal slider: press or drag anywhere on the track. `on_change` gets the position as a
//! fraction `0.0..=1.0`; the caller maps it to a value (so it can be non-linear or snapped).
//!
//! The track's bounds are read back from the last paint, so the first frame can't turn a mouse
//! position into a fraction; keep one [`SliderState`] per slider and reuse it between renders.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, InteractiveElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Pixels, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, canvas, div, px,
    relative, rgb, transparent_black,
};

use crate::ui::theme;

const THUMB: f32 = 14.0;
const TRACK_H: f32 = 4.0;
/// Inner height inside the 32 px control after the 1 px top and bottom focus-ring borders.
const INNER_H: f32 = theme::CONTROL_H - 2.0;
/// Horizontal inset so the circular thumb stays clear of the focus ring at 0% and 100%.
const TRACK_INSET: f32 = THUMB / 2.0 + 4.0;

/// What a slider keeps between frames: where its track was painted, and whether it is being dragged.
#[derive(Clone, Default)]
pub struct SliderState {
    bounds: Rc<Cell<Bounds<Pixels>>>,
    dragging: Rc<Cell<bool>>,
}

impl SliderState {
    /// `x` (window pixels) as a fraction of the track, clamped to `0.0..=1.0`.
    fn fraction_at(&self, x: Pixels) -> f32 {
        let b = self.bounds.get();
        let width = f32::from(b.size.width);
        if width <= 0.0 { 0.0 } else { (f32::from(x - b.origin.x) / width).clamp(0.0, 1.0) }
    }
}

pub fn slider(
    id: impl Into<SharedString>,
    fraction: f32,
    state: &SliderState,
    on_change: impl Fn(f32, &mut Window, &mut App) + 'static,
) -> Stateful<gpui::Div> {
    let id: SharedString = id.into();
    let group_id = id.clone();
    let on_change = Rc::new(on_change);
    let fraction = fraction.clamp(0.0, 1.0);
    let dragging = state.dragging.get();
    let track_top = (INNER_H - TRACK_H) / 2.0;
    let thumb_top = (INNER_H - THUMB) / 2.0;

    let rail = div()
        .absolute()
        .top(px(track_top))
        .h(px(TRACK_H))
        .w_full()
        .rounded_full()
        .bg(rgb(theme::INPUT))
        .group_hover(group_id.clone(), |s| s.bg(rgb(theme::BORDER_STRONG)));

    let fill = div()
        .absolute()
        .top(px(track_top))
        .h(px(TRACK_H))
        .w(relative(fraction))
        .rounded_full()
        .bg(rgb(theme::BRAND));

    let thumb = div()
        .absolute()
        .top(px(thumb_top))
        .left(relative(fraction))
        .ml(px(-THUMB / 2.0))
        .size(px(THUMB))
        .rounded_full()
        .border_1()
        .bg(rgb(if dragging { theme::PRIMARY_ACTIVE } else { theme::PRIMARY }))
        .border_color(rgb(if dragging { theme::RING } else { theme::BORDER_STRONG }))
        .group_hover(group_id.clone(), |s| {
            s.bg(rgb(theme::PRIMARY_HOVER)).border_color(rgb(theme::RING))
        });

    // Room each side so the thumb at 0 and 1 stays inside the element and its focus ring.
    let track = div()
        .absolute()
        .left(px(TRACK_INSET))
        .right(px(TRACK_INSET))
        .top_0()
        .bottom_0()
        .child({
            let bounds = state.bounds.clone();
            canvas(move |b, _, _| bounds.set(b), |_, (), _, _| {}).absolute().size_full()
        })
        .child(rail)
        .child(fill)
        .child(thumb);

    let (down, drag, drop, out) =
        (state.clone(), state.clone(), state.dragging.clone(), state.dragging.clone());
    let (on_down, on_drag) = (on_change.clone(), on_change);
    div()
        .id(id)
        .group(group_id)
        .relative()
        .tab_stop(true)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .rounded(px(theme::RADIUS_MD))
        // 200 px when there is room, shrinking to 110 in a narrow window.
        .w(px(200.0))
        .min_w(px(110.0))
        .h(px(theme::CONTROL_H))
        .flex_shrink(1.)
        .cursor_pointer()
        .active(|s| s.opacity(0.92))
        .child(track)
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
            down.dragging.set(true);
            on_down(down.fraction_at(ev.position.x), window, cx);
        })
        .on_mouse_move(move |ev: &MouseMoveEvent, window, cx| {
            if drag.dragging.get() && ev.dragging() {
                on_drag(drag.fraction_at(ev.position.x), window, cx);
            }
        })
        .on_mouse_up(MouseButton::Left, move |_, _, _| drop.set(false))
        .on_mouse_up_out(MouseButton::Left, move |_, _, _| out.set(false))
}
