//! A horizontal slider: press or drag anywhere on the track. `on_change` gets the position as a
//! fraction `0.0..=1.0`; the caller maps it to a value (so it can be non-linear or snapped).
//!
//! The track's bounds are read back from the last paint, so the first frame can't turn a mouse
//! position into a fraction; keep one [`SliderState`] per slider and reuse it between renders.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, InteractiveElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Pixels, SharedString, Stateful, Styled, Window, canvas, div, px, relative, rgb,
};

use crate::ui::theme;

const THUMB: f32 = 14.0;

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
    let on_change = Rc::new(on_change);
    let fraction = fraction.clamp(0.0, 1.0);
    // Half a thumb of room each side, so the thumb at 0 and 1 stays inside the element.
    let track = div()
        .absolute()
        .left(px(THUMB / 2.0))
        .right(px(THUMB / 2.0))
        .top_0()
        .bottom_0()
        .child({
            let bounds = state.bounds.clone();
            canvas(move |b, _, _| bounds.set(b), |_, (), _, _| {}).absolute().size_full()
        })
        .child(
            div()
                .absolute()
                .top(px(8.0))
                .h(px(4.0))
                .w_full()
                .rounded(px(2.0))
                .bg(rgb(theme::INPUT)),
        )
        .child(
            div()
                .absolute()
                .top(px(8.0))
                .h(px(4.0))
                .w(relative(fraction))
                .rounded(px(2.0))
                .bg(rgb(theme::BRAND)),
        )
        .child(
            div()
                .absolute()
                .top(px(3.0))
                .left(relative(fraction))
                .ml(px(-THUMB / 2.0))
                .size(px(THUMB))
                .rounded_full()
                .bg(rgb(theme::PRIMARY)),
        );

    let (down, drag, drop, out) =
        (state.clone(), state.clone(), state.dragging.clone(), state.dragging.clone());
    let (on_down, on_drag) = (on_change.clone(), on_change);
    div()
        .id(id.into())
        .relative()
        // 200 px when there is room, shrinking to 110 in a narrow window.
        .w(px(200.0))
        .min_w(px(110.0))
        .h(px(20.0))
        .flex_shrink(1.)
        .cursor_pointer()
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
