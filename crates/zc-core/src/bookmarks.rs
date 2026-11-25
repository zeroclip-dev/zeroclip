//! Bookmarks: moments marked while a recording runs, kept beside the video so the editor and the
//! player can show them. One plain-text file per video, one line per bookmark: the time in
//! milliseconds from the start of the video. Appending a line is all a bookmark costs, so a crash
//! never loses the earlier ones.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::time::Hns;

/// Where `video`'s bookmarks live: the same name with the extension `bookmarks`.
#[must_use]
pub fn sidecar(video: &Path) -> PathBuf {
    video.with_extension("bookmarks")
}

/// The bookmarks of `video`, earliest first. A missing or unreadable file is no bookmarks, and
/// lines that aren't numbers are skipped.
#[must_use]
pub fn load(video: &Path) -> Vec<Hns> {
    let text = std::fs::read_to_string(sidecar(video)).unwrap_or_default();
    let mut marks: Vec<Hns> =
        text.lines().filter_map(|l| l.trim().parse::<i64>().ok()).map(Hns::from_millis).collect();
    marks.sort();
    marks
}

/// Adds a bookmark at `at` to `video`'s file.
///
/// # Errors
/// When the file can't be opened or written.
pub fn add(video: &Path, at: Hns) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(sidecar(video))?;
    writeln!(file, "{}", at.as_millis().max(0))
}

/// Moves `from`'s bookmarks along when the video is renamed to `to`. Nothing to move is fine.
pub fn rename(from: &Path, to: &Path) {
    let (old, new) = (sidecar(from), sidecar(to));
    if old.is_file() {
        let _ = std::fs::rename(old, new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_are_kept_in_order_and_follow_a_rename() {
        let dir = std::env::temp_dir().join(format!("zc-bookmarks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("a clip.mp4");
        assert!(load(&video).is_empty());
        add(&video, Hns::from_secs(30)).unwrap();
        add(&video, Hns::from_secs(5)).unwrap();
        assert_eq!(load(&video), [Hns::from_secs(5), Hns::from_secs(30)]);
        let renamed = dir.join("b clip.mp4");
        rename(&video, &renamed);
        assert!(load(&video).is_empty());
        assert_eq!(load(&renamed).len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
