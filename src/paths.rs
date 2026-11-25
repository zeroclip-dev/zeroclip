//! Where ZeroClip keeps its files.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use directories::{ProjectDirs, UserDirs};

/// Resolved locations. On Windows:
///
/// | What | Where |
/// |---|---|
/// | settings | `%APPDATA%\ZeroClip\config\settings.toml` |
/// | logs | `%LOCALAPPDATA%\ZeroClip\data\logs\` |
/// | metadata database | `%LOCALAPPDATA%\ZeroClip\data\zeroclip.db` |
/// | recordings (default) | `<Videos>\ZeroClip\` |
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// The settings file.
    pub settings_file: PathBuf,
    /// Rolling log files.
    pub logs_dir: PathBuf,
    /// Program icons, as PNGs, for the game list.
    pub icons_dir: PathBuf,
    /// The metadata database (waveform cache).
    pub db_file: PathBuf,
    /// Where recordings go unless the settings say otherwise.
    pub default_output_dir: PathBuf,
}

impl AppPaths {
    /// Resolves the standard locations for the current user.
    pub fn resolve() -> anyhow::Result<Self> {
        let project = ProjectDirs::from("", "", "ZeroClip")
            .context("couldn't find the user's home folder")?;
        let videos = UserDirs::new()
            .and_then(|user| user.video_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| project.data_local_dir().to_path_buf());
        Ok(Self {
            settings_file: project.config_dir().join("settings.toml"),
            logs_dir: project.data_local_dir().join("logs"),
            icons_dir: project.data_local_dir().join("icons"),
            db_file: project.data_local_dir().join("zeroclip.db"),
            default_output_dir: videos.join("ZeroClip"),
        })
    }
}
