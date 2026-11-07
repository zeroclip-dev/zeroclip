//! The clip list. For now a plain folder listing; the library (thumbnails,
//! metadata from zc-store), the editor and sharing arrive in Phases 4–6.

use std::path::Path;
use std::time::SystemTime;

use gpui::{
    Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px, rgb,
};

use super::components::button::button;
use super::{GRAY, RED};
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
    size_bytes: u64,
    modified: Option<SystemTime>,
}

impl ClipList {
    /// Marks the listing stale; it is re-read the next time the tab is shown.
    pub fn invalidate(&mut self) {
        self.scanned = false;
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
                    size_bytes: meta.len(),
                    modified: meta.modified().ok(),
                });
            }
        }
        self.files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    }
}

/// The Clips screen: the folder listing with Refresh and Open folder.
pub struct ClipsView {
    app: WeakEntity<ZeroClipApp>,
}

impl ClipsView {
    pub fn new(app: WeakEntity<ZeroClipApp>) -> Self {
        Self { app }
    }
}

impl Render for ClipsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entity) = self.app.upgrade() else {
            return div().into_any_element();
        };
        let dir = entity.read(cx).output_dir();
        // The listing lives on the app (other screens invalidate it), so it is re-read here,
        // lazily, the first time the tab is shown after a change.
        entity.update(cx, |app, _| {
            if !app.clips.scanned {
                app.clips.rescan(&dir);
            }
        });
        let clips = &entity.read(cx).clips;

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Clips"))
            .child(button("clips-refresh", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                let _ = this.app.update(cx, |app, _| app.clips.invalidate());
                cx.notify();
            })))
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

        let body = if clips.files.is_empty() {
            div()
                .text_color(rgb(GRAY))
                .child("No clips yet. Saved clips and screenshots will show up here.")
                .into_any_element()
        } else {
            let rows = clips.files.iter().enumerate().map(|(i, file)| {
                let stripe = if i % 2 == 1 { 0x24242a } else { 0x1b1b1f };
                table_row(
                    file.name.clone(),
                    format!("{:.1} MB", file.size_bytes as f64 / 1_000_000.0),
                    file.modified.map_or_else(|| "—".to_owned(), age),
                )
                .bg(rgb(stripe))
            });
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(table_row("File", "Size", "Saved").font_weight(FontWeight::BOLD))
                .child(
                    div()
                        .id("clips-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .child(header)
            .child(div().text_color(rgb(GRAY)).child(dir.display().to_string()))
            .children(
                clips.error.as_ref().map(|error| div().text_color(rgb(RED)).child(error.clone())),
            )
            .child(body)
            .into_any_element()
    }
}

/// One row of the table: the name takes the spare width, size and age keep fixed widths.
fn table_row(name: impl IntoElement, size: impl IntoElement, saved: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap_6()
        .py_1()
        .child(div().flex_1().min_w_0().child(name))
        .child(div().w(px(90.)).child(size))
        .child(div().w(px(120.)).child(saved))
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
