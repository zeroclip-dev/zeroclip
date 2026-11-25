//! The Clips library: clips and screenshots grouped by game folder, with search, kind/game
//! filtering, sorting, selection, inline rename, drag-out, and folder controls.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    BoxShadow, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, ParentElement, Pixels, Point, Render,
    Role, SharedString, StatefulInteractiveElement as _, Styled, StyledImage as _, WeakEntity,
    Window, div, img, point, px, rgb, rgba, transparent_black,
};
use zc_core::Hns;
use zc_editor::ffmpeg::Ffmpeg;

use super::components::{
    button::{button, button_ghost, button_primary},
    exe_icon,
    filter_chip::{ChipIcon, filter_chip},
    segmented::segmented,
    text_field::text_field,
};
use super::{icons, theme};
use crate::app::{NoticeLevel, ZeroClipApp};

/// Minimum width of a clip card before a column drops.
const CARD_MIN_W: f32 = 200.0;
/// Horizontal and vertical gap between clip cards.
const CARD_GAP: f32 = 12.0;
/// Card horizontal chrome: 8 px padding + 1 px card border + 1 px media focus border on each side.
const CARD_CHROME: f32 = 20.0;
/// Page horizontal padding on each side.
const PAGE_PAD: f32 = 16.0;

const EXTENSIONS: &[&str] = &["mp4", "mkv", "png", "jpg"];
const MAX_SCAN_DEPTH: usize = 4;

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
        match walk(dir, 0, &mut self.files) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.error = Some(format!("Couldn't read {}: {e}", dir.display()));
                return;
            }
        }
        self.files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    }
}

/// Walks `dir` up to `MAX_SCAN_DEPTH` sub-folders deep, pushing matching clips to `out`.
fn walk(dir: &Path, depth: usize, out: &mut Vec<ClipFile>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth < MAX_SCAN_DEPTH {
                let _ = walk(&path, depth + 1, out);
            }
            continue;
        }
        let wanted = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
        if wanted && meta.is_file() {
            let game = zc_core::game_tag::load(&path);
            out.push(ClipFile {
                name: name.to_string_lossy().into_owned(),
                path,
                size_bytes: meta.len(),
                modified: meta.modified().ok(),
                game,
            });
        }
    }
    Ok(())
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

/// How clips within each game group are ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SortOrder {
    #[default]
    Newest,
    Largest,
    Name,
}

impl SortOrder {
    const ALL: [SortOrder; 3] = [SortOrder::Newest, SortOrder::Largest, SortOrder::Name];

    fn label(self) -> &'static str {
        match self {
            SortOrder::Newest => "Newest",
            SortOrder::Largest => "Largest",
            SortOrder::Name => "Name",
        }
    }
}

/// The game filter's name for clips with no tag or subfolder.
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

/// Group heading for `file`: its game tag, else its top subfolder under `root`, else [`OTHER`].
fn clip_group(file: &ClipFile, root: &Path) -> String {
    if let Some(game) = file.game.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        return game
            .strip_suffix(".exe")
            .or_else(|| game.strip_suffix(".EXE"))
            .unwrap_or(game)
            .to_owned();
    }
    if let Ok(rel) = file.path.strip_prefix(root) {
        let mut components = rel.components();
        if let (Some(Component::Normal(folder)), Some(_)) = (components.next(), components.next()) {
            let folder = folder.to_string_lossy();
            if !folder.is_empty() {
                return folder.into_owned();
            }
        }
    }
    OTHER.to_owned()
}

fn sort_group(files: &mut [&ClipFile], order: SortOrder) {
    match order {
        SortOrder::Newest => files.sort_by_key(|f| std::cmp::Reverse(f.modified)),
        SortOrder::Largest => files.sort_by_key(|f| std::cmp::Reverse(f.size_bytes)),
        SortOrder::Name => files.sort_by_key(|f| display_name(&f.name).to_lowercase()),
    }
}

