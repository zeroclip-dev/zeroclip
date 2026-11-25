//! Loading and saving `settings.toml`.

use std::path::Path;

use anyhow::Context as _;
use zc_core::settings::Settings;

/// Loads settings, falling back to defaults when the file is missing. A file that
/// doesn't parse is moved aside to `settings.toml.bak` (so the user's edits aren't
/// lost) and defaults are used.
pub fn load(path: &Path) -> Settings {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no settings file yet; using defaults");
            return Settings::default();
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "couldn't read settings; using defaults");
            return Settings::default();
        }
    };
    match toml::from_str(&text) {
        Ok(settings) => settings,
        Err(e) => {
            let backup = path.with_extension("toml.bak");
            tracing::warn!(error = %e, backup = %backup.display(), "settings file is invalid; using defaults");
            if let Err(e) = std::fs::rename(path, &backup) {
                tracing::warn!(error = %e, "couldn't back up the invalid settings file");
            }
            Settings::default()
        }
    }
}

/// Saves settings atomically: write a temporary file, then rename it over the
/// old one, so a crash mid-save never leaves a truncated file.
pub fn save(path: &Path, settings: &Settings) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("couldn't create {}", dir.display()))?;
    }
    let text = toml::to_string_pretty(settings).context("couldn't serialise settings")?;
    let tmp = path.with_extension("toml.tmp");
    // sync_all before the rename, or a power cut can leave the new name pointing at empty data.
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("couldn't write {}", tmp.display()))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("couldn't write {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("couldn't replace {}", path.display()))?;
    tracing::info!(path = %path.display(), "settings saved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A fresh, empty directory for one test.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zeroclip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = scratch_dir("missing");
        assert_eq!(load(&dir.join("settings.toml")), Settings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = scratch_dir("round-trip");
        let path = dir.join("nested").join("settings.toml");
        let mut settings = Settings::default();
        settings.video.fps = 144;
        save(&path, &settings).unwrap();
        assert_eq!(load(&path), settings);
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn invalid_file_is_backed_up_and_replaced_by_defaults() {
        let dir = scratch_dir("invalid");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "video = [not toml").unwrap();
        assert_eq!(load(&path), Settings::default());
        assert!(path.with_extension("toml.bak").exists());
    }
}
