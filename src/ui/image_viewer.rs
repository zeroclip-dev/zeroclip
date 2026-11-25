//! Full-size screenshot viewer: fits the selected screenshot to the window on a dark canvas,
//! with previous/next navigation across the active filtered list, metadata (file name, date,
//! size), Show in folder, Copy path, and keyboard shortcuts (`Esc`, `Left`, `Right`).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    ClipboardItem, Context, FocusHandle, FontFeatures, FontWeight, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ObjectFit, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window,
    div, img, px, rgb,
};

use super::clips::{ClipFile, age};
use super::components::{button::button, card::kbd};
use super::icons::icon;
use super::theme;
use crate::app::{NoticeLevel, Tab, ZeroClipApp};

/// How long the Copy path button displays a "Copied" confirmation before resetting.
const COPIED_DURATION: Duration = Duration::from_millis(1_800);

/// Screen for inspecting screenshots from the Clips library.
pub struct ImageViewerView {
    app: WeakEntity<ZeroClipApp>,
    /// Screenshots in the current filtered Clips list, in display order.
    items: Vec<ClipFile>,
    /// Active screenshot index inside [`Self::items`].
    index: usize,
    /// Bumped on every [`Self::open`] so initial keyboard focus is claimed once per open.
    generation: u64,
    focus: Option<FocusHandle>,
    focused_generation: Option<u64>,
    /// Timestamp of the last "Copy path" click for transient confirmation feedback.
    copied_at: Option<Instant>,
}

impl ImageViewerView {
    /// Creates an empty screenshot viewer bound to `app`.
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            items: Vec::new(),
            index: 0,
            generation: 0,
            focus: None,
            focused_generation: None,
            copied_at: None,
        }
    }

    /// Opens the screenshot at `index` inside `items`.
    pub fn open(&mut self, items: Vec<ClipFile>, index: usize, cx: &mut Context<Self>) {
        self.generation += 1;
        self.items = items;
        self.index = if self.items.is_empty() { 0 } else { index.min(self.items.len() - 1) };
        self.copied_at = None;
        cx.notify();
    }

    fn current(&self) -> Option<&ClipFile> {
        self.items.get(self.index)
    }

    fn sync_selected_clip(&self, cx: &mut Context<Self>) {
        let Some(path) = self.current().map(|f| f.path.clone()) else { return };
        let _ = self.app.update(cx, |app, cx| {
            app.screens.clips().clone().update(cx, |clips, cx| clips.set_selected(path, cx));
        });
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.items.len() <= 1 {
            return;
        }
        if let Some(next) = step_index(self.index, self.items.len(), delta) {
            self.index = next;
            self.copied_at = None;
            self.sync_selected_clip(cx);
            cx.notify();
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.sync_selected_clip(cx);
        let _ = self.app.update(cx, |app, cx| {
            app.tab = Tab::Clips;
            cx.notify();
        });
    }

    fn reveal_current(&self, cx: &mut Context<Self>) {
        let Some(path) = self.current().map(|f| f.path.clone()) else { return };
        if let Err(error) = zc_win::shell::reveal(&path) {
            let _ = self.app.update(cx, |app, cx| {
                app.notify(NoticeLevel::Error, format!("Couldn't open Explorer: {error}"));
                cx.notify();
            });
        }
    }

    fn copy_current_path(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.current().map(|f| f.path.clone()) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
        let now = Instant::now();
        self.copied_at = Some(now);
        let _ = self.app.update(cx, |app, cx| {
            app.notify(NoticeLevel::Info, "Copied screenshot path");
            cx.notify();
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_at == Some(now) {
                    this.copied_at = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            return;
        }
        match k.key.as_str() {
            "escape" => self.close(cx),
            "left" => self.step(-1, cx),
            "right" => self.step(1, cx),
            _ => {}
        }
    }
}

/// Steps `current` by `delta` within `0..len`, wrapping around both ends. Returns `None` when
/// `len == 0`.
pub(crate) fn step_index(current: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let clamped = current.min(len - 1) as isize;
    let span = len as isize;
    Some((clamped + delta).rem_euclid(span) as usize)
}

/// Converts days since `1970-01-01` to `(year, month, day)` in the proleptic Gregorian calendar
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// Formats a `SystemTime` as a short UTC calendar date such as `"Nov 25, 2026"`.
fn format_utc_date(when: SystemTime) -> Option<String> {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = when.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let days = (secs / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    let mon = MONTHS.get(month.saturating_sub(1) as usize)?;
    Some(format!("{mon} {day}, {year}"))
}

/// Formats file byte size for screenshots (KB under 1 MB, MB above).
fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{} KB", bytes / 1_000)
    } else {
        "< 1 KB".to_owned()
    }
}

/// Metadata subtitle for the viewer header: calendar date, relative age, and file size.
fn format_meta(modified: Option<SystemTime>, size_bytes: u64) -> String {
    let size = format_size(size_bytes);
    match modified {
        Some(when) => match format_utc_date(when) {
            Some(date) => format!("{date} \u{b7} {} \u{b7} {size}", age(when)),
            None => format!("{} \u{b7} {size}", age(when)),
        },
        None => format!("Unknown date \u{b7} {size}"),
    }
}

fn tabular_features() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

fn shortcut_hint(keys: &[&'static str], action: &'static str) -> gpui::Div {
    div().flex().flex_row().items_center().gap_1().children(keys.iter().map(|k| kbd(*k))).child(
        div()
            .ml(px(2.))
            .text_size(px(theme::TEXT_CAPTION))
            .text_color(rgb(theme::MUTED_FG))
            .child(action),
    )
}

fn nav_button(
    id: &'static str,
    label: &'static str,
    icon_name: &'static str,
    icon_first: bool,
    aria: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .tab_stop(true)
        .role(Role::Button)
        .aria_label(aria)
        .h(px(theme::CONTROL_H))
        .px_3()
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .whitespace_nowrap()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::SURFACE_RAISED))
        .border_1()
        .border_color(rgb(theme::INPUT))
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::FG))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER_STRONG)))
        .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)).border_color(rgb(theme::BORDER_STRONG)));
    let glyph = icon(icon_name, theme::FG_SOFT).size(px(14.));
    if icon_first { base.child(glyph).child(label) } else { base.child(label).child(glyph) }
}

