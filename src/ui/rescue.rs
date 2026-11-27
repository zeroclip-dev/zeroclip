//! The rescue prompt: footage dumped after a game died. A non-modal banner under the
//! focus prompt. It never takes keyboard focus, and the window is never raised for it.

use std::io;
use std::path::{Path, PathBuf};

use gpui::{
    Context, FontWeight, IntoElement, ParentElement, StatefulInteractiveElement as _, Styled, div,
    px, rgb,
};
use zc_core::Hns;

use super::components::banner::banner as strip;
use super::components::button::{button_danger, button_ghost, button_primary};
use super::{human_duration, icons, theme};
use crate::app::{Notice, NoticeLevel, ZeroClipApp};

/// Footage waiting for an answer. `game` and `duration` are unknown for files found at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rescue {
    pub path: PathBuf,
    pub game: Option<String>,
    pub duration: Option<Hns>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RescueChoice {
    Keep,
    Discard,
    Later,
}

/// `recovery-123.mp4.tmp` -> `rescued-123.mp4` in the same folder. None for any other name.
pub fn kept_name(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("recovery-")?.strip_suffix(".tmp")?;
    // `rest` is `<unix_ms>.<ext>`: both parts must exist.
    let (stamp, ext) = rest.split_once('.')?;
    if stamp.is_empty() || ext.is_empty() {
        return None;
    }
    Some(path.with_file_name(format!("rescued-{rest}")))
}

/// Carries out the answer. Returns the notice to show (None for Later) and whether a file was
/// kept, so the caller can refresh the clip list. An io error is a notice too: the caller drops
/// the prompt either way, so it can't loop.
pub fn resolve(path: &Path, choice: RescueChoice) -> (Option<Notice>, bool) {
    let notice = |level, text: String| Some(Notice { level, text });
    match choice {
        RescueChoice::Later => (None, false),
        RescueChoice::Keep => {
            let Some(target) = kept_name(path) else {
                let text = format!("Can't keep {}: unexpected name.", path.display());
                return (notice(NoticeLevel::Error, text), false);
            };
            match std::fs::rename(path, &target) {
                Ok(()) => {
                    let text = format!("Kept the footage: {}", target.display());
                    (notice(NoticeLevel::Info, text), true)
                }
                Err(error) => (notice(NoticeLevel::Error, io_text("keep", path, &error)), false),
            }
        }
        RescueChoice::Discard => match std::fs::remove_file(path) {
            Ok(()) => (notice(NoticeLevel::Info, "Discarded the rescued footage.".into()), false),
            Err(error) => (notice(NoticeLevel::Error, io_text("discard", path, &error)), false),
        },
    }
}

fn io_text(verb: &str, path: &Path, error: &io::Error) -> String {
    format!("Couldn't {verb} {}: {error}", path.display())
}

fn question(rescue: &Rescue) -> String {
    match (&rescue.game, rescue.duration) {
        (Some(game), Some(d)) => format!("Keep the last {} of {game}?", human_duration(d)),
        _ => "Keep the footage saved after the game closed unexpectedly?".into(),
    }
}

/// The banner for the first waiting rescue. Nothing to draw when the list is empty.
pub fn banner(app: &ZeroClipApp, cx: &mut Context<ZeroClipApp>) -> Option<impl IntoElement> {
    let rescue = app.rescues.first()?;
    let extra = app.rescues.len().saturating_sub(1);
    let answer = |choice| {
        cx.listener(move |app, _, _, cx| {
            app.answer_rescue(choice);
            cx.notify();
        })
    };
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
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(icons::icon("film", theme::WARN))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .child(question(rescue)),
                    )
                    .children((extra > 0).then(|| theme::caption(format!("+{extra} more")))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        button_primary("rescue-keep", "Keep clip")
                            .on_click(answer(RescueChoice::Keep)),
                    )
                    .child(
                        button_danger("rescue-discard", "Discard")
                            .on_click(answer(RescueChoice::Discard)),
                    )
                    .child(
                        button_ghost("rescue-later", "Later").on_click(answer(RescueChoice::Later)),
                    ),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zc-rescue-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn kept_name_renames_matching_files() {
        let p = Path::new("out").join("recovery-123.mp4.tmp");
        assert_eq!(kept_name(&p), Some(Path::new("out").join("rescued-123.mp4")));
    }

    #[test]
    fn kept_name_rejects_other_names() {
        for name in
            ["clip.mp4", "recovery-123.mp4", "recovery-.mp4.tmp", "recovery-123.tmp", "x-1.mp4.tmp"]
        {
            assert_eq!(kept_name(Path::new(name)), None, "{name}");
        }
    }

    #[test]
    fn keep_renames_the_file() {
        let dir = scratch("keep");
        let tmp = dir.join("recovery-1.mp4.tmp");
        std::fs::write(&tmp, b"x").expect("file");
        let (notice, kept) = resolve(&tmp, RescueChoice::Keep);
        let ok = dir.join("rescued-1.mp4").exists() && !tmp.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok && kept);
        assert_eq!(notice.map(|n| n.level), Some(NoticeLevel::Info));
    }

    #[test]
    fn discard_deletes_the_file() {
        let dir = scratch("discard");
        let tmp = dir.join("recovery-2.mp4.tmp");
        std::fs::write(&tmp, b"x").expect("file");
        let (notice, kept) = resolve(&tmp, RescueChoice::Discard);
        let gone = !tmp.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone && !kept);
        assert_eq!(notice.map(|n| n.level), Some(NoticeLevel::Info));
    }

    #[test]
    fn later_leaves_the_file_alone() {
        let dir = scratch("later");
        let tmp = dir.join("recovery-3.mp4.tmp");
        std::fs::write(&tmp, b"x").expect("file");
        let (notice, kept) = resolve(&tmp, RescueChoice::Later);
        let stays = tmp.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(stays && !kept && notice.is_none());
    }

    #[test]
    fn io_errors_become_error_notices() {
        let dir = scratch("err");
        let missing = dir.join("recovery-4.mp4.tmp");
        for choice in [RescueChoice::Keep, RescueChoice::Discard] {
            let (notice, kept) = resolve(&missing, choice);
            assert_eq!(notice.map(|n| n.level), Some(NoticeLevel::Error));
            assert!(!kept);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn question_is_generic_without_game_info() {
        let r = Rescue { path: "p".into(), game: None, duration: None };
        assert!(question(&r).contains("closed unexpectedly"));
        let r = Rescue {
            path: "p".into(),
            game: Some("G.exe".into()),
            duration: Some(Hns::from_secs(120)),
        };
        assert_eq!(question(&r), "Keep the last 2 min of G.exe?");
    }
}
