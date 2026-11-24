//! A top-bar tab. Highlighted when `active`; add `.on_click(cx.listener(..))` at the call site.

use gpui::{InteractiveElement, ParentElement, SharedString, Styled, div, px, rgb};

use crate::ui::icons::icon;
use crate::ui::theme;

pub fn tab_button(icon_name: &str, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    let text: SharedString = label.into();
    let tab = div()
        .id(text.clone())
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(theme::RADIUS_MD))
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)));
    let tab = if active {
        tab.bg(rgb(theme::MUTED)).text_color(rgb(theme::FG))
    } else {
        tab.text_color(rgb(theme::MUTED_FG))
    };
    tab.child(icon(icon_name)).child(text)
}
