//! The clip right-click menu, shared by every screen that shows clips. The state lives on the app
//! (`ZeroClipApp::clip_menu`); a screen opens it with [`on_right_click`] or [`more_button`], and
//! the shell draws it with [`layer`].

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Role, SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, Window, anchored,
    deferred, div, point, px, rgb, transparent_black,
};

use super::clips::Kind;
use super::{icons, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

const MENU_WIDTH: f32 = 224.0;
const MENU_ROW_H: f32 = theme::CONTROL_H;
const MENU_SEP_H: f32 = 9.0;
const MENU_PAD: f32 = 4.0;
const MENU_MARGIN: f32 = 8.0;

/// Right-clicking `el` opens the menu for `path`.
pub fn on_right_click<E: InteractiveElement>(
    el: E,
    app: &WeakEntity<ZeroClipApp>,
    path: &std::path::Path,
) -> E {
    let (app, path) = (app.clone(), path.to_path_buf());
    el.on_mouse_down(MouseButton::Right, move |ev, window, cx| {
        let _ = app.update(cx, |app, cx| app.open_clip_menu(path.clone(), ev.position, window, cx));
    })
}

/// The "..." button: the same menu for people who don't right-click. `size` is its side in px.
pub fn more_button(
    id: impl Into<gpui::ElementId>,
    app: &WeakEntity<ZeroClipApp>,
    path: &std::path::Path,
    size: f32,
) -> gpui::Stateful<gpui::Div> {
    let (app, path) = (app.clone(), path.to_path_buf());
    div()
        .id(id)
        .tab_stop(true)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(theme::RING)))
        .role(Role::Button)
        .aria_label("More actions")
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_MD))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ACCENT)).border_color(rgb(theme::BORDER)))
        .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
        .child(icons::icon("ellipsis", theme::FG_SOFT))
        .on_mouse_down(MouseButton::Left, {
            let (app, path) = (app.clone(), path.clone());
            move |ev, window, cx| {
                // Not a drag start for the card underneath.
                cx.stop_propagation();
                let _ = app
                    .update(cx, |app, cx| app.open_clip_menu(path.clone(), ev.position, window, cx));
            }
        })
        // Enter or Space on the focused button (a mouse click already opened it on press).
        .on_click(move |ev, window, cx| {
            if ev.is_keyboard() {
                let _ =
                    app.update(cx, |app, cx| app.open_clip_menu(path.clone(), ev.position(), window, cx));
            }
        })
}

fn menu_row(id: &'static str, label: &'static str, danger: bool) -> gpui::Stateful<gpui::Div> {
    let text: SharedString = label.into();
    let row = div()
        .id(id)
        .tab_stop(true)
        .role(Role::MenuItem)
        .aria_label(text.clone())
        .h(px(MENU_ROW_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded(px(theme::RADIUS_SM))
        .border_1()
        .border_color(transparent_black())
        .text_size(px(theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(text);
    if danger {
        row.text_color(rgb(theme::DANGER))
            .focus_visible(|s| s.border_color(rgb(theme::RING)).bg(rgb(theme::DANGER_SOFT)))
            .hover(|s| {
                s.bg(rgb(theme::DANGER_SOFT))
                    .border_color(rgb(theme::DANGER_BORDER))
                    .text_color(rgb(theme::DANGER))
            })
            .active(|s| {
                s.bg(rgb(theme::DANGER))
                    .border_color(rgb(theme::DANGER))
                    .text_color(rgb(theme::PRIMARY_FG))
            })
    } else {
        row.text_color(rgb(theme::FG))
            .focus_visible(|s| s.border_color(rgb(theme::RING)).bg(rgb(theme::ACCENT)))
            .hover(|s| s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::FG)))
            .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)).text_color(rgb(theme::FG)))
    }
}

fn menu_rule() -> gpui::Div {
    div().h(px(1.)).my(px(4.)).mx(px(6.)).bg(rgb(theme::BORDER_SUBTLE))
}