fn disabled_nav_button(
    id: &'static str,
    label: &'static str,
    icon_name: &'static str,
    icon_first: bool,
    aria: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria)
        .h(px(theme::CONTROL_H))
        .px_3()
        .flex()
        .flex_none()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .whitespace_nowrap()
        .rounded(px(theme::RADIUS_MD))
        .bg(rgb(theme::MUTED))
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::TEXT_DIM))
        .opacity(0.5)
        .cursor_default();
    let glyph = icon(icon_name, theme::TEXT_DIM).size(px(14.));
    if icon_first { base.child(glyph).child(label) } else { base.child(label).child(glyph) }
}

impl Render for ImageViewerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if self.focused_generation != Some(self.generation) {
            self.focused_generation = Some(self.generation);
            window.focus(&focus, cx);
        }

        let current = self.current().cloned();
        let total = self.items.len();
        let can_step = total > 1;
        let copied = self.copied_at.is_some_and(|t| t.elapsed() < COPIED_DURATION);

        let title: SharedString =
            current.as_ref().map_or_else(|| "Screenshot viewer".into(), |f| f.name.clone().into());
        let subtitle: Option<SharedString> =
            current.as_ref().map(|f| format_meta(f.modified, f.size_bytes).into());

        let show_in_folder_btn = current.as_ref().map(|_| {
            button("viewer-reveal", "Show in folder")
                .aria_label("Show screenshot in folder")
                .flex_none()
                .whitespace_nowrap()
                .child(icon("folder-open", theme::FG_SOFT).size(px(14.)))
                .on_click(cx.listener(|this, _, _, cx| this.reveal_current(cx)))
        });

        let copy_path_btn = current.as_ref().map(|_| {
            let label = if copied { "Copied" } else { "Copy path" };
            let glyph = if copied {
                icon("check", theme::BRAND).size(px(14.))
            } else {
                icon("copy", theme::FG_SOFT).size(px(14.))
            };
            button("viewer-copy", label)
                .aria_label("Copy screenshot file path")
                .flex_none()
                .whitespace_nowrap()
                .when(copied, |d| {
                    d.bg(rgb(theme::BRAND_SOFT))
                        .border_color(rgb(theme::BRAND_BORDER))
                        .text_color(rgb(theme::FG))
                })
                .child(glyph)
                .on_click(cx.listener(|this, _, _, cx| this.copy_current_path(cx)))
        });

        let close_btn = button("viewer-close", "Close")
            .aria_label("Close screenshot viewer")
            .flex_none()
            .whitespace_nowrap()
            .child(icon("x", theme::FG_SOFT).size(px(14.)))
            .on_click(cx.listener(|this, _, _, cx| this.close(cx)));

        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .bg(rgb(theme::BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(icon("image", theme::MUTED_FG))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .truncate()
                                    .text_size(px(theme::TEXT_HEADING))
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::FG))
                                    .child(title),
                            )
                            .children(subtitle.map(|sub| {
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .truncate()
                                    .text_size(px(theme::TEXT_CAPTION))
                                    .line_height(px(16.))
                                    .text_color(rgb(theme::MUTED_FG))
                                    .child(sub)
                            })),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .children(show_in_folder_btn)
                    .children(copy_path_btn)
                    .child(close_btn),
            );

        let canvas = div()
            .id("viewer-canvas")
            .flex_1()
            .min_h(px(120.))
            .bg(rgb(theme::MEDIA_BG))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .relative()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    if let Some(focus) = &this.focus
                        && !focus.contains_focused(window, cx)
                    {
                        window.focus(focus, cx);
                    }
                }),
            );

        let canvas = match &current {
            Some(item) if item.path.is_file() => canvas
                .child(img(item.path.clone()).w_full().h_full().object_fit(ObjectFit::Contain)),
            Some(item) => canvas.child(
                div()
                    .max_w(px(460.))
                    .mx_4()
                    .p_4()
                    .rounded(px(theme::RADIUS))
                    .bg(rgb(theme::DANGER_SOFT))
                    .border_1()
                    .border_color(rgb(theme::DANGER_BORDER))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(theme::TEXT_HEADING))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::FG))
                            .child("Screenshot not found"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .line_height(px(18.))
                            .text_color(rgb(theme::FG_SOFT))
                            .child(format!(
                                "{} may have been moved or deleted.",
                                item.path.display()
                            )),
                    ),
            ),
            None => canvas.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .child(
                        div()
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(theme::CARD))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .child(icon("image", theme::MUTED_FG)),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .text_color(rgb(theme::FG_SOFT))
                            .child("Select a screenshot to view."),
                    ),
            ),
        };

        let prev_btn = if can_step {
            nav_button("viewer-prev", "Previous", "chevron-left", true, "Previous screenshot")
                .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx)))
        } else {
            disabled_nav_button(
                "viewer-prev",
                "Previous",
                "chevron-left",
                true,
                "Previous screenshot",
            )
        };

        let next_btn = if can_step {
            nav_button("viewer-next", "Next", "chevron-right", false, "Next screenshot")
                .on_click(cx.listener(|this, _, _, cx| this.step(1, cx)))
        } else {
            disabled_nav_button("viewer-next", "Next", "chevron-right", false, "Next screenshot")
        };

        let position_pill = (total > 0).then(|| {
            div()
                .px(px(10.))
                .py(px(4.))
                .rounded_full()
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .text_size(px(theme::TEXT_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .font_features(tabular_features())
                .text_color(rgb(theme::FG_SOFT))
                .child(format!("{} of {total}", self.index + 1))
        });

        let control_bar = div()
            .flex_none()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_3()
            .bg(rgb(theme::CARD))
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(prev_btn)
                    .child(next_btn)
                    .children(position_pill),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_x_4()
                    .gap_y_1()
                    .child(shortcut_hint(&["\u{2190}", "\u{2192}"], "Navigate"))
                    .child(shortcut_hint(&["Esc"], "Close")),
            );

        div()
            .id("image-viewer")
            .track_focus(&focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme::BG))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key(ev, cx)))
            .child(header)
            .child(canvas)
            .child(control_bar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_index_wraps_in_both_directions_and_handles_edge_cases() {
        assert_eq!(step_index(0, 0, 1), None);
        assert_eq!(step_index(0, 0, -1), None);
        assert_eq!(step_index(0, 1, 1), Some(0));
        assert_eq!(step_index(0, 1, -1), Some(0));

        // Forward wrap and backward wrap across a 4-item list.
        assert_eq!(step_index(0, 4, 1), Some(1));
        assert_eq!(step_index(3, 4, 1), Some(0));
        assert_eq!(step_index(0, 4, -1), Some(3));
        assert_eq!(step_index(2, 4, -1), Some(1));

        // Out-of-range starting index clamps before stepping.
        assert_eq!(step_index(99, 3, 1), Some(0));
    }

    #[test]
    fn format_utc_date_handles_known_dates_and_leap_years() {
        assert_eq!(format_utc_date(UNIX_EPOCH).as_deref(), Some("Jan 1, 1970"));
        // 2024-02-29 00:00:00 UTC = 19782 days after epoch.
        let leap = UNIX_EPOCH + Duration::from_secs(19_782 * 86_400);
        assert_eq!(format_utc_date(leap).as_deref(), Some("Feb 29, 2024"));
        // 2026-11-25 00:00:00 UTC = 20782 days after epoch.
        let nov = UNIX_EPOCH + Duration::from_secs(20_782 * 86_400);
        assert_eq!(format_utc_date(nov).as_deref(), Some("Nov 25, 2026"));
    }

    #[test]
    fn format_size_picks_appropriate_unit() {
        assert_eq!(format_size(0), "< 1 KB");
        assert_eq!(format_size(999), "< 1 KB");
        assert_eq!(format_size(420_500), "420 KB");
        assert_eq!(format_size(1_450_000), "1.4 MB");
    }
}
