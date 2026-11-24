//! The clip list. For now a plain folder listing; the library (thumbnails,
//! metadata from zc-store), the editor and sharing arrive in Phases 4–6.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::{
    Context, FocusHandle, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px, rgb,
};

use super::components::{
    button::{button, button_ghost},
    segmented::segmented,
    text_field::text_field,
};
use super::theme;
use crate::app::{NoticeLevel, ZeroClipApp};

const EXTENSIONS: &[&str] = &["mp4", "mkv", "png", "jpg"];

#[derive(Debug, Default)]
pub struct ClipList {
    files: Vec<ClipFile>,
    scanned: bool,
    error: Option<String>,
}

#[derive(Debug)]
struct ClipFile {
    name: String,
    path: PathBuf,
    size_bytes: u64,
    modified: Option<SystemTime>,
}

impl ClipList {
    /// Marks the listing stale; it is re-read the next time the tab is shown.
    pub fn invalidate(&mut self) {
        self.scanned = false;
    }

    /// Re-reads `dir` if the listing is stale.
    pub fn ensure_scanned(&mut self, dir: &Path) {
        if !self.scanned {
            self.rescan(dir);
        }
    }

    /// Video files (mp4, mkv): name and path, newest first. For the Editor's picker.
    pub fn videos(&self) -> Vec<(String, PathBuf)> {
        self.files
            .iter()
            .filter(|f| Kind::Videos.accepts(&f.name))
            .map(|f| (f.name.clone(), f.path.clone()))
            .collect()
    }

    fn rescan(&mut self, dir: &Path) {
        self.scanned = true;
        self.files.clear();
        self.error = None;
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.error = Some(format!("Couldn't read {}: {e}", dir.display()));
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let wanted = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
            let Ok(meta) = entry.metadata() else { continue };
            if wanted && meta.is_file() {
                self.files.push(ClipFile {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    path,
                    size_bytes: meta.len(),
                    modified: meta.modified().ok(),
                });
            }
        }
        self.files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    }
}

/// Which kinds of file the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    All,
    Videos,
    Screenshots,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::All, Kind::Videos, Kind::Screenshots];

    fn label(self) -> &'static str {
        match self {
            Kind::All => "All",
            Kind::Videos => "Videos",
            Kind::Screenshots => "Screenshots",
        }
    }

    /// mp4 and mkv are videos; everything else the list shows (png, jpg) is a screenshot.
    fn accepts(self, name: &str) -> bool {
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_video = ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("mkv");
        match self {
            Kind::All => true,
            Kind::Videos => is_video,
            Kind::Screenshots => !is_video,
        }
    }
}

/// The files of `kind` whose name contains `query` (case-insensitive, surrounding spaces
/// ignored), in the original order.
fn filter_files<'a>(files: &'a [ClipFile], kind: Kind, query: &str) -> Vec<&'a ClipFile> {
    let query = query.trim().to_lowercase();
    files
        .iter()
        .filter(|f| kind.accepts(&f.name) && f.name.to_lowercase().contains(&query))
        .collect()
}

/// The Clips screen: the folder listing with Refresh, Open folder, search and a kind filter.
pub struct ClipsView {
    app: WeakEntity<ZeroClipApp>,
    query: String,
    kind: Kind,
    /// Search box focus, made on the first render.
    focus: Option<FocusHandle>,
}

impl ClipsView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app, query: String::new(), kind: Kind::All, focus: None }
    }
}