/// Groups `files` by game folder/tag (alphabetical, [`OTHER`] last) and sorts inside each group.
fn group_files<'a>(
    files: &[&'a ClipFile],
    root: &Path,
    order: SortOrder,
) -> Vec<(String, Vec<&'a ClipFile>)> {
    let mut by_key: BTreeMap<(bool, String), (String, Vec<&'a ClipFile>)> = BTreeMap::new();
    for file in files {
        let label = clip_group(file, root);
        let key = (label == OTHER, label.to_lowercase());
        by_key.entry(key).or_insert_with(|| (label, Vec::new())).1.push(*file);
    }
    let mut groups: Vec<(String, Vec<&'a ClipFile>)> = by_key.into_values().collect();
    for (_, group) in &mut groups {
        sort_group(group, order);
    }
    groups
}

/// Computes the "All" chip count and per-game `(label, count)` entries (alphabetical, [`OTHER`]
/// last). Every game present in `all_files` gets a chip; counts reflect `filtered` (after kind
/// and search filters, before the game chip filter) so users see how many matching clips each game
/// holds.
fn game_chip_counts(
    all_files: &[ClipFile],
    filtered: &[&ClipFile],
    root: &Path,
) -> (usize, Vec<(String, usize)>) {
    let mut by_key: BTreeMap<(bool, String), (String, usize)> = BTreeMap::new();
    for file in all_files {
        let label = clip_group(file, root);
        let key = (label == OTHER, label.to_lowercase());
        by_key.entry(key).or_insert((label, 0));
    }
    for file in filtered {
        let label = clip_group(file, root);
        let key = (label == OTHER, label.to_lowercase());
        by_key.entry(key).or_insert((label, 0)).1 += 1;
    }
    (filtered.len(), by_key.into_values().collect())
}

/// Chooses the leading icon for a game filter chip: folder icon for [`OTHER`], cached exe icon
/// when present on disk, otherwise the generic Lucide gamepad icon.
fn game_chip_icon(game: &str, icons_dir: &Path) -> ChipIcon {
    if game == OTHER {
        ChipIcon::Svg("folder-open")
    } else if let Some(path) = exe_icon::find_cached(icons_dir, game) {
        ChipIcon::Image(path)
    } else {
        ChipIcon::Svg("gamepad-2")
    }
}

/// Collects the screenshots from `groups` in their on-screen display order, so previous/next in
/// the screenshot viewer matches the filtered and sorted Clips grid/list.
fn ordered_screenshots(groups: &[(String, Vec<&ClipFile>)]) -> Vec<ClipFile> {
    groups
        .iter()
        .flat_map(|(_, files)| files.iter())
        .filter(|file| Kind::Screenshots.accepts(&file.name))
        .map(|file| (*file).clone())
        .collect()
}

/// A rename in progress: the file, the name being typed (without extension) and the last error.
struct Renaming {
    path: PathBuf,
    text: String,
    error: Option<String>,
}

/// The Clips screen: game-grouped clip library with search, filters, sort, selection and rename.
pub struct ClipsView {
    app: WeakEntity<ZeroClipApp>,
    query: String,
    kind: Kind,
    sort: SortOrder,
    /// Only clips of this game (`OTHER` for untagged ones); `None` shows all.
    game: Option<String>,
    /// Currently selected clip path, shown with a brand focus ring.
    selected: Option<PathBuf>,
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
            sort: SortOrder::Newest,
            game: None,
            selected: None,
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
                self.selected = Some(to.clone());
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
    // The cleanup below deletes `to` on failure: never when it was already there (ffmpeg refuses
    // to overwrite, and that file isn't ours).
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
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
    // A leftover of an earlier, killed run: ours to clear.
    let _ = std::fs::remove_file(&tmp);
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
    // Windows reserves these device names, with or without an extension.
    let base = stem.split('.').next().unwrap_or(stem).trim_end().to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&base.as_str())
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.ends_with(|c: char| ('1'..='9').contains(&c)));
    if reserved {
        return Err("Windows doesn't allow that name.".into());
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

/// A clip's title in the UI: the date and time, without the repeated "ZeroClip clip" prefix and the
/// extension, so near-identical names stay tellable apart when the card truncates them.
pub fn display_name(file_name: &str) -> String {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(stem, _)| stem);
    let short = stem
        .strip_prefix("ZeroClip clip ")
        .or_else(|| stem.strip_prefix("ZeroClip "))
        .unwrap_or(stem);
    short.to_owned()
}

/// The small picture for a clip card or list row. `source` is the image file, if there is one yet.
pub fn thumb_box(source: Option<PathBuf>, w: f32, h: f32) -> gpui::Div {
    let frame = div()
        .w(px(w))
        .h(px(h))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_MD))
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_INSET));
    match source {
        Some(path) => frame.child(img(path).w(px(w)).h(px(h)).object_fit(ObjectFit::Cover)),
        None => frame.child(icons::icon("film", theme::TEXT_DIM)),
    }
}

fn brand_ring_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: rgb(theme::BRAND).into(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(1.),
        inset: false,
    }]
}

