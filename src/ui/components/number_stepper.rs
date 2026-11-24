//! A number with minus/plus buttons. A click moves by `step`, Shift-click by 10 steps. The value
//! is clamped to `min..=max`. `on_change` gets the new value; wire it with `cx.listener`.

use gpui::{
    App, ElementId, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px, rgb,
};

use crate::ui::theme;

pub struct Stepper {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// Shown after the number, e.g. " kbps".
    pub suffix: &'static str,
    pub enabled: bool,
}

pub fn number_stepper(
    id: impl Into<SharedString>,
    value: f64,
    s: Stepper,
    on_change: impl Fn(&f64, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let id: SharedString = id.into();
    let on_change = std::rc::Rc::new(on_change);
    let button = |sub: &'static str, sign: f64, label: &'static str| {
        let on_change = on_change.clone();
        let (min, max, step) = (s.min, s.max, s.step);
        let b = div()
            .id(ElementId::Name(format!("{id}-{sub}").into()))
            .w(px(26.))
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::INPUT))
            .child(label);
        if s.enabled {
            b.cursor_pointer().hover(|h| h.bg(rgb(theme::ACCENT))).on_click(
                move |ev, window, cx| {
                    let k = if ev.modifiers().shift { 10.0 } else { 1.0 };
                    on_change(&(value + sign * step * k).clamp(min, max), window, cx);
                },
            )
        } else {
            b
        }
    };
    let text = if value.fract() == 0.0 { format!("{value:.0}") } else { format!("{value:.1}") };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .opacity(if s.enabled { 1.0 } else { 0.4 })
        .child(button("dec", -1.0, "\u{2212}"))
        .child(div().min_w(px(100.)).flex().justify_center().child(format!("{text}{}", s.suffix)))
        .child(button("inc", 1.0, "+"))
}
