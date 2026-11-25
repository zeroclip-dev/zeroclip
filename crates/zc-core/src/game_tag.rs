//! Which game a clip came from, kept beside the video in a one-line `.game` file so the Clips
//! screen can group by game. Clips made before this existed simply have no tag.

use std::path::{Path, PathBuf};

fn sidecar(video: &Path) -> PathBuf {
    video.with_extension("game")
}

/// The game `video` was recorded from, if it was tagged.
#[must_use]
pub fn load(video: &Path) -> Option<String> {
    let text = std::fs::read_to_string(sidecar(video)).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Tags `video` with `game`. A failure only means the clip lands under "Other", so it is ignored.
pub fn save(video: &Path, game: &str) {
    let _ = std::fs::write(sidecar(video), game);
}

/// Moves the tag along when the video is renamed to `to`.
pub fn rename(from: &Path, to: &Path) {
    let old = sidecar(from);
    if old.is_file() {
        let _ = std::fs::rename(old, sidecar(to));
    }
}

/// Deletes the tag of a video that was deleted.
pub fn remove(video: &Path) {
    let _ = std::fs::remove_file(sidecar(video));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trips_and_follows_a_rename() {
        let dir = std::env::temp_dir().join(format!("zc-gametag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.mp4"), dir.join("b.mp4"));
        assert_eq!(load(&a), None);
        save(&a, "War Thunder");
        assert_eq!(load(&a).as_deref(), Some("War Thunder"));
        rename(&a, &b);
        assert_eq!((load(&a), load(&b).as_deref()), (None, Some("War Thunder")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
