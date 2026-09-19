//! The clip list. For now a plain folder listing; the library (thumbnails,
//! metadata from zc-store), the editor and sharing arrive in Phases 4–6.

use std::path::Path;
use std::time::SystemTime;

use eframe::egui;

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

pub fn show(app: &mut ZeroClipApp, ui: &mut egui::Ui) {
    let dir = app.output_dir();
    if !app.clips.scanned {
        app.clips.rescan(&dir);
    }

    ui.horizontal(|ui| {
        ui.heading("Clips");
        if ui.button("Refresh").clicked() {
            app.clips.invalidate();
        }
        if ui.button("Open folder").clicked()
            && let Err(e) = open_folder(&dir)
        {
            app.notify(NoticeLevel::Error, format!("Couldn't open {}: {e}", dir.display()));
        }
    });
    ui.weak(dir.display().to_string());
    ui.add_space(8.0);

    if let Some(error) = &app.clips.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
    if app.clips.files.is_empty() {
        ui.weak("No clips yet. Saved clips and screenshots will show up here.");
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        egui::Grid::new("clips").num_columns(3).striped(true).spacing([24.0, 6.0]).show(ui, |ui| {
            ui.strong("File");
            ui.strong("Size");
            ui.strong("Saved");
            ui.end_row();
            for file in &app.clips.files {
                ui.label(&file.name);
                ui.label(format!("{:.1} MB", file.size_bytes as f64 / 1_000_000.0));
                ui.label(file.modified.map_or_else(|| "—".to_owned(), age));
                ui.end_row();
            }
        });
    });
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