/// The open menu, if any: watch, edit, show in Explorer, rename, delete.
pub fn layer(
    app: &ZeroClipApp,
    window: &Window,
    cx: &mut Context<ZeroClipApp>,
) -> Option<AnyElement> {
    let (path, at) = app.clip_menu.clone()?;
    let is_video = path.file_name().is_some_and(|n| Kind::Videos.accepts(&n.to_string_lossy()));
    // Each item closes the menu, then does its thing.
    let item =
        |id: &'static str,
         label: &'static str,
         danger: bool,
         act: fn(&mut ZeroClipApp, PathBuf, &mut Window, &mut Context<ZeroClipApp>)| {
            let path = path.clone();
            menu_row(id, label, danger)
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.clip_menu = None;
                    act(app, path.clone(), window, cx);
                    cx.notify();
                }))
                .into_any_element()
        };
    let mut items = Vec::new();
    let mut row_count: usize = 0;
    let mut sep_count: usize = 0;
    if is_video {
        items.push(item("menu-watch", "Watch clip", false, |app, path, _, cx| {
            app.play_clip(path, cx)
        }));
        items.push(item("menu-edit", "Edit in editor", false, |app, path, _, cx| {
            app.open_in_editor(path, false, cx)
        }));
        items.push(menu_rule().into_any_element());
        row_count += 2;
        sep_count += 1;
    }
    items.push(item("menu-reveal", "Show in folder", false, |app, path, _, _| {
        if let Err(error) = zc_win::shell::reveal(&path) {
            app.notify(NoticeLevel::Error, format!("Couldn't open Explorer: {error}"));
        }
    }));
    items.push(item("menu-rename", "Rename", false, |app, path, window, cx| {
        // The rename bar lives on the Clips screen.
        app.tab = crate::app::Tab::Clips;
        app.screens.clips().clone().update(cx, |clips, cx| clips.begin_rename(path, window, cx));
    }));
    items.push(menu_rule().into_any_element());
    items.push(item("menu-delete", "Move to Recycle Bin", true, |app, path, _, cx| {
        match zc_win::shell::recycle(&path) {
            Ok(()) => {
                // The bookmarks and game tag go to the bin too, so restoring the clip from there
                // brings them back with it.
                for side in [zc_core::bookmarks::sidecar(&path), path.with_extension("game")] {
                    if side.is_file() {
                        let _ = zc_win::shell::recycle(&side);
                    }
                }
                // A screen still pointing at the deleted file would show or play a ghost.
                app.screens.clips().clone().update(cx, |clips, cx| clips.forget(&path, cx));
                app.screens.player().clone().update(cx, |player, cx| player.forget(&path, cx));
                app.screens.editor().clone().update(cx, |editor, cx| editor.forget(&path, cx));
                app.clips.invalidate();
                app.notify(NoticeLevel::Info, "Moved to the Recycle Bin");
            }
            Err(error) => app.notify(NoticeLevel::Error, format!("Couldn't delete: {error}")),
        }
    }));
    row_count += 3;
    sep_count += 1;

    let view_size = window.viewport_size();
    let height =
        row_count as f32 * MENU_ROW_H + sep_count as f32 * MENU_SEP_H + 2.0 * MENU_PAD + 2.0;
    let x =
        f32::from(at.x).min(f32::from(view_size.width) - MENU_WIDTH - MENU_MARGIN).max(MENU_MARGIN);
    let y =
        f32::from(at.y).min(f32::from(view_size.height) - height - MENU_MARGIN).max(MENU_MARGIN);

    let view = cx.entity();
    let close_left = view.clone();
    let close_right = view.clone();
    let close_key = view;
    let menu = div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(MENU_WIDTH))
        .p(px(MENU_PAD))
        .flex()
        .flex_col()
        .rounded(px(theme::RADIUS))
        .bg(rgb(theme::SURFACE_RAISED))
        .border_1()
        .border_color(rgb(theme::BORDER_STRONG))
        .shadow(theme::floating_shadow())
        // Clicks inside must not reach the backdrop, which closes the menu.
        .occlude()
        // Esc closes it. The owner focuses `menu_focus` when it opens the menu.
        .track_focus(&app.menu_focus)
        .on_key_down(move |ev, window, cx| match ev.keystroke.key.as_str() {
            "escape" => close_key.update(cx, |app, cx| {
                app.clip_menu = None;
                cx.notify();
            }),
            // Arrow keys walk the rows, like Tab and Shift+Tab.
            "down" => window.focus_next(cx),
            "up" => window.focus_prev(cx),
            _ => {}
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .children(items);

    // A window-sized layer drawn above everything; the menu sits in it, the rest is the backdrop.
    Some(
        deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .w(view_size.width)
                    .h(view_size.height)
                    .relative()
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        close_left.update(cx, |app, cx| {
                            app.clip_menu = None;
                            cx.notify();
                        });
                    })
                    .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                        close_right.update(cx, |app, cx| {
                            app.clip_menu = None;
                            cx.notify();
                        });
                    })
                    .child(menu),
            ),
        )
        .into_any_element(),
    )
}
