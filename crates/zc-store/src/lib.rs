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
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
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
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite refused an operation (I/O, a corrupt file, a bad statement).
    #[error("metadata store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The file was written by a newer ZeroClip. This build does not know its schema.
    #[error(
        "the metadata database is from a newer ZeroClip (schema v{found}; this build knows v{known})"
    )]
    TooNew {
        /// Version stored in the file.
        found: u32,
        /// Newest version this build knows.
        known: u32,
    },
}

/// The metadata database.
///
/// `rusqlite::Connection` is `Send` but not `Sync`. Share one store between threads as
/// `Arc<Mutex<Store>>`. The store is never on the capture or audio hot path, so a lock is fine.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the database at `path` and applies pending migrations.
    ///
    /// The connection uses WAL mode (readers do not block the writer), waits up to 5 s on a
    /// busy database, and enforces foreign keys (off by default in SQLite).
    ///
    /// # Errors
    /// SQLite errors, or a database from a newer ZeroClip.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // journal_mode returns a row, so plain pragma_update fails with "Execute returned results".
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        // Off by default per connection. The schema's ON DELETE CASCADE needs it.
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The schema version stored in the file (`PRAGMA user_version`).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self.conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// The cached waveform peaks for the file keyed `key` (`zc_editor::waveform::file_key`),
    /// if they were stored with exactly `buckets` buckets.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn waveform(&self, key: &str, buckets: usize) -> Result<Option<Vec<f32>>, StoreError> {
        let buckets = i64::try_from(buckets).unwrap_or(i64::MAX);
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT peaks FROM waveform_cache WHERE file_hash = ?1 AND buckets = ?2",
                (key, buckets),
                |r| r.get(0),
            )
            .optional()?;
        Ok(blob.map(|b| {
            b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
        }))
    }

    /// Stores (or replaces) the waveform peaks of the file keyed `key`.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn put_waveform(&self, key: &str, peaks: &[f32]) -> Result<(), StoreError> {
        let blob: Vec<u8> = peaks.iter().flat_map(|p| p.to_le_bytes()).collect();
        let buckets = i64::try_from(peaks.len()).unwrap_or(i64::MAX);
        self.conn.execute(
            "INSERT OR REPLACE INTO waveform_cache (file_hash, buckets, peaks) VALUES (?1, ?2, ?3)",
            (key, buckets, blob),
        )?;
        Ok(())
    }
}

/// Applies every migration past the file's `user_version`, one transaction per migration.
///
/// A migration and its version bump commit together, so a crash never leaves a half-applied
/// schema that claims to be complete.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let found: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let known = u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX);
    if found > known {
        return Err(StoreError::TooNew { found, known });
    }
    for (i, (_name, sql)) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        let version = u32::try_from(i + 1).unwrap_or(u32::MAX);
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
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

    /// A fresh database path for one test (the directory is recreated each run).
    fn scratch_db(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeroclip-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("metadata.db")
    }

    #[test]
    fn store_open_applies_every_migration() {
        let path = scratch_db("every-migration");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), u32::try_from(MIGRATIONS.len()).unwrap());
        let tables: i64 = store
            .conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'clips'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn store_reopen_is_idempotent() {
        let path = scratch_db("reopen");
        let first = Store::open(&path).unwrap().schema_version().unwrap();
        let second = Store::open(&path).unwrap().schema_version().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn store_refuses_a_newer_schema() {
        let path = scratch_db("newer");
        drop(Store::open(&path).unwrap());
        let raw = Connection::open(&path).unwrap();
        raw.pragma_update(None, "user_version", 99).unwrap();
        drop(raw);
        let err = Store::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::TooNew { found: 99, .. }), "{err}");
    }

    #[test]
    fn store_enables_foreign_keys() {
        let path = scratch_db("foreign-keys");
        let store = Store::open(&path).unwrap();
        let on: i64 = store.conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(on, 1);
    }

    #[test]
    fn waveform_round_trips_and_misses_cleanly() {
        let store = Store::open(&scratch_db("waveform")).unwrap();
        assert_eq!(store.waveform("k", 3).unwrap(), None);
        store.put_waveform("k", &[0.0, 0.5, 1.0]).unwrap();
        assert_eq!(store.waveform("k", 3).unwrap(), Some(vec![0.0, 0.5, 1.0]));
        // A different bucket count is a different waveform.
        assert_eq!(store.waveform("k", 4).unwrap(), None);
        assert_eq!(store.waveform("other", 3).unwrap(), None);
        // Replacing keeps one row per file.
        store.put_waveform("k", &[0.25; 4]).unwrap();
        assert_eq!(store.waveform("k", 4).unwrap(), Some(vec![0.25; 4]));
        assert_eq!(store.waveform("k", 3).unwrap(), None);
    }
}