fn group_heading(title: &str, count: usize, first: bool) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .when(!first, |d| d.pt_2())
        .child(
            div()
                .text_size(px(theme::TEXT_CAPTION))
                .line_height(px(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::FG_SOFT))
                .child(title.to_owned()),
        )
        .child(
            div()
                .px(px(7.))
                .py(px(1.))
                .rounded_full()
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .text_size(px(theme::TEXT_MICRO))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::MUTED_FG))
                .child(count.to_string()),
        )
        .child(div().flex_1().h(px(1.)).bg(rgb(theme::BORDER_SUBTLE)))
}

impl ClipsView {
    /// Lets `el` be dragged out of the window as a file: onto a browser's upload box, Explorer, a
    /// chat. A press selects the clip immediately; moving a few pixels starts the drag.
    fn drag_source<E: InteractiveElement>(&self, el: E, path: &Path, cx: &Context<Self>) -> E {
        let path = path.to_path_buf();
        super::clip_menu::on_right_click(el, &self.app, &path)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.selected = Some(path.clone());
                    this.drag_from = Some((path.clone(), ev.position));
                    cx.notify();
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

    /// Opens the rename bar for `path`.
    pub fn begin_rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let text = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        self.selected = Some(path.clone());
        self.renaming = Some(Renaming { path, text, error: None });
        // Created here too: Rename from Home runs before Clips has ever rendered.
        self.rename_focus.get_or_insert_with(|| cx.focus_handle()).focus(window, cx);
        cx.notify();
    }

    /// Drops the selection when it is `path`, a clip that was just deleted.
    pub fn forget(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(path) {
            self.selected = None;
            cx.notify();
        }
    }

    /// Updates the highlighted clip selection (e.g. when stepping through screenshots in the
    /// viewer).
    pub fn set_selected(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected = Some(path);
        cx.notify();
    }

