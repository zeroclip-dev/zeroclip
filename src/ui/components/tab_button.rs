//! A top-bar tab. Selected renders as a filled soft pill with a brand-teal icon; unselected stays
//! quiet until hovered. Add `.on_click(cx.listener(..))` at the call site.

use gpui::{
    FontWeight, InteractiveElement, ParentElement, Role, SharedString, StatefulInteractiveElement,
    Styled, div, px, rgb, transparent_black,
};

use crate::ui::icons::icon;
use crate::ui::theme;

pub fn tab_button(icon_name: &str, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    let text: SharedString = label.into();
    let tab = div()
        .id(text.clone())
        .tab_stop(true)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .role(Role::Tab)
        .aria_label(text.clone())
        .h(px(theme::CONTROL_H))
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(7.))
        .rounded(px(theme::RADIUS_MD))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer();
    let (tab, icon_color) = if active {
        (
            tab.bg(rgb(theme::SURFACE_RAISED))
                .border_color(rgb(theme::BORDER))
                .text_color(rgb(theme::FG))
                .aria_selected(true)
                .hover(|s| s.bg(rgb(theme::ACCENT)))
                .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE))),
            theme::BRAND,
        )
    } else {
        (
            tab.text_color(rgb(theme::MUTED_FG))
                .hover(|s| s.bg(rgb(theme::SURFACE_RAISED)).text_color(rgb(theme::FG)))
                .active(|s| s.bg(rgb(theme::ACCENT))),
            theme::MUTED_FG,
        )
    };
    tab.child(icon(icon_name, icon_color)).child(text)
}
