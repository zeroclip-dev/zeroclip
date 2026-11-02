//! The focus prompt banner (D35): a second game has focus while one is clipped. Non-modal, under
//! the top bar. It never takes keyboard focus, and the window is never raised for it.

use eframe::egui;
use zc_core::control::FocusChoice;
use zc_core::focus_prompt::is_shell;

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

/// The banner: the sentence, then the three answers. Keep is the default, so Enter picks it when
/// no other widget has keyboard focus.
pub fn banner(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    let Some(prompt) = app.focus_prompt.clone() else { return };
    let enter_keeps =
        ui.ctx().memory(|m| m.focused().is_none()) && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Clipping {}. {} is in focus.", prompt.current, prompt.candidate));
        if ui.button(format!("Keep {}", prompt.current)).clicked() || enter_keeps {
            app.answer_focus_prompt(FocusChoice::Keep);
        }
        if ui.button(format!("Switch to {}", prompt.candidate)).clicked() {
            app.answer_focus_prompt(FocusChoice::Switch);
        }
        if ui.button("Clip desktop").clicked() {
            // The tracked game stays as it is; only the capture mode changes.
            app.answer_focus_prompt(FocusChoice::Keep);
            app.clip_desktop();
        }
    });
}