    fn render_card(
        &self,
        idx: usize,
        file: &ClipFile,
        thumb: Option<PathBuf>,
        card_w: f32,
        screenshot_nav: Option<(Rc<Vec<ClipFile>>, usize)>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let is_video = Kind::Videos.accepts(&file.name);
        let selected = self.selected.as_deref() == Some(file.path.as_path());
        let title = display_name(&file.name);
        let meta = format!(
            "{:.1} MB \u{b7} {}",
            file.size_bytes as f64 / 1_000_000.0,
            file.modified.map_or_else(|| "\u{2014}".to_owned(), age),
        );
        let pic_w = (card_w - CARD_CHROME).max(120.0);
        let pic_h = (pic_w * 9.0 / 16.0).floor();
        let group_name = format!("clip-card-{idx}");
        let play_path = file.path.clone();
        let play_path_key = file.path.clone();

        let picture = thumb_box(thumb, pic_w, pic_h);
        let media = div()
            .id(("clip-play", idx))
            .relative()
            .rounded(px(theme::RADIUS_MD))
            .overflow_hidden()
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .child(picture)
            .when(is_video, |d| {
                d.tab_stop(true)
                    .role(Role::Button)
                    .aria_label(format!("Watch {title}"))
                    .cursor_pointer()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .opacity(0.)
                            .group_hover(group_name.clone(), |s| s.opacity(1.))
                            .child(
                                div()
                                    .size(px(40.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(rgba(theme::SCRIM))
                                    .child(icons::icon("play", theme::FG)),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = play_path.clone();
                        this.selected = Some(path.clone());
                        let _ = this.app.update(cx, |app, cx| app.play_clip(path, cx));
                    }))
                    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                        if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            let path = play_path_key.clone();
                            this.selected = Some(path.clone());
                            let _ = this.app.update(cx, |app, cx| app.play_clip(path, cx));
                        }
                    }))
            })
            .when(!is_video, |d| {
                let open_path = file.path.clone();
                let open_path_key = file.path.clone();
                let nav_click = screenshot_nav.clone();
                let nav_key = screenshot_nav;
                d.tab_stop(true)
                    .role(Role::Button)
                    .aria_label(format!("View {title}"))
                    .cursor_pointer()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .opacity(0.)
                            .group_hover(group_name.clone(), |s| s.opacity(1.))
                            .child(
                                div()
                                    .size(px(40.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(rgba(theme::SCRIM))
                                    .child(icons::icon("image", theme::FG)),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(open_path.clone());
                        if let Some((shots, shot_idx)) = &nav_click {
                            let list = (**shots).clone();
                            let idx = *shot_idx;
                            let _ =
                                this.app.update(cx, |app, cx| app.view_screenshot(list, idx, cx));
                        }
                    }))
                    .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                        if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.selected = Some(open_path_key.clone());
                            if let Some((shots, shot_idx)) = &nav_key {
                                let list = (**shots).clone();
                                let idx = *shot_idx;
                                let _ = this
                                    .app
                                    .update(cx, |app, cx| app.view_screenshot(list, idx, cx));
                            }
                        }
                    }))
            });

        let more = super::clip_menu::more_button(("clip-more", idx), &self.app, &file.path, 28.);

        let card = div()
            .id(("clip-card", idx))
            .group(group_name)
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(title.clone())
            .aria_selected(selected)
            .w(px(card_w))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(theme::RADIUS))
            .border_1()
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .when(selected, |d| {
                d.bg(rgb(theme::SURFACE_RAISED))
                    .border_color(rgb(theme::BRAND))
                    .shadow(brand_ring_shadow())
                    .hover(|s| s.bg(rgb(theme::ACCENT)))
                    .active(|s| s.bg(rgb(theme::SURFACE_ACTIVE)))
            })
            .when(!selected, |d| {
                d.bg(rgb(theme::CARD))
                    .border_color(rgb(theme::BORDER))
                    .hover(|s| {
                        s.bg(rgb(theme::SURFACE_RAISED)).border_color(rgb(theme::BORDER_STRONG))
                    })
                    .active(|s| s.bg(rgb(theme::ACCENT)))
            })
            .child(media)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(theme::TEXT_BODY))
                                    .line_height(px(18.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(theme::FG))
                                    .truncate()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::TEXT_MICRO))
                                    .line_height(px(15.))
                                    .text_color(rgb(theme::MUTED_FG))
                                    .truncate()
                                    .child(meta),
                            ),
                    )
                    .child(more),
            );

        self.drag_source(card, &file.path, cx)
    }

    fn render_list_row(
        &self,
        idx: usize,
        file: &ClipFile,
        thumb: Option<PathBuf>,
        screenshot_nav: Option<(Rc<Vec<ClipFile>>, usize)>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let is_video = Kind::Videos.accepts(&file.name);
        let selected = self.selected.as_deref() == Some(file.path.as_path());
        let title = display_name(&file.name);
        let edit = is_video.then(|| {
            [("clip-watch", "Watch", true), ("clip-edit", "Edit", false)].map(
                |(id, label, watch)| {
                    let path = file.path.clone();
                    button_ghost((id, idx), label).on_click(cx.listener(move |this, _, _, cx| {
                        let path = path.clone();
                        this.selected = Some(path.clone());
                        let _ = this.app.update(cx, |app, cx| {
                            if watch {
                                app.play_clip(path, cx);
                            } else {
                                app.open_in_editor(path, false, cx);
                            }
                        });
                    }))
                },
            )
        });
        let view_shot = (!is_video).then(|| {
            let path = file.path.clone();
            let path_key = file.path.clone();
            let nav_click = screenshot_nav.clone();
            let nav_key = screenshot_nav;
            button_ghost(("clip-view", idx), "View")
                .aria_label(format!("View {title}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(path.clone());
                    if let Some((shots, shot_idx)) = &nav_click {
                        let list = (**shots).clone();
                        let idx = *shot_idx;
                        let _ = this.app.update(cx, |app, cx| app.view_screenshot(list, idx, cx));
                    }
                }))
                .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                    if matches!(ev.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.selected = Some(path_key.clone());
                        if let Some((shots, shot_idx)) = &nav_key {
                            let list = (**shots).clone();
                            let idx = *shot_idx;
                            let _ =
                                this.app.update(cx, |app, cx| app.view_screenshot(list, idx, cx));
                        }
                    }
                }))
        });
        let rename = {
            let path = file.path.clone();
            button_ghost(("clip-rename-open", idx), "Rename").on_click(
                cx.listener(move |this, _, window, cx| this.begin_rename(path.clone(), window, cx)),
            )
        };
        let more = super::clip_menu::more_button(("clip-more", idx), &self.app, &file.path, 32.);
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .children(edit.into_iter().flatten())
            .children(view_shot)
            .child(rename)
            .child(more);

        let name_col = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .min_w_0()
            .child(thumb_box(thumb, 64., 36.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::FG))
                            .truncate()
                            .child(title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(theme::TEXT_MICRO))
                            .text_color(rgb(theme::TEXT_DIM))
                            .truncate()
                            .child(file.name.clone()),
                    ),
            );

        let row = div()
            .id(("clip-row", idx))
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(title)
            .aria_selected(selected)
            .flex()
            .flex_row()
            .items_stretch()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .focus_visible(|s| s.border_color(rgb(theme::RING)))
            .when(selected, |d| {
                d.bg(rgb(theme::BRAND_SOFT)).hover(|s| s.bg(rgb(theme::BRAND_SOFT)))
            })
            .when(!selected, |d| {
                d.hover(|s| s.bg(rgb(theme::SURFACE_RAISED))).active(|s| s.bg(rgb(theme::ACCENT)))
            })
            .child(div().w(px(3.)).flex_none().when(selected, |d| d.bg(rgb(theme::BRAND))))
            .child(
                table_row(
                    name_col,
                    div()
                        .text_size(px(theme::TEXT_CAPTION))
                        .text_color(rgb(theme::MUTED_FG))
                        .child(format!("{:.1} MB", file.size_bytes as f64 / 1_000_000.0)),
                    div()
                        .text_size(px(theme::TEXT_CAPTION))
                        .text_color(rgb(theme::MUTED_FG))
                        .child(file.modified.map_or_else(|| "\u{2014}".to_owned(), age)),
                    Some(actions),
                )
                .flex_1()
                .min_w_0(),
            );

        self.drag_source(row, &file.path, cx)
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
        let (ffmpeg_path, thumb_dir, icons_dir) = {
            let app = entity.read(cx);
            (
                app.settings.editor.ffmpeg_path.clone(),
                app.paths.db_file.parent().map(|data| data.join("thumbs")),
                app.paths.icons_dir.clone(),
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
        let base_shown = filter_files(&clips.files, self.kind, &self.query, None);
        let (all_chip_count, game_counts) = game_chip_counts(&clips.files, &base_shown, &dir);
        let shown: Vec<&ClipFile> = match self.game.as_deref() {
            Some(selected_game) => {
                base_shown.into_iter().filter(|f| clip_group(f, &dir) == selected_game).collect()
            }
            None => base_shown,
        };
        let groups = group_files(&shown, &dir, self.sort);
        let screenshots = Rc::new(ordered_screenshots(&groups));

        let total_count = clips.files.len();
        let shown_count = shown.len();
        let count_text = if shown_count == total_count {
            match total_count {
                1 => "1 clip".to_owned(),
                n => format!("{n} clips"),
            }
        } else {
            format!("{shown_count} of {total_count}")
        };

        // Single 36 px toolbar row: title + count left, search + sort + view toggle right.
        let toolbar = div()
            .min_h(px(theme::CONTROL_H_LG))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(theme::page_title("Clips"))
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded_full()
                            .bg(rgb(theme::SURFACE_RAISED))
                            .border_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .text_size(px(theme::TEXT_CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::MUTED_FG))
                            .child(count_text),
                    ),
            )
            .child(div().flex_1().min_w(px(8.)))
            .child(text_field(
                "clips-search",
                &focus,
                window,
                &self.query,
                "Search clips",
                180.,
                cx.listener(|this, v: &String, _, cx| {
                    this.query = v.clone();
                    cx.notify();
                }),
            ))
            .child(segmented(
                "clips-sort",
                SortOrder::ALL.iter().map(|s| SharedString::from(s.label())).collect(),
                SortOrder::ALL.iter().position(|s| *s == self.sort),
                true,
                cx.listener(|this, i: &usize, _, cx| {
                    this.sort = SortOrder::ALL.get(*i).copied().unwrap_or_default();
                    cx.notify();
                }),
            ))
            .child(segmented(
                "clips-layout",
                vec![SharedString::from("List"), SharedString::from("Grid")],
                Some(usize::from(self.grid)),
                true,
                cx.listener(|this, i: &usize, _, cx| {
                    this.grid = *i == 1;
                    cx.notify();
                }),
            ));

        // Filter row: kind filter + wrapping game filter chips with icons and clip counts.
        let game_chips = (!game_counts.is_empty()).then(|| {
            let all_chip = filter_chip(
                "clips-game-all",
                "All",
                all_chip_count,
                None,
                self.game.is_none(),
                cx.listener(|this, _, _, cx| {
                    this.game = None;
                    cx.notify();
                }),
            );
            let per_game = game_counts.iter().enumerate().map(|(i, (game_name, count))| {
                let selected = self.game.as_deref() == Some(game_name.as_str());
                let leading = game_chip_icon(game_name, &icons_dir);
                let target = game_name.clone();
                filter_chip(
                    ("clips-game", i),
                    game_name.clone(),
                    *count,
                    Some(leading),
                    selected,
                    cx.listener(move |this, _, _, cx| {
                        this.game = Some(target.clone());
                        cx.notify();
                    }),
                )
            });
            div()
                .id("clips-game-chips")
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .child(all_chip)
                .children(per_game)
        });

        let filters = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_x_3()
            .gap_y_2()
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
            .children(game_chips);

        // Storage location row: current folder path + refresh, folder picker, open in Explorer.
        let storage_row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(6.))
            .rounded(px(theme::RADIUS_MD))
            .bg(rgb(theme::SURFACE_INSET))
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(icons::icon("folder-open", theme::MUTED_FG))
            .child(
                div()
                    .flex_1()
                    .min_w(px(140.))
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::FG_SOFT))
                    .truncate()
                    .child(dir.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(button_ghost("clips-refresh", "Refresh").h(px(28.)).on_click(
                        cx.listener(|this, _, _, cx| {
                            let _ = this.app.update(cx, |app, _| app.clips.invalidate());
                            cx.notify();
                        }),
                    ))
                    .child(button("clips-change", "Change folder").h(px(28.)).on_click(
                        cx.listener(|this, _, _, cx| {
                            let _ = this.app.update(cx, |app, cx| {
                                app.choose_output_dir(cx);
                                cx.notify();
                            });
                        }),
                    ))
                    .child(button_ghost("clips-open", "Open folder").h(px(28.)).on_click({
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
                    })),
            );

        // Compute responsive card width so cards fit cleanly at 760x500 and wide monitors alike.
        let avail_w = (f32::from(window.viewport_size().width) - 2.0 * PAGE_PAD - 12.0).max(320.0);
        let cols = (((avail_w + CARD_GAP) / (CARD_MIN_W + CARD_GAP)) as usize).clamp(2, 5);
        let card_w = ((avail_w - CARD_GAP * (cols - 1) as f32) / cols as f32).floor();

        let body = if clips.files.is_empty() {
            let open_dir = dir.clone();
            empty_state(
                "No clips saved yet",
                "Saved clips and screenshots from your sessions will appear here.",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(button("clips-empty-open", "Open folder").on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Err(e) = open_folder(&open_dir) {
                                let _ = this.app.update(cx, |app, cx| {
                                    app.notify(
                                        NoticeLevel::Error,
                                        format!("Couldn't open {}: {e}", open_dir.display()),
                                    );
                                    cx.notify();
                                });
                            }
                        },
                    )))
                    .child(button_ghost("clips-empty-change", "Change folder").on_click(
                        cx.listener(|this, _, _, cx| {
                            let _ = this.app.update(cx, |app, cx| {
                                app.choose_output_dir(cx);
                                cx.notify();
                            });
                        }),
                    )),
            )
        } else if shown.is_empty() {
            let trimmed = self.query.trim();
            let detail = if trimmed.is_empty() {
                "No clips match the active filters.".to_owned()
            } else {
                format!("No clips match \u{201c}{trimmed}\u{201d}.")
            };
            empty_state(
                "No matching clips",
                detail,
                div().pt_1().child(button("clips-clear-filters", "Clear filters").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.query.clear();
                        this.kind = Kind::All;
                        this.game = None;
                        cx.notify();
                    }),
                )),
            )
        } else {
            let mut global_idx: usize = 0;
            let mut shot_idx: usize = 0;
            let mut sections = Vec::with_capacity(groups.len());
            for (group_idx, (group_title, files)) in groups.into_iter().enumerate() {
                let heading = group_heading(&group_title, files.len(), group_idx == 0);
                let section_body = if self.grid {
                    let mut cards = Vec::with_capacity(files.len());
                    for file in files {
                        let is_video = Kind::Videos.accepts(&file.name);
                        let (thumb, nav) = if is_video {
                            (thumb_dir.as_ref().and_then(|dir| self.thumbs.get(dir, file)), None)
                        } else {
                            let nav = Some((Rc::clone(&screenshots), shot_idx));
                            shot_idx += 1;
                            (Some(file.path.clone()), nav)
                        };
                        cards.push(self.render_card(global_idx, file, thumb, card_w, nav, cx));
                        global_idx += 1;
                    }
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .content_start()
                        .gap(px(CARD_GAP))
                        .children(cards)
                        .into_any_element()
                } else {
                    let mut rows = Vec::with_capacity(files.len());
                    for file in files {
                        let is_video = Kind::Videos.accepts(&file.name);
                        let (thumb, nav) = if is_video {
                            (thumb_dir.as_ref().and_then(|dir| self.thumbs.get(dir, file)), None)
                        } else {
                            let nav = Some((Rc::clone(&screenshots), shot_idx));
                            shot_idx += 1;
                            (Some(file.path.clone()), nav)
                        };
                        rows.push(self.render_list_row(global_idx, file, thumb, nav, cx));
                        global_idx += 1;
                    }
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(theme::RADIUS))
                        .bg(rgb(theme::CARD))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .overflow_hidden()
                        .child(
                            table_row("File", "Size", "Saved", None::<gpui::Div>)
                                .text_size(px(theme::TEXT_MICRO))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::MUTED_FG)),
                        )
                        .children(rows)
                        .into_any_element()
                };
                sections.push(div().flex().flex_col().gap_3().child(heading).child(section_body));
            }
            let scroll_id = if self.grid { "clips-grid" } else { "clips-list" };
            div()
                .id(scroll_id)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().flex().flex_col().gap_5().pb_4().children(sections))
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
                .flex_col()
                .gap_2()
                .p_3()
                .rounded(px(theme::RADIUS))
                .bg(rgb(theme::SURFACE_RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER_STRONG))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    match ev.keystroke.key.as_str() {
                        "enter" => this.commit_rename(cx),
                        "escape" => {
                            this.renaming = None;
                            cx.notify();
                        }
                        _ => {}
                    }
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(theme::TEXT_CAPTION))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::FG_SOFT))
                                .child("Rename clip"),
                        )
                        .child(text_field(
                            "clip-rename",
                            &rename_focus,
                            window,
                            &rename.text,
                            "New name",
                            280.,
                            cx.listener(|this, v: &String, _, cx| {
                                if let Some(rename) = this.renaming.as_mut() {
                                    rename.text = v.clone();
                                    rename.error = None;
                                }
                                cx.notify();
                            }),
                        ))
                        .child(
                            div()
                                .text_size(px(theme::TEXT_CAPTION))
                                .text_color(rgb(theme::MUTED_FG))
                                .child(ext),
                        )
                        .child(
                            button_primary("clip-rename-save", "Save name")
                                .on_click(cx.listener(|this, _, _, cx| this.commit_rename(cx))),
                        )
                        .child(button_ghost("clip-rename-cancel", "Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.renaming = None;
                                cx.notify();
                            },
                        ))),
                )
                .children(rename.error.as_ref().map(|e| {
                    div()
                        .text_size(px(theme::TEXT_CAPTION))
                        .text_color(rgb(theme::DANGER))
                        .child(e.clone())
                }))
        });

        let list_error = clips.error.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p(px(PAGE_PAD))
            .overflow_hidden()
            .child(toolbar)
            .child(filters)
            .child(storage_row)
            .children(list_error.map(|error| {
                div()
                    .px_3()
                    .py_2()
                    .rounded(px(theme::RADIUS_MD))
                    .bg(rgb(theme::DANGER_SOFT))
                    .border_1()
                    .border_color(rgb(theme::DANGER_BORDER))
                    .text_size(px(theme::TEXT_CAPTION))
                    .text_color(rgb(theme::DANGER))
                    .child(error)
            }))
            .children(rename_bar)
            .child(body)
            .into_any_element()
    }
}

