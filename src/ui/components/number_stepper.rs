//! A number with minus/plus buttons. A click moves by `step`, Shift-click by 10 steps. The value
//! is clamped to `min..=max`. `on_change` gets the new value; wire it with `cx.listener`.

use gpui::{
    App, ElementId, FontWeight, InteractiveElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, rgb,
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
    let button = |sub: &'static str, sign: f64, label: &'static str, aria: &'static str| {
        let on_change = on_change.clone();
        let (min, max, step) = (s.min, s.max, s.step);
        let at_limit = (sign < 0.0 && value <= min) || (sign > 0.0 && value >= max);
        let b = div()
            .id(ElementId::Name(format!("{id}-{sub}").into()))
            .tab_stop(true)
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .role(Role::Button)
            .aria_label(aria)
            .size(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_SM))
            .bg(rgb(theme::SURFACE_RAISED))
            .border_1()
            .border_color(rgb(theme::INPUT))
            .text_size(px(theme::TEXT_BODY))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(if at_limit { theme::TEXT_DIM } else { theme::FG_SOFT }))
            .child(label);
        if s.enabled {
            b.cursor_pointer()
                .hover(|h| {
                    h.bg(rgb(theme::ACCENT))
                        .border_color(rgb(theme::BORDER_STRONG))
                        .text_color(rgb(theme::FG))
                })
                .active(|a| {
                    a.bg(rgb(theme::SURFACE_ACTIVE))
                        .border_color(rgb(theme::BORDER_STRONG))
                        .text_color(rgb(theme::FG))
                })
                .on_click(move |ev, window, cx| {
                    let k = if ev.modifiers().shift { 10.0 } else { 1.0 };
                    on_change(&(value + sign * step * k).clamp(min, max), window, cx);
                })
        } else {
            b.cursor_default()
        }
    };
    let text = if value.fract() == 0.0 { format!("{value:.0}") } else { format!("{value:.1}") };
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(theme::CONTROL_H))
        .p(px(3.))
        .gap(px(2.))
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::SURFACE_INSET))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .opacity(if s.enabled { 1.0 } else { 0.45 })
        .child(button("dec", -1.0, "\u{2212}", "Decrease"))
        .child(
            div()
                .min_w(px(100.))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(theme::TEXT_BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(if s.enabled { theme::FG } else { theme::TEXT_DIM }))
                .child(format!("{text}{}", s.suffix)),
        )
        .child(button("inc", 1.0, "+", "Increase"))
}
