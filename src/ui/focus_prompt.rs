//! The focus prompt banner (D35): a second game has focus while one is clipped. Non-modal, under
//! the top bar. It never takes keyboard focus, and the window is never raised for it.

use gpui::{Context, IntoElement, ParentElement, StatefulInteractiveElement as _, Styled, div};
use zc_core::control::FocusChoice;
use zc_core::focus_prompt::is_shell;

use super::components::button::button;
use crate::app::ZeroClipApp;

/// A focus prompt on screen: the game being clipped, and the app that has focus (file names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusPrompt {
    pub current: String,
    pub candidate: String,
}

/// Drops the prompt once its candidate is no longer the focused app. The engine's status names
/// the focused app, so the banner clears when focus moves on. Focus on the shell (taskbar, Start,
/// alt-tab) never clears it (D35): that is the way back to ZeroClip. Focus on ZeroClip itself
/// doesn't send a foreground event, so it never clears the banner either.
pub fn clear_if_left(app: &mut ZeroClipApp) {
    let on_shell = app.status.game.as_deref().is_some_and(is_shell);
    let left = !on_shell
        && app
            .focus_prompt
            .as_ref()
            .is_some_and(|prompt| app.status.game.as_deref() != Some(prompt.candidate.as_str()));
    if left {
        app.focus_prompt = None;
    }
}

/// The banner: the sentence, then the three answers. Returns nothing to draw when no prompt is up.
/// Non-modal and never takes keyboard focus. (Enter-picks-Keep from the egui version is not ported yet.)
pub fn banner(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> Option<impl IntoElement> {
    let prompt = app.focus_prompt.as_ref()?;
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .bg(gpui::rgb(0x3a3320))
            .child(format!("Clipping {}. {} is in focus.", prompt.current, prompt.candidate))
            .child(button("fp-keep", format!("Keep {}", prompt.current)).on_click(cx.listener(
                |app, _, _, cx| {
                    app.answer_focus_prompt(FocusChoice::Keep);
                    cx.notify();
                },
            )))
            .child(button("fp-switch", format!("Switch to {}", prompt.candidate)).on_click(
                cx.listener(|app, _, _, cx| {
                    app.answer_focus_prompt(FocusChoice::Switch);
                    cx.notify();
                }),
            ))
            .child(button("fp-desktop", "Clip desktop").on_click(cx.listener(|app, _, _, cx| {
                // The tracked game stays as it is; only the capture mode changes.
                app.answer_focus_prompt(FocusChoice::Keep);
                app.clip_desktop();
                cx.notify();
            }))),
    )
}
