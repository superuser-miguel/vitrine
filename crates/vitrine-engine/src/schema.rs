//! Database schema as forward-only, numbered migrations (PLAN §5).
//!
//! `MIGRATIONS[i]` is applied to move the database from version `i` to `i+1`,
//! tracked by SQLite's `PRAGMA user_version`. **Never edit an applied
//! migration** — append a new one. The current target version is
//! [`SCHEMA_VERSION`].
//!
//! All tag/rating rows key on `content_hash` (BLAKE3 hex), *not* path, so tags
//! survive gallery-dl renames/moves; the scanner reconciles paths. The
//! `sync_state` / `source` columns are written with defaults and never read in
//! v1 — dormant seams so v2 metadata-write and the rule engine stay additive.

/// Forward-only migrations; index `i` reaches schema version `i + 1`.
pub const MIGRATIONS: &[&str] = &[
    // v1 — initial schema.
    r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE files (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,
  content_hash  TEXT NOT NULL,
  phash         INTEGER,
  size          INTEGER NOT NULL,
  mtime         INTEGER NOT NULL,
  width         INTEGER,
  height        INTEGER,
  format        TEXT,
  date_taken    INTEGER,
  camera        TEXT,
  orientation   INTEGER,
  indexed_at    INTEGER NOT NULL,
  missing       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_files_hash  ON files(content_hash);
CREATE INDEX idx_files_phash ON files(phash);
CREATE INDEX idx_files_date  ON files(date_taken);

CREATE TABLE tags (
  id    INTEGER PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE file_tags (
  content_hash TEXT NOT NULL,
  tag_id       INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  sync_state   TEXT NOT NULL DEFAULT 'db-only',
  source       TEXT NOT NULL DEFAULT 'manual',
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (content_hash, tag_id)
);

CREATE TABLE ratings (
  content_hash TEXT PRIMARY KEY,
  rating       INTEGER NOT NULL CHECK (rating BETWEEN 0 AND 5),
  sync_state   TEXT NOT NULL DEFAULT 'db-only',
  updated_at   INTEGER NOT NULL
);

CREATE TABLE collections (
  id     INTEGER PRIMARY KEY,
  name   TEXT NOT NULL,
  kind   TEXT NOT NULL CHECK (kind IN ('smart','catalog')),
  query  TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE collection_items (
  collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
  content_hash  TEXT NOT NULL,
  position      INTEGER NOT NULL,
  PRIMARY KEY (collection_id, content_hash)
);
"#,
    // v2 — per-image free-text comment (Phase 3). Mirrors `ratings`:
    // content-hash keyed so it survives renames; `sync_state` seam for the
    // deferred v2 embedded-write (dc:description). See PLAN Phase 3 task 2a.
    r#"
CREATE TABLE comments (
  content_hash TEXT PRIMARY KEY,
  body         TEXT NOT NULL,
  sync_state   TEXT NOT NULL DEFAULT 'db-only',
  updated_at   INTEGER NOT NULL
);
"#,
];

/// The schema version this build targets (number of migrations).
pub const SCHEMA_VERSION: usize = MIGRATIONS.len();
