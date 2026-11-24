//! The clip list. For now a plain folder listing; the library (thumbnails,
//! metadata from zc-store), the editor and sharing arrive in Phases 4–6.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::{
    Context, FocusHandle, FontWeight, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled, StyledImage as _,
    WeakEntity, Window, div, img, px, rgb,
};
use zc_core::Hns;
use zc_editor::ffmpeg::Ffmpeg;

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

/// A rename in progress: the file, the name being typed (without extension) and the last error.
struct Renaming {
    path: PathBuf,
    text: String,
    error: Option<String>,
}

/// The Clips screen: the folder listing with Refresh, Open folder, search and a kind filter.
pub struct ClipsView {
    app: WeakEntity<ZeroClipApp>,
    query: String,
    kind: Kind,
    /// Search box focus, made on the first render.
    focus: Option<FocusHandle>,
    /// Rename box focus, made on the first render.
    rename_focus: Option<FocusHandle>,
    renaming: Option<Renaming>,
    /// Thumbnail files known to exist.
    thumbs_ready: HashSet<PathBuf>,
    /// Thumbnails already asked for, so a failed one is not retried on every frame.
    thumbs_pending: HashSet<PathBuf>,
}

impl ClipsView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            query: String::new(),
            kind: Kind::All,
            focus: None,
            rename_focus: None,
            renaming: None,
            thumbs_ready: HashSet::new(),
            thumbs_pending: HashSet::new(),
        }
    }

    /// Makes the missing thumbnails one after another in the background (one ffmpeg at a time, so
    /// a big folder doesn't start hundreds of processes) and repaints as each one lands.
    fn request_thumbs(
        &mut self,
        todo: Vec<(PathBuf, PathBuf)>,
        ffmpeg_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if todo.is_empty() {
            return;
        }
        for (_, thumb) in &todo {
            self.thumbs_pending.insert(thumb.clone());
        }
        cx.spawn(async move |this, cx| {
            for (clip, thumb) in todo {
                let ffmpeg_path = ffmpeg_path.clone();
                let made = cx
                    .background_executor()
                    .spawn(async move {
                        make_thumbnail(ffmpeg_path.as_deref(), &clip, &thumb).then_some(thumb)
                    })
                    .await;
                if let Some(thumb) = made {
                    let _ = this.update(cx, |this, cx| {
                        this.thumbs_ready.insert(thumb);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Renames the file being edited, keeping its extension. A problem stays in the rename bar.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.renaming.as_mut() else { return };
        let outcome = renamed_path(&rename.path, &rename.text).and_then(|to| {
            std::fs::rename(&rename.path, &to)
                .map(|()| to)
                .map_err(|e| format!("Couldn't rename: {e}"))
        });
        match outcome {
            Ok(to) => {
                self.renaming = None;
                let name =
                    to.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let _ = self.app.update(cx, |app, cx| {
                    app.clips.invalidate();
                    app.notify(NoticeLevel::Info, format!("Renamed to {name}"));
                    cx.notify();
                });
            }
            Err(message) => rename.error = Some(message),
        }
        cx.notify();
    }
}

/// The thumbnail's file name: a hash of the clip's path, size and save time, so a renamed or
/// replaced clip gets a fresh one (FNV-1a, which is stable between runs).
fn thumb_file_name(file: &ClipFile) -> String {
    let secs = file
        .modified
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in file
        .path
        .to_string_lossy()
        .bytes()
        .chain(file.size_bytes.to_le_bytes())
        .chain(secs.to_le_bytes())
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}.jpg")
}

/// Writes the thumbnail for `clip` to `thumb`. Returns whether the file now exists.
fn make_thumbnail(ffmpeg_path: Option<&Path>, clip: &Path, thumb: &Path) -> bool {
    let Ok(ffmpeg) = Ffmpeg::locate(ffmpeg_path) else { return false };
    let Some(dir) = thumb.parent() else { return false };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    // Written under a temporary name and renamed, so a killed ffmpeg never leaves a half image
    // that would later count as a finished thumbnail.
    let part = thumb.with_extension("part.jpg");
    let _ = std::fs::remove_file(&part);
    let done = ffmpeg.thumbnail(clip, Hns::from_millis(500), 192, &part).is_ok()
        && std::fs::rename(&part, thumb).is_ok();
    if !done {
        let _ = std::fs::remove_file(&part);
    }
    done
}

/// Where `path` goes when its name, without the extension, becomes `stem`.
///
/// # Errors
/// A message for the user when the name is empty, has characters Windows doesn't allow, is the
/// current name, or belongs to another file.
fn renamed_path(path: &Path, stem: &str) -> Result<PathBuf, String> {
    let stem = stem.trim();
    if stem.is_empty() {
        return Err("Type a name.".into());
    }
    if stem.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
        return Err("A name can't contain < > : \" / \\ | ? *".into());
    }
    if stem.ends_with('.') {
        return Err("A name can't end with a dot.".into());
    }
    let name = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    };
    let to = path.with_file_name(name);
    if to == path {
        return Err("That is already its name.".into());
    }
    // Windows names ignore case, so a case-only change points at the same file and is allowed.
    let same_file = to.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy());
    if !same_file && to.exists() {
        return Err("A file with that name already exists.".into());
    }
    Ok(to)
}

/// The small picture at the start of a row. `source` is the image file, if there is one yet.
fn thumb_box(source: Option<PathBuf>) -> gpui::Div {
    let frame = div()
        .w(px(64.))
        .h(px(36.))
        .flex_none()
        .rounded(px(theme::RADIUS_MD))
        .overflow_hidden()
        .bg(rgb(theme::ACCENT));
    match source {
        Some(path) => frame.child(img(path).w(px(64.)).h(px(36.)).object_fit(ObjectFit::Cover)),
        None => frame,
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
        let rename_focus = self.rename_focus.get_or_insert_with(|| cx.focus_handle()).clone();

        // Thumbnails live next to the database. Any that are missing are made in the background.
        let (ffmpeg_path, thumb_dir) = {
            let app = entity.read(cx);
            (
                app.settings.editor.ffmpeg_path.clone(),
                app.paths.db_file.parent().map(|data| data.join("thumbs")),
            )
        };
        if let Some(thumb_dir) = &thumb_dir {
            let wanted: Vec<(PathBuf, PathBuf)> = entity
                .read(cx)
                .clips
                .files
                .iter()
                .filter(|f| Kind::Videos.accepts(&f.name))
                .map(|f| (f.path.clone(), thumb_dir.join(thumb_file_name(f))))
                .collect();
            let mut todo = Vec::new();
            for (clip, thumb) in wanted {
                if self.thumbs_ready.contains(&thumb) || self.thumbs_pending.contains(&thumb) {
                    continue;
                }
                if thumb.is_file() {
                    self.thumbs_ready.insert(thumb);
                } else {
                    todo.push((clip, thumb));
                }
            }
            self.request_thumbs(todo, ffmpeg_path, cx);
        }

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
                let is_video = Kind::Videos.accepts(&file.name);
                let edit = is_video.then(|| {
                    let path = file.path.clone();
                    button_ghost(("clip-edit", i), "Edit").on_click(cx.listener(
                        move |this, _, _, cx| {
                            let path = path.clone();
                            let _ = this.app.update(cx, |app, cx| app.open_in_editor(path, cx));
                        },
                    ))
                });
                let rename = {
                    let path = file.path.clone();
                    button_ghost(("clip-rename-open", i), "Rename").on_click(cx.listener(
                        move |this, _, window, cx| {
                            let text = path
                                .file_stem()
                                .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                            this.renaming =
                                Some(Renaming { path: path.clone(), text, error: None });
                            if let Some(focus) = &this.rename_focus {
                                focus.focus(window, cx);
                            }
                            cx.notify();
                        },
                    ))
                };
                // A video shows its frame once ffmpeg has made it; a screenshot is its own picture.
                let thumb = if is_video {
                    thumb_dir
                        .as_ref()
                        .map(|dir| dir.join(thumb_file_name(file)))
                        .filter(|thumb| self.thumbs_ready.contains(thumb))
                } else {
                    Some(file.path.clone())
                };
                let name = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(thumb_box(thumb))
                    .child(div().min_w_0().child(file.name.clone()));
                let actions = div().flex().flex_row().gap_1().children(edit).child(rename);
                table_row(
                    name,
                    format!("{:.1} MB", file.size_bytes as f64 / 1_000_000.0),
                    file.modified.map_or_else(|| "\u{2014}".to_owned(), age),
                    Some(actions),
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

        // The rename bar sits above the list while a rename is open: the typed name, the kept
        // extension, Save and Cancel, and the last problem.
        let rename_bar = self.renaming.as_ref().map(|rename| {
            let ext = rename
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map_or_else(String::new, |e| format!(".{e}"));
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(text_field(
                    "clip-rename",
                    &rename_focus,
                    window,
                    &rename.text,
                    "New name",
                    360.,
                    cx.listener(|this, v: &String, _, cx| {
                        if let Some(rename) = this.renaming.as_mut() {
                            rename.text = v.clone();
                            rename.error = None;
                        }
                        cx.notify();
                    }),
                ))
                .child(div().text_sm().text_color(rgb(theme::MUTED_FG)).child(ext))
                .child(
                    button("clip-rename-save", "Save")
                        .on_click(cx.listener(|this, _, _, cx| this.commit_rename(cx))),
                )
                .child(button_ghost("clip-rename-cancel", "Cancel").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.renaming = None;
                        cx.notify();
                    },
                )))
                .children(
                    rename
                        .error
                        .as_ref()
                        .map(|e| div().text_sm().text_color(rgb(theme::DANGER)).child(e.clone())),
                )
        });

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
            .children(rename_bar)
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
        .child(div().w(px(150.)).flex().justify_end().children(action))
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
    fn rename_keeps_the_extension_and_rejects_bad_names() {
        let path = Path::new("clips/old.mp4");
        assert_eq!(renamed_path(path, " ace ").unwrap(), Path::new("clips/ace.mp4"));
        assert!(renamed_path(path, "").is_err());
        assert!(renamed_path(path, "a/b").is_err());
        assert!(renamed_path(path, "what?").is_err());
        assert!(renamed_path(path, "dots.").is_err());
        assert!(renamed_path(path, "old").is_err(), "the same name is refused");
    }

    #[test]
    fn thumbnail_name_is_stable_and_follows_size() {
        let a = file("a.mp4");
        let mut b = file("a.mp4");
        assert_eq!(thumb_file_name(&a), thumb_file_name(&b));
        b.size_bytes = 2;
        assert_ne!(thumb_file_name(&a), thumb_file_name(&b));
        assert!(thumb_file_name(&a).ends_with(".jpg"));
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
