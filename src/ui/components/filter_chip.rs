//! Selectable filter chip with an optional leading icon (cached exe PNG or Lucide SVG) and a
//! trailing count badge. Keyboard-operable with Tab + Enter/Space.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, ElementId, FontFeatures, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, Window, div, px, rgb,
};

use super::exe_icon;
use crate::ui::{icons, theme};

/// Leading icon displayed before a [`filter_chip`]'s label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChipIcon {
    /// A cached executable icon PNG on disk (from [`exe_icon::find_cached`]).
    Image(PathBuf),
    /// A Lucide SVG icon name embedded in [`icons`].
    Svg(&'static str),
}

fn tabular_features() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

/// Renders a pill-shaped selectable filter chip with optional leading icon, label, and clip count.
pub fn filter_chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    count: usize,
    leading: Option<ChipIcon>,
    selected: bool,
    on_select: impl Fn(&(), &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    let noun = if count == 1 { "clip" } else { "clips" };
    let aria: SharedString = format!("{label}, {count} {noun}").into();
    let on_select = Rc::new(on_select);
    let on_click = Rc::clone(&on_select);

    let icon_el = leading.map(|kind| match kind {
        ChipIcon::Image(path) => exe_icon::chip_view(path).into_any_element(),
        ChipIcon::Svg(name) => {
            let color = if selected { theme::BRAND } else { theme::MUTED_FG };
            icons::icon(name, color).size(px(14.)).into_any_element()
        }
    });

    let count_el = div()
        .text_size(px(theme::TEXT_MICRO))
        .line_height(px(14.))
        .font_weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .font_features(tabular_features())
        .text_color(rgb(if selected { theme::BRAND } else { theme::MUTED_FG }))
        .child(count.to_string());

    let chip = div()
        .id(id)
        .tab_stop(true)
        .role(Role::RadioButton)
        .aria_label(aria)
        .aria_selected(selected)
        .h(px(theme::CONTROL_H))
        .px(px(10.))
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .whitespace_nowrap()
        .rounded_full()
        .border_1()
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .focus_visible(|s| s.border_color(rgb(theme::RING)));

    let chip = if selected {
        chip.bg(rgb(theme::BRAND_SOFT))
            .border_color(rgb(theme::BRAND_BORDER))
            .text_color(rgb(theme::FG))
            .hover(|s| s.border_color(rgb(theme::BRAND)))
            .active(|s| s.opacity(0.9))
    } else {
        chip.bg(rgb(theme::CARD))
            .border_color(rgb(theme::BORDER))
            .text_color(rgb(theme::FG_SOFT))
            .hover(|s| {
                s.bg(rgb(theme::SURFACE_RAISED))
                    .border_color(rgb(theme::BORDER_STRONG))
                    .text_color(rgb(theme::FG))
            })
            .active(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
    };

    chip.children(icon_el)
        .child(label)
        .child(count_el)
        .on_click(move |_, window, cx| on_click(&(), window, cx))
        .on_key_down(move |ev: &KeyDownEvent, window, cx| {
            if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                cx.stop_propagation();
                on_select(&(), window, cx);
            }
        })
}
