//! # zc-store — the metadata store (DESIGN §2 "Metadata", Phase 4)
//!
//! An embedded SQLite database (via `rusqlite` with the `bundled` feature, added
//! in Phase 4) holding everything that isn't a video file:
//!
//! - clips and their keyframe positions (snap points for the editor, DESIGN §9);
//! - game profiles (DESIGN §7), once they outgrow the settings file;
//! - the encoder capability probe, re-run only on GPU or driver change (DESIGN §3);
//! - waveform peaks keyed by file hash, so reopening a clip is instant (DESIGN §9);
//! - uploads and their verification state (DESIGN §10).
//!
//! The schema is in `migrations/`. Never edit a migration that has shipped; add
//! the next numbered file instead.

use std::path::{Path, PathBuf};

use zc_core::Hns;
use zc_core::media::Codec;

/// Schema migrations, in order: `(name, SQL)`.
pub const MIGRATIONS: &[(&str, &str)] =
    &[("0001_init", include_str!("../migrations/0001_init.sql"))];

/// How a clip came to exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClipKind {
    /// Saved from the DVR buffer.
    Dvr,
    /// Manual start/stop recording.
    Manual,
    /// Automatic session recording.
    Session,
    /// Produced by the editor (trim, stitch, remux).
    Export,
}

/// A row of the `clips` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipRecord {
    /// Row id.
    pub id: i64,
    /// File on disk.
    pub path: PathBuf,
    /// Origin.
    pub kind: ClipKind,
    /// Creation time, Unix seconds.
    pub created_at: i64,
    /// Length.
    pub duration: Hns,
    /// File size.
    pub size_bytes: u64,
    /// Video codec.
    pub codec: Codec,
    /// Frame size, pixels.
    pub size: (u32, u32),
    /// Frame rate.
    pub fps: u32,
    /// Game that had focus when it was recorded.
    pub game_exe: Option<String>,
}

/// Store failures.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metadata store: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

/// The metadata database.
#[derive(Debug)]
pub struct Store {
    _private: (),
}

impl Store {
    /// Opens (or creates) the database at `path` and applies pending migrations.
    ///
    /// # Errors
    /// SQLite errors, or a database from a newer ZeroClip.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let _ = path;
        todo!(
            "Phase 4: add rusqlite (bundled), open with WAL mode, run MIGRATIONS in a transaction"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_numbered_and_non_empty() {
        for (i, (name, sql)) in MIGRATIONS.iter().enumerate() {
            assert!(name.starts_with(&format!("{:04}_", i + 1)), "{name}");
            assert!(sql.contains("CREATE TABLE"), "{name} is empty");
        }
    }
}
