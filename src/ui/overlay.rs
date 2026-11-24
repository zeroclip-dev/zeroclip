//! The toast overlay: a small card in the bottom-left corner that says what just happened
//! ("Clipped last 20 min"). It lives in its own borderless window. `zc_win::overlay` makes that
//! window click-through, non-activating and excluded from capture, so it never shows in a
//! recording or a screenshot.
//!
//! Limit: borderless and windowed games only. An exclusive-fullscreen game draws over the
//! overlay, so the card is not visible there.

mod model;

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    Animation, AnimationExt, App, AppContext, Bounds, Context, FontWeight, IntoElement,
    ParentElement, Render, Styled, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, div, ease_in_out, point, px, rgb, rgba,
    size,
};

pub use model::ToastKind;
use model::{CARD_H, CARD_W, FADE, HOLD, Rect, SHIMMER, SHIMMER_BAND};

use crate::ui::components::status_dot::status_dot;
use crate::ui::theme;

/// Gives each toast window a unique title, so `zc_win` finds exactly that window.
static NEXT_TITLE: AtomicU64 = AtomicU64::new(0);

/// The card's content. It is rendered in the toast window.
pub struct ToastView {
    kind: ToastKind,
    /// Set after the hold; the card then fades out.
    closing: bool,
    /// Whether to play the shimmer. Off when the user has turned animations off.
    shimmer: bool,
}

impl Render for ToastView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let text = model::toast_text(self.kind);
        let card = div()
            .relative()
            .overflow_hidden()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .px_4()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .rounded(px(theme::RADIUS))
            .bg(rgb(theme::CARD))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .text_color(rgb(theme::FG))
            .child(status_dot(rgb(model::accent(self.kind))))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(text.title))
                    .children(
                        text.detail
                            .map(|d| div().text_sm().text_color(rgb(theme::MUTED_FG)).child(d)),
                    ),
            );
        // One sweep when the card appears, then nothing: no looping motion.
        let card = if self.shimmer {
            card.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .w(px(SHIMMER_BAND))
                    .bg(rgba(0xffffff14))
                    .with_animation(
                        "toast-shimmer",
                        Animation::new(SHIMMER).with_easing(ease_in_out),
                        |el, t| el.left(px(model::shimmer_left(t, CARD_W, SHIMMER_BAND))),
                    ),
            )
        } else {
            card
        };
        if self.closing {
            card.with_animation(
                "toast-out",
                Animation::new(FADE).with_easing(ease_in_out),
                |el, t| el.opacity(1.0 - t),
            )
            .into_any_element()
        } else {
            card.with_animation(
                "toast-in",
                Animation::new(FADE).with_easing(ease_in_out),
                |el, t| el.opacity(t),
            )
            .into_any_element()
        }
    }
}

/// Opens a toast for `kind` in the bottom-left of the primary display and starts its timer:
/// hold, fade out, close. Returns `None` if the window can't be opened or can't be excluded
/// from capture. In that case nothing is shown.
pub fn show(cx: &mut App, kind: ToastKind) -> Option<WindowHandle<ToastView>> {
    let title = format!("ZeroClip toast {}", NEXT_TITLE.fetch_add(1, Ordering::Relaxed));
    let area = cx
        .primary_display()
        .map(|display| {
            let b = display.bounds();
            Rect {
                x: f32::from(b.origin.x),
                y: f32::from(b.origin.y),
                w: f32::from(b.size.width),
                h: f32::from(b.size.height),
            }
        })
        .unwrap_or(Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 });
    let (x, y) = model::bottom_left_origin(area, (CARD_W, CARD_H));
    // The card is created hidden and only shown once `zc_win` has excluded it from capture.
    let shimmer = !cx.reduce_motion() && zc_win::overlay::system_animations_enabled();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(x), px(y)),
            size(px(CARD_W), px(CARD_H)),
        ))),
        titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
        focus: false,
        show: false,
        // PopUp is borderless and top-most, and is a tool window (no taskbar button).
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let handle = match cx
        .open_window(options, move |_, cx| cx.new(|_| ToastView { kind, closing: false, shimmer }))
    {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(%error, "couldn't open the toast window");
            return None;
        }
    };
    if let Err(error) = zc_win::overlay::prepare_toast(&title) {
        tracing::warn!(%error, "toast not shown: it can't be excluded from capture");
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        return None;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(HOLD).await;
        // A window closed by the user or the app makes `update` fail; that is fine.
        let _ = handle.update(cx, |view, _, cx| {
            view.closing = true;
            cx.notify();
        });
        cx.background_executor().timer(FADE).await;
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    })
    .detach();
    Some(handle)
}
