//! The clip right-click menu, shared by every screen that shows clips. The state lives on the app
//! (`ZeroClipApp::clip_menu`); a screen opens it with [`on_right_click`] or [`more_button`], and
//! the shell draws it with [`layer`].

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Role,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px, rgb,
};

use super::clips::Kind;
use super::components::context_menu::{context_menu, menu_item, menu_separator};
use super::{icons, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

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
        .focus_visible(|s| s.border_1().border_color(rgb(theme::RING)))
        .role(Role::Button)
        .aria_label("More actions")
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_MD))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ACCENT)))
        .child(icons::icon("ellipsis", theme::FG_SOFT))
        .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
            // Not a drag start for the card underneath.
            cx.stop_propagation();
            let _ =
                app.update(cx, |app, cx| app.open_clip_menu(path.clone(), ev.position, window, cx));
        })
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
            menu_item(id, label, danger)
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.clip_menu = None;
                    act(app, path.clone(), window, cx);
                    cx.notify();
                }))
                .into_any_element()
        };
    let mut items = Vec::new();
    if is_video {
        items.push(item("menu-watch", "Watch", false, |app, path, _, cx| app.play_clip(path, cx)));
        items.push(item("menu-edit", "Edit", false, |app, path, _, cx| {
            app.open_in_editor(path, false, cx)
        }));
        items.push(menu_separator().into_any_element());
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
    items.push(menu_separator().into_any_element());
    items.push(item("menu-delete", "Move to Recycle Bin", true, |app, path, _, _| {
        match zc_win::shell::recycle(&path) {
            Ok(()) => {
                let _ = std::fs::remove_file(zc_core::bookmarks::sidecar(&path));
                zc_core::game_tag::remove(&path);
                app.clips.invalidate();
                app.notify(NoticeLevel::Info, "Moved to the Recycle Bin");
            }
            Err(error) => app.notify(NoticeLevel::Error, format!("Couldn't delete: {error}")),
        }
    }));
    let rows = items.len();
    let view = cx.entity();
    Some(
        context_menu(
            window,
            &app.menu_focus,
            at,
            rows,
            move |_, cx| {
                view.update(cx, |app, cx| {
                    app.clip_menu = None;
                    cx.notify();
                });
            },
            items,
        )
        .into_any_element(),
    )
}