impl Render for ClipsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entity) = self.app.upgrade() else {
            return div().into_any_element();
        };
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let dir = entity.read(cx).output_dir();
        // The listing lives on the app (other screens invalidate it), so it is re-read here,
        // lazily, the first time the tab is shown after a change.
        entity.update(cx, |app, _| {
            if !app.clips.scanned {
                app.clips.rescan(&dir);
            }
        });
        let clips = &entity.read(cx).clips;
        let shown = filter_files(&clips.files, self.kind, &self.query);

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().flex_1().text_xl().font_weight(FontWeight::SEMIBOLD).child("Clips"))
            .child(button_ghost("clips-refresh", "Refresh").on_click(cx.listener(
                |this, _, _, cx| {
                    let _ = this.app.update(cx, |app, _| app.clips.invalidate());
                    cx.notify();
                },
            )))
            .child(button("clips-open", "Open folder").on_click({
                let dir = dir.clone();
                cx.listener(move |this, _, _, cx| {
                    if let Err(e) = open_folder(&dir) {
                        let _ = this.app.update(cx, |app, cx| {
                            app.notify(
                                NoticeLevel::Error,
                                format!("Couldn't open {}: {e}", dir.display()),
                            );
                            cx.notify();
                        });
                    }
                })
            }));

        let tools = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(text_field(
                "clips-search",
                &focus,
                window,
                &self.query,
                "Search clips",
                240.,
                cx.listener(|this, v: &String, _, cx| {
                    this.query = v.clone();
                    cx.notify();
                }),
            ))
            .child(segmented(
                "clips-kind",
                Kind::ALL.iter().map(|k| SharedString::from(k.label())).collect(),
                Kind::ALL.iter().position(|k| *k == self.kind),
                true,
                cx.listener(|this, i: &usize, _, cx| {
                    this.kind = Kind::ALL.get(*i).copied().unwrap_or_default();
                    cx.notify();
                }),
            ))
            .child(div().ml_auto().text_sm().text_color(rgb(theme::MUTED_FG)).child(format!(
                "{} of {} files",
                shown.len(),
                clips.files.len()
            )));

        let body = if clips.files.is_empty() {
            empty("No clips yet. Saved clips and screenshots will show up here.")
        } else if shown.is_empty() {
            empty(format!("No clips match \u{201c}{}\u{201d}.", self.query.trim()))
        } else {
            let rows = shown.iter().enumerate().map(|(i, file)| {
                let edit = Kind::Videos.accepts(&file.name).then(|| {
                    let path = file.path.clone();
                    button_ghost(("clip-edit", i), "Edit").on_click(cx.listener(
                        move |this, _, _, cx| {
                            let path = path.clone();
                            let _ = this.app.update(cx, |app, cx| app.open_in_editor(path, cx));
                        },
                    ))
                });
                table_row(
                    file.name.clone(),
                    format!("{:.1} MB", file.size_bytes as f64 / 1_000_000.0),
                    file.modified.map_or_else(|| "\u{2014}".to_owned(), age),
                    edit,
                )
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .hover(|s| s.bg(rgb(theme::ACCENT)))
            });
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .rounded(px(theme::RADIUS))
                .bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    table_row("File", "Size", "Saved", None::<gpui::Div>)
                        .text_sm()
                        .text_color(rgb(theme::MUTED_FG)),
                )
                .child(
                    div()
                        .id("clips-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .text_sm()
                        .children(rows),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(header)
            .child(
                div().text_sm().text_color(rgb(theme::MUTED_FG)).child(dir.display().to_string()),
            )
            .children(
                clips.error.as_ref().map(|error| {
                    div().text_sm().text_color(rgb(theme::DANGER)).child(error.clone())
                }),
            )
            .child(tools)
            .child(body)
            .into_any_element()
    }
}

fn empty(text: impl Into<SharedString>) -> gpui::AnyElement {
    div()
        .py_8()
        .flex()
        .justify_center()
        .text_color(rgb(theme::MUTED_FG))
        .child(text.into())
        .into_any_element()
}

/// One row of the table: the name takes the spare width, size and age keep fixed widths.
fn table_row(
    name: impl IntoElement,
    size: impl IntoElement,
    saved: impl IntoElement,
    action: Option<impl IntoElement>,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap_6()
        .px_3()
        .py_2()
        .child(div().flex_1().min_w_0().child(name))
        .child(div().w(px(90.)).child(size))
        .child(div().w(px(120.)).child(saved))
        .child(div().w(px(56.)).flex().justify_end().children(action))
}

fn age(when: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(when).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

fn open_folder(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(windows)]
    std::process::Command::new("explorer").arg(dir).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> ClipFile {
        ClipFile { name: name.to_owned(), path: PathBuf::from(name), size_bytes: 1, modified: None }
    }

    fn names(files: &[&ClipFile]) -> Vec<String> {
        files.iter().map(|f| f.name.clone()).collect()
    }

    #[test]
    fn filters_by_kind_and_case_insensitive_name() {
        let files = [file("Clutch.MP4"), file("ace.mkv"), file("shot-clutch.png"), file("a.jpg")];
        assert_eq!(filter_files(&files, Kind::All, "").len(), 4);
        assert_eq!(names(&filter_files(&files, Kind::Videos, "")), ["Clutch.MP4", "ace.mkv"]);
        assert_eq!(
            names(&filter_files(&files, Kind::Screenshots, "")),
            ["shot-clutch.png", "a.jpg"]
        );
        assert_eq!(
            names(&filter_files(&files, Kind::All, " CLUTCH ")),
            ["Clutch.MP4", "shot-clutch.png"]
        );
        assert_eq!(names(&filter_files(&files, Kind::Videos, "clutch")), ["Clutch.MP4"]);
        assert!(filter_files(&files, Kind::Screenshots, "ace").is_empty());
    }
}
