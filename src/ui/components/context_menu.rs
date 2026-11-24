//! A right-click menu: a small card at the pointer, over everything, closed by a click anywhere
//! outside it. The owning view keeps `Option<(target, Point<Pixels>)>` for where it was opened,
//! sets it from `on_mouse_down(MouseButton::Right, ..)`, and renders [`context_menu`] while it is
//! `Some`. Items are [`menu_item`]s with `.on_click(cx.listener(..))`, and each closes the menu.

use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Point, SharedString,
    Styled, Window, anchored, deferred, div, point, px, rgb,
};

use crate::ui::theme;

const WIDTH: f32 = 230.0;
const ITEM_HEIGHT: f32 = 30.0;
/// Space kept between the menu and the window edge.
const MARGIN: f32 = 8.0;

/// One row. `danger` paints it red (delete).
pub fn menu_item(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(ITEM_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .rounded(px(theme::RADIUS_SM))
        .text_sm()
        .cursor_pointer()
        .text_color(rgb(if danger { theme::DANGER } else { theme::FG }))
        .hover(|s| s.bg(rgb(theme::ACCENT)))
        .child(label.into())
}

/// A thin rule between groups of items.
pub fn menu_separator() -> gpui::Div {
    div().h(px(1.)).my_1().bg(rgb(theme::BORDER))
}

/// The menu, placed at `at` (window coordinates) and kept inside the window. `rows` is the number
/// of items plus separators, for the height. `on_close` runs on a click outside the menu.
pub fn context_menu(
    window: &Window,
    at: Point<Pixels>,
    rows: usize,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
    items: impl IntoIterator<Item = gpui::AnyElement>,
) -> impl IntoElement {
    let view = window.viewport_size();
    let height = rows as f32 * ITEM_HEIGHT + 2.0 * MARGIN;
    let x = f32::from(at.x).min(f32::from(view.width) - WIDTH - MARGIN).max(MARGIN);
    let y = f32::from(at.y).min(f32::from(view.height) - height).max(MARGIN);
    let on_close = std::rc::Rc::new(on_close);
    let close_left = std::rc::Rc::clone(&on_close);
    let menu = div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(WIDTH))
        .p_1()
        .flex()
        .flex_col()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::CARD))
        .border_1()
        .border_color(rgb(theme::INPUT))
        .shadow_lg()
        // Clicks inside must not reach the backdrop, which closes the menu.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .children(items);
    // A window-sized layer drawn above everything; the menu sits in it, the rest is the backdrop.
    deferred(
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .w(view.width)
                .h(view.height)
                .relative()
                .on_mouse_down(MouseButton::Left, move |_, window, cx| close_left(window, cx))
                .on_mouse_down(MouseButton::Right, move |_, window, cx| on_close(window, cx))
                .child(menu),
        ),
    )
}
