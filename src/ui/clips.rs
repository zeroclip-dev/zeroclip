//! The clip list. For now a plain folder listing; the library (thumbnails,
//! metadata from zc-store), the editor and sharing arrive in Phases 4–6.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, ParentElement, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity, Window, div, img, px,
    rgb,
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

/// Width of a card in the grid view.
const CARD_W: f32 = 220.0;

const EXTENSIONS: &[&str] = &["mp4", "mkv", "png", "jpg"];

#[derive(Debug, Default)]
pub struct ClipList {
    pub files: Vec<ClipFile>,
    scanned: bool,
    error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ClipFile {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: Option<SystemTime>,
    /// The game it was recorded from; `None` for clips from before games were tracked.
    pub game: Option<String>,
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

    /// The newest `n` videos.
    pub fn recent_videos(&self, n: usize) -> impl Iterator<Item = &ClipFile> {
        self.files.iter().filter(|f| Kind::Videos.accepts(&f.name)).take(n)
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
                let game = zc_core::game_tag::load(&path);
                self.files.push(ClipFile {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    path,
                    size_bytes: meta.len(),
                    modified: meta.modified().ok(),
                    game,
                });
            }
        }
        self.files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    }
}

/// Which kinds of file the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
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
    pub fn accepts(self, name: &str) -> bool {
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_video = ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("mkv");
        match self {
            Kind::All => true,
            Kind::Videos => is_video,
            Kind::Screenshots => !is_video,
        }
    }
}

/// The game filter's name for clips with no tag.
const OTHER: &str = "Other";

/// The files of `kind` whose name contains `query` (case-insensitive, surrounding spaces
/// ignored), in the original order.
fn filter_files<'a>(
    files: &'a [ClipFile],
    kind: Kind,
    query: &str,
    game: Option<&str>,
) -> Vec<&'a ClipFile> {
    let query = query.trim().to_lowercase();
    files
        .iter()
        .filter(|f| {
            kind.accepts(&f.name)
                && f.name.to_lowercase().contains(&query)
                && game.is_none_or(|g| f.game.as_deref().unwrap_or(OTHER) == g)
        })
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
    /// Only clips of this game (`OTHER` for untagged ones); `None` shows all.
    game: Option<String>,
    /// Search box focus, made on the first render.
    focus: Option<FocusHandle>,
    /// Rename box focus, made on the first render.
    rename_focus: Option<FocusHandle>,
    renaming: Option<Renaming>,
    /// The file the mouse went down on, and where. Moving on with the button held drags it out.
    drag_from: Option<(PathBuf, Point<Pixels>)>,
    /// Cards in a grid instead of rows in a list.
    grid: bool,
    thumbs: Thumbs,
}

/// Thumbnails of the clips: which exist, and the background job that makes the missing ones. Shared
/// by every screen that shows clip pictures.
#[derive(Default)]
pub struct Thumbs {
    /// Thumbnail files known to exist.
    ready: HashSet<PathBuf>,
    /// Thumbnails already asked for, so a failed one is not retried on every frame.
    pending: HashSet<PathBuf>,
}

impl Thumbs {
    /// The picture of `file` if it has been made.
    pub fn get(&self, dir: &Path, file: &ClipFile) -> Option<PathBuf> {
        Some(dir.join(thumb_file_name(file))).filter(|t| self.ready.contains(t))
    }

