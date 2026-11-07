#![allow(dead_code)] // read once the Clips screen is ported

//! The clip list. For now a plain folder listing; the library (thumbnails,
//! metadata from zc-store), the editor and sharing arrive in Phases 4–6.

use std::path::Path;
use std::time::SystemTime;

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

/// Placeholder until the Clips screen is ported to GPUI.
pub struct ClipsView {
    #[allow(dead_code)] // read once the screen has content
    app: gpui::WeakEntity<crate::app::ZeroClipApp>,
}

impl ClipsView {
    pub fn new(app: gpui::WeakEntity<crate::app::ZeroClipApp>) -> Self {
        Self { app }
    }
}

impl gpui::Render for ClipsView {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div().p_4().child("Clips")
    }
}
