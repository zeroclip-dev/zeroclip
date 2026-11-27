-- ZeroClip metadata, schema v1.
-- Never edit this file once it has shipped: add 0002_<name>.sql instead.
-- Times are Unix seconds; durations and PTS are 100 ns ticks ("hns"), as in the code.

CREATE TABLE clips (
    id            INTEGER PRIMARY KEY,
    path          TEXT    NOT NULL UNIQUE,
    kind          TEXT    NOT NULL CHECK (kind IN ('dvr', 'manual', 'session', 'export')),
    created_at    INTEGER NOT NULL,
    duration_hns  INTEGER NOT NULL,
    size_bytes    INTEGER NOT NULL,
    codec         TEXT    NOT NULL CHECK (codec IN ('av1', 'hevc', 'h264')),
    width         INTEGER NOT NULL,
    height        INTEGER NOT NULL,
    fps           INTEGER NOT NULL,
    game_exe      TEXT,
    -- For exports: the clip this one was cut from.
    source_clip   INTEGER REFERENCES clips (id) ON DELETE SET NULL
);
CREATE INDEX clips_by_created_at ON clips (created_at DESC);

-- Keyframe positions: the editor's snap points.
CREATE TABLE clip_keyframes (
    clip_id  INTEGER NOT NULL REFERENCES clips (id) ON DELETE CASCADE,
    pts_hns  INTEGER NOT NULL,
    PRIMARY KEY (clip_id, pts_hns)
) WITHOUT ROWID;

-- Per-game profiles.
CREATE TABLE game_profiles (
    exe                TEXT    PRIMARY KEY COLLATE NOCASE,
    buffer_secs        INTEGER NOT NULL,
    session_recording  INTEGER NOT NULL DEFAULT 0,
    last_seen_at       INTEGER
);

-- The encoder capability ladder, re-probed only on GPU or driver change.
CREATE TABLE encoder_probe (
    adapter_luid    TEXT    NOT NULL,
    driver_version  TEXT    NOT NULL,
    codecs          TEXT    NOT NULL,  -- comma-separated, best first: 'av1,hevc,h264'
    probed_at       INTEGER NOT NULL,
    PRIMARY KEY (adapter_luid, driver_version)
);

-- Waveform peaks, keyed by a hash of the file, so reopening a clip is instant.
CREATE TABLE waveform_cache (
    file_hash  TEXT    PRIMARY KEY,
    buckets    INTEGER NOT NULL,
    peaks      BLOB    NOT NULL  -- little-endian f32 per bucket
);

-- Uploads. `verified_at` is set only after a status check confirms the
-- remote copy is processed and not rejected; local deletion requires it.
CREATE TABLE uploads (
    id           INTEGER PRIMARY KEY,
    clip_id      INTEGER NOT NULL REFERENCES clips (id) ON DELETE CASCADE,
    destination  TEXT    NOT NULL,
    status       TEXT    NOT NULL CHECK (status IN ('queued', 'uploading', 'verifying', 'done', 'failed')),
    remote_id    TEXT,
    url          TEXT,
    verified_at  INTEGER,
    error        TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX uploads_by_clip ON uploads (clip_id);