    /// Starts making the missing thumbnails of the `files` that are videos, one ffmpeg at a time so
    /// a big folder doesn't start hundreds of processes, repainting `V` as each one lands.
    /// `of` finds these thumbnails inside the view.
    pub fn ensure<'a, V: 'static>(
        &mut self,
        files: impl Iterator<Item = &'a ClipFile>,
        dir: &Path,
        ffmpeg_path: Option<PathBuf>,
        of: fn(&mut V) -> &mut Thumbs,
        cx: &mut Context<V>,
    ) {
        let mut todo = Vec::new();
        for file in files.filter(|f| Kind::Videos.accepts(&f.name)) {
            let thumb = dir.join(thumb_file_name(file));
            if self.ready.contains(&thumb) || self.pending.contains(&thumb) {
                continue;
            }
            if thumb.is_file() {
                self.ready.insert(thumb);
            } else {
                self.pending.insert(thumb.clone());
                todo.push((file.path.clone(), thumb));
            }
        }
        if todo.is_empty() {
            return;
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
                        of(this).ready.insert(thumb);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}

impl ClipsView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self {
            app,
            query: String::new(),
            kind: Kind::All,
            game: None,
            focus: None,
            rename_focus: None,
            renaming: None,
            drag_from: None,
            grid: true,
            thumbs: Thumbs::default(),
        }
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
                zc_core::bookmarks::rename(&rename.path, &to);
                zc_core::game_tag::rename(&rename.path, &to);
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

/// Rewrites `clip` as a plain MP4 at `to`: index up front, the video and the Master audio only.
/// Recordings are fragmented so a crash can't corrupt them, but VLC and Windows Media Player
/// mishandle seeking in those, and in files with several audio tracks.
pub fn remux_plain(ffmpeg_path: Option<&Path>, clip: &Path, to: &Path) -> Result<(), String> {
    let ffmpeg = Ffmpeg::locate(ffmpeg_path).map_err(|e| e.to_string())?;
    let result = ffmpeg.remux_for_export(clip, to).map_err(|e| e.to_string());
    if result.is_err() || std::fs::metadata(to).map_or(true, |m| m.len() == 0) {
        let _ = std::fs::remove_file(to);
        return result.and(Err("ffmpeg wrote nothing".into()));
    }
    Ok(())
}

/// Replaces `clip` with its plain-MP4 remux. The original stays if anything goes wrong, and
/// when something else (the player) holds it open.
pub fn make_plain_in_place(ffmpeg_path: Option<&Path>, clip: &Path) -> Result<(), String> {
    let tmp = clip.with_extension("zc-plain.mp4");
    remux_plain(ffmpeg_path, clip, &tmp)?;
    std::fs::rename(&tmp, clip).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("couldn't replace {}: {e}", clip.display())
    })
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
pub fn thumb_box(source: Option<PathBuf>, w: f32, h: f32) -> gpui::Div {
    let frame = div()
        .w(px(w))
        .h(px(h))
        .flex_none()
        .rounded(px(theme::RADIUS_MD))
        .overflow_hidden()
        .bg(rgb(theme::ACCENT));
    match source {
        Some(path) => frame.child(img(path).w(px(w)).h(px(h)).object_fit(ObjectFit::Cover)),
        None => frame,
    }
}

impl ClipsView {
    /// Lets `el` be dragged out of the window as a file: onto a browser's upload box, Explorer, a
    /// chat. A press then a move of a few pixels starts the drag; a plain click is untouched.
    fn drag_source<E: InteractiveElement>(&self, el: E, path: &Path, cx: &Context<Self>) -> E {
        let path = path.to_path_buf();
        el.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                this.drag_from = Some((path.clone(), ev.position));
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, _| {
            let Some((path, from)) = this.drag_from.clone() else { return };
            if !ev.dragging() {
                this.drag_from = None;
                return;
            }
            let moved = ev.position - from;
            if f32::from(moved.x).abs() + f32::from(moved.y).abs() < 8.0 {
                return;
            }
            this.drag_from = None;
            // Blocks until the drop or a cancel.
            if let Err(error) = zc_win::drag::drag_file(&path) {
                tracing::warn!(%error, "couldn't drag the clip out");
            }
        }))
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
            let mut thumbs = std::mem::take(&mut self.thumbs);
            let files = entity.read(cx).clips.files.clone();
            thumbs.ensure(
                files.iter(),
                thumb_dir,
                ffmpeg_path,
                |this: &mut Self| &mut this.thumbs,
                cx,
            );
            self.thumbs = thumbs;
        }

        let clips = &entity.read(cx).clips;
        let mut games: Vec<&str> =
            clips.files.iter().map(|f| f.game.as_deref().unwrap_or(OTHER)).collect();
        games.sort_unstable_by_key(|g| (*g == OTHER, g.to_lowercase()));
        games.dedup();
        let shown = filter_files(&clips.files, self.kind, &self.query, self.game.as_deref());

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
            .children((games.len() > 1).then(|| {
                let names: Vec<SharedString> = std::iter::once("All games")
                    .chain(games.iter().copied())
                    .map(SharedString::from)
                    .collect();
                let current = self
                    .game
                    .as_deref()
                    .map_or(Some(0), |g| games.iter().position(|x| *x == g).map(|i| i + 1));
                let games: Vec<String> = games.iter().map(|g| (*g).to_owned()).collect();
                segmented(
                    "clips-game",
                    names,
                    current,
                    true,
                    cx.listener(move |this, i: &usize, _, cx| {
                        this.game = i.checked_sub(1).and_then(|i| games.get(i).cloned());
                        cx.notify();
                    }),
                )
            }))
            .child(segmented(
                "clips-layout",
                vec![SharedString::from("List"), SharedString::from("Grid")],
                Some(usize::from(self.grid)),
                true,
                cx.listener(|this, i: &usize, _, cx| {
                    this.grid = *i == 1;
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
                    [("clip-watch", "Watch", true), ("clip-edit", "Edit", false)].map(
                        |(id, label, watch)| {
                            let path = file.path.clone();
                            button_ghost((id, i), label).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    let path = path.clone();
                                    let _ = this.app.update(cx, |app, cx| {
                                        if watch {
                                            app.play_clip(path, cx);
                                        } else {
                                            app.open_in_editor(path, false, cx);
                                        }
                                    });
                                },
                            ))
                        },
                    )
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
                    thumb_dir.as_ref().and_then(|dir| self.thumbs.get(dir, file))
                } else {
                    Some(file.path.clone())
                };
                let actions = div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .children(edit.into_iter().flatten())
                    .child(rename);
                if self.grid {
                    return self
                        .drag_source(div(), &file.path, cx)
                        .w(px(CARD_W))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .rounded(px(theme::RADIUS))
                        .bg(rgb(theme::CARD))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .hover(|s| s.bg(rgb(theme::ACCENT)))
                        .child({
                            let path = file.path.clone();
                            let picture = thumb_box(thumb, CARD_W - 18., (CARD_W - 18.) * 9. / 16.);
                            // A video's picture is a play button: click to watch it.
                            div().id(("clip-play", i)).relative().child(picture).when(
                                is_video,
                                |d| {
                                    d.cursor_pointer()
                                        .child(
                                            div()
                                                .absolute()
                                                .inset_0()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_color(rgb(0xffffff))
                                                .text_2xl()
                                                .child("\u{25b6}"),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let path = path.clone();
                                            let _ = this
                                                .app
                                                .update(cx, |app, cx| app.play_clip(path, cx));
                                        }))
                                },
                            )
                        })
                        .child(div().text_sm().truncate().child(file.name.clone()))
                        .child(div().text_xs().text_color(rgb(theme::MUTED_FG)).child(format!(
                            "{:.1} MB \u{b7} {}",
                            file.size_bytes as f64 / 1_000_000.0,
                            file.modified.map_or_else(|| "\u{2014}".to_owned(), age)
                        )))
                        .child(actions);
                }
                let name = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(thumb_box(thumb, 64., 36.))
                    .child(div().min_w_0().child(file.name.clone()));
                let row = table_row(
                    name,
                    format!("{:.1} MB", file.size_bytes as f64 / 1_000_000.0),
                    file.modified.map_or_else(|| "\u{2014}".to_owned(), age),
                    Some(actions),
                );
                self.drag_source(row, &file.path, cx)
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .hover(|s| s.bg(rgb(theme::ACCENT)))
            });
            if self.grid {
                div()
                    .id("clips-grid")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .content_start()
                    .gap_3()
                    .children(rows)
                    .into_any_element()
            } else {
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
            }
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

pub fn age(when: SystemTime) -> String {
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
        ClipFile {
            name: name.to_owned(),
            path: PathBuf::from(name),
            size_bytes: 1,
            modified: None,
            game: None,
        }
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
        assert_eq!(filter_files(&files, Kind::All, "", None).len(), 4);
        assert_eq!(names(&filter_files(&files, Kind::Videos, "", None)), ["Clutch.MP4", "ace.mkv"]);
        assert_eq!(
            names(&filter_files(&files, Kind::Screenshots, "", None)),
            ["shot-clutch.png", "a.jpg"]
        );
        assert_eq!(
            names(&filter_files(&files, Kind::All, " CLUTCH ", None)),
            ["Clutch.MP4", "shot-clutch.png"]
        );
        assert_eq!(names(&filter_files(&files, Kind::Videos, "clutch", None)), ["Clutch.MP4"]);
        assert!(filter_files(&files, Kind::Screenshots, "ace", None).is_empty());
    }
}