fn empty_state(
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    action: impl IntoElement,
) -> gpui::AnyElement {
    div()
        .id("clips-empty")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .items_center()
        .justify_center()
        .py_6()
        .child(
            div()
                .w_full()
                .max_w(px(400.))
                .px_6()
                .py_8()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .rounded(px(theme::RADIUS_LG))
                .bg(rgb(theme::CARD))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    div()
                        .size(px(40.))
                        .mb_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(theme::RADIUS_MD))
                        .bg(rgb(theme::SURFACE_RAISED))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .child(icons::icon("film", theme::MUTED_FG)),
                )
                .child(theme::section_heading(title))
                .child(theme::caption(detail).text_center())
                .child(action),
        )
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
        .items_center()
        .gap_4()
        .px_3()
        .py_2()
        .child(div().flex_1().min_w_0().child(name))
        .child(div().w(px(76.)).flex_none().child(size))
        .child(div().w(px(96.)).flex_none().child(saved))
        .child(div().w(px(212.)).flex_none().flex().justify_end().children(action))
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
        assert!(renamed_path(path, "con").is_err());
        assert!(renamed_path(path, "COM3.txt").is_err());
        assert!(renamed_path(path, "COM10").is_ok());
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

    #[test]
    fn rescan_walks_subfolders_skips_hidden_and_caps_depth() {
        let root = std::env::temp_dir().join(format!("zc-clips-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d1 = root.join("Valorant");
        let d4 = root.join("a").join("b").join("c").join("d");
        let d5 = d4.join("e");
        let hidden = root.join(".hidden");
        for d in [&d1, &d4, &d5, &hidden] {
            std::fs::create_dir_all(d).expect("create dir");
        }
        std::fs::write(root.join("top.mp4"), b"1").expect("top");
        std::fs::write(root.join(".dot.mp4"), b"1").expect("dot file");
        std::fs::write(d1.join("sub.mkv"), b"1").expect("sub");
        std::fs::write(d4.join("depth4.png"), b"1").expect("depth4");
        std::fs::write(d5.join("too_deep.mp4"), b"1").expect("depth5");
        std::fs::write(hidden.join("secret.mp4"), b"1").expect("hidden");

        let mut list = ClipList::default();
        list.rescan(&root);
        let mut found: Vec<String> = list.files.iter().map(|f| f.name.clone()).collect();
        found.sort();

        let mut missing = ClipList::default();
        missing.rescan(&root.join("does-not-exist"));
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(found, ["depth4.png", "sub.mkv", "top.mp4"]);
        assert!(list.error.is_none());
        assert!(missing.files.is_empty() && missing.error.is_none());
    }

    #[test]
    fn game_chip_counts_tallies_filtered_clips_and_orders_other_last() {
        let root = Path::new("clips");
        let files = [
            ClipFile {
                name: "v1.mp4".into(),
                path: root.join("v1.mp4"),
                size_bytes: 1,
                modified: None,
                game: Some("Valorant.exe".into()),
            },
            ClipFile {
                name: "s1.png".into(),
                path: root.join("s1.png"),
                size_bytes: 1,
                modified: None,
                game: Some("Valorant".into()),
            },
            ClipFile {
                name: "c1.mp4".into(),
                path: root.join("c1.mp4"),
                size_bytes: 1,
                modified: None,
                game: Some("cs2.exe".into()),
            },
            ClipFile {
                name: "other.png".into(),
                path: root.join("other.png"),
                size_bytes: 1,
                modified: None,
                game: None,
            },
        ];

        // All kinds: sum of game chips equals total count, alphabetical with Other last.
        let all_filtered = filter_files(&files, Kind::All, "", None);
        let (all_total, chips) = game_chip_counts(&files, &all_filtered, root);
        assert_eq!(all_total, 4);
        assert_eq!(chips, vec![("cs2".into(), 1), ("Valorant".into(), 2), (OTHER.into(), 1)]);

        // Screenshots only: cs2 stays listed with count 0, Valorant has 1, Other has 1.
        let shots_filtered = filter_files(&files, Kind::Screenshots, "", None);
        let (shots_total, shot_chips) = game_chip_counts(&files, &shots_filtered, root);
        assert_eq!(shots_total, 2);
        assert_eq!(shot_chips, vec![("cs2".into(), 0), ("Valorant".into(), 1), (OTHER.into(), 1)]);
    }

    #[test]
    fn game_chip_icon_uses_folder_for_other_and_prefers_cached_exe_icon() {
        let dir = std::env::temp_dir().join(format!("zc-clips-chip-icon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp icon dir");
        let cs2 = dir.join("cs2.exe.png");
        std::fs::write(&cs2, b"png").expect("write icon");

        assert_eq!(game_chip_icon(OTHER, &dir), ChipIcon::Svg("folder-open"));
        assert_eq!(game_chip_icon("cs2", &dir), ChipIcon::Image(cs2));
        assert_eq!(game_chip_icon("UnknownGame", &dir), ChipIcon::Svg("gamepad-2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ordered_screenshots_follows_group_and_sort_order_and_skips_videos() {
        let root = Path::new("clips");
        let files = [
            ClipFile {
                name: "b.png".into(),
                path: root.join("b.png"),
                size_bytes: 1,
                modified: None,
                game: Some("Valorant".into()),
            },
            ClipFile {
                name: "clip.mp4".into(),
                path: root.join("clip.mp4"),
                size_bytes: 1,
                modified: None,
                game: Some("Valorant".into()),
            },
            ClipFile {
                name: "a.jpg".into(),
                path: root.join("a.jpg"),
                size_bytes: 1,
                modified: None,
                game: Some("Valorant".into()),
            },
            ClipFile {
                name: "c.png".into(),
                path: root.join("c.png"),
                size_bytes: 1,
                modified: None,
                game: Some("CS2".into()),
            },
        ];
        let shown: Vec<&ClipFile> = files.iter().collect();
        let groups = group_files(&shown, root, SortOrder::Name);
        let ordered: Vec<String> =
            ordered_screenshots(&groups).into_iter().map(|f| f.name).collect();
        assert_eq!(ordered, ["c.png", "a.jpg", "b.png"]);
    }
}
