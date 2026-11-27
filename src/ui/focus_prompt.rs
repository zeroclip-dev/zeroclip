//! The focus prompt banner: a second game has focus while one is clipped. Non-modal, under
//! the top bar. It never takes keyboard focus, and the window is never raised for it.

use gpui::{
    Context, FontWeight, IntoElement, ParentElement, StatefulInteractiveElement as _, Styled, div,
    px, rgb,
};
use zc_core::control::FocusChoice;
use zc_core::focus_prompt::is_shell;

use super::components::banner::banner as strip;
use super::components::button::{button, button_ghost, button_primary};
use super::{icons, theme};
use crate::app::ZeroClipApp;

/// A focus prompt on screen: the game being clipped, and the app that has focus (file names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusPrompt {
    pub current: String,
    pub candidate: String,
}

/// Drops the prompt once its candidate is no longer the focused app. The engine's status names
/// the focused app, so the banner clears when focus moves on. Focus on the shell (taskbar, Start,
/// alt-tab) never clears it: that is the way back to ZeroClip. Focus on ZeroClip itself
/// doesn't send a foreground event, so it never clears the banner either.
pub fn clear_if_left(app: &mut ZeroClipApp) {
    if has_left(app.status.game.as_deref(), app.focus_prompt.as_ref()) {
        app.focus_prompt = None;
    }
}

/// True when the focused app (`game`) has moved away from the prompt's candidate, and is not the shell.
fn has_left(game: Option<&str>, prompt: Option<&FocusPrompt>) -> bool {
    let Some(prompt) = prompt else { return false };
    let on_shell = game.is_some_and(is_shell);
    !on_shell && game != Some(prompt.candidate.as_str())
}

/// The banner: the sentence, then the three answers. Returns nothing to draw when no prompt is up.
/// Non-modal and never takes keyboard focus. Enter-picks-Keep (egui version) is not ported: it needs
/// a focus handle on the root view, which lives outside this file.
pub fn banner(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> Option<impl IntoElement> {
    let prompt = app.focus_prompt.as_ref()?;
    Some(
        strip()
            .px_4()
            .py_2()
            .gap_3()
            .border_color(rgb(theme::WARN_BORDER))
            .child(
                div()
                    .flex_1()
                    .min_w(px(220.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(icons::icon("gamepad-2", theme::WARN))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .child(format!(
                                "Clipping {}. {} is in focus.",
                                prompt.current, prompt.candidate
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(button_primary("fp-keep", format!("Keep {}", prompt.current)).on_click(
                        cx.listener(|app, _, _, cx| {
                            app.answer_focus_prompt(FocusChoice::Keep);
                            cx.notify();
                        }),
                    ))
                    .child(button("fp-switch", format!("Switch to {}", prompt.candidate)).on_click(
                        cx.listener(|app, _, _, cx| {
                            app.answer_focus_prompt(FocusChoice::Switch);
                            cx.notify();
                        }),
                    ))
                    .child(button_ghost("fp-desktop", "Clip desktop").on_click(cx.listener(
                        |app, _, _, cx| {
                            // The tracked game stays as it is; only the capture mode changes.
                            app.answer_focus_prompt(FocusChoice::Keep);
                            app.clip_desktop();
                            cx.notify();
                        },
                    ))),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> FocusPrompt {
        FocusPrompt { current: "Game.exe".into(), candidate: "Other.exe".into() }
    }

    #[test]
    fn prompt_clears_when_candidate_loses_focus() {
        assert!(has_left(Some("Discord.exe"), Some(&prompt())));
        assert!(has_left(None, Some(&prompt())));
    }

    #[test]
    fn prompt_stays_while_candidate_has_focus() {
        assert!(!has_left(Some("Other.exe"), Some(&prompt())));
    }

    #[test]
    fn prompt_stays_when_focus_goes_to_the_shell() {
        // Taskbar, Start and alt-tab are the way back to ZeroClip.
        assert!(!has_left(Some("C:\\Windows\\explorer.exe"), Some(&prompt())));
    }

    #[test]
    fn no_prompt_means_nothing_to_clear() {
        assert!(!has_left(Some("Discord.exe"), None));
    }
}
