//! The `files` table: one row per indexed image, content-hash keyed so tags
//! survive renames. This module is the CRUD/query surface the scanner and the
//! app's ingestion use; it does no I/O beyond SQLite.

use rusqlite::{OptionalExtension, Row};

use crate::db::Db;

/// Whether `path` is an XDG document-portal path (`/run/user/<uid>/doc/<id>/…`).
///
/// These are **handles, not locations**. The portal exposes a host file under an
/// opaque per-document path that stops resolving once the grant lapses — so a row
/// keyed to one can point at nothing while the real file is untouched. A folder
/// opened through the file chooser is indexed under such a path; the same file
/// reached through a directly-granted root keeps its real one.
pub fn is_portal_document_path(path: &str) -> bool {
    let rest = match path.strip_prefix("/run/user/") {
        Some(rest) => rest,
        None => return false,
    };
    match rest.split_once('/') {
        Some((uid, tail)) => uid.chars().all(|c| c.is_ascii_digit()) && tail.starts_with("doc/"),
        None => false,
    }
}

/// Prefer the durable path wherever the same content is indexed more than once.
///
/// Drops document-portal rows when a non-portal row for the **same content hash**
/// exists to stand for the file. Two things fall out of this: a stale portal
/// handle no longer renders as a broken cell when the real file is right there,
/// and duplicate detection stops reporting one file against itself.
///
/// Grouping by hash matters — a near-duplicate cluster mixes differing hashes, so
/// dropping portal rows across a whole group could discard a genuinely distinct
/// image. Content reachable *only* through a portal path keeps its row, since
/// nothing else represents it.
pub fn prefer_durable_paths(files: Vec<FileRecord>) -> Vec<FileRecord> {
    let mut by_hash: std::collections::HashMap<String, Vec<FileRecord>> =
        std::collections::HashMap::new();
    for file in files {
        by_hash
            .entry(file.content_hash.clone())
            .or_default()
            .push(file);
    }
    let mut kept = Vec::new();
    for (_, mut group) in by_hash {
        if group.iter().any(|f| !is_portal_document_path(&f.path)) {
            group.retain(|f| !is_portal_document_path(&f.path));
        }
        kept.append(&mut group);
    }
    kept
}

/// A row of the `files` table. `id` is `None` before insertion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileRecord {
    pub id: Option<i64>,
    pub path: String,
    pub content_hash: String,
    pub phash: Option<i64>,
    pub size: i64,
    pub mtime: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub format: Option<String>,
    pub date_taken: Option<i64>,
    pub camera: Option<String>,
    pub orientation: Option<i64>,
    pub indexed_at: i64,
    pub missing: bool,
}

impl FileRecord {
    pub(crate) fn from_row(row: &Row) -> rusqlite::Result<FileRecord> {
        Ok(FileRecord {
            id: Some(row.get("id")?),
            path: row.get("path")?,
            content_hash: row.get("content_hash")?,
            phash: row.get("phash")?,
            size: row.get("size")?,
            mtime: row.get("mtime")?,
            width: row.get("width")?,
            height: row.get("height")?,
            format: row.get("format")?,
            date_taken: row.get("date_taken")?,
            camera: row.get("camera")?,
            orientation: row.get("orientation")?,
            indexed_at: row.get("indexed_at")?,
            missing: row.get::<_, i64>("missing")? != 0,
        })
    }
}

pub(crate) const SELECT_COLS: &str = "id, path, content_hash, phash, size, mtime, width, \
     height, format, date_taken, camera, orientation, indexed_at, missing";

/// The decode-derived fields written by the app's enrichment pass, once per file
/// (after identity indexing). `width`/`height` double as the "enriched" marker:
/// they are `NULL` until enrichment runs (see [`Db::paths_needing_enrichment`]),
/// so a failed decode still writes `0`×`0` to avoid re-attempting forever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Enrichment {
    pub width: i64,
    pub height: i64,
    pub phash: Option<i64>,
    pub format: Option<String>,
    pub date_taken: Option<i64>,
    pub camera: Option<String>,
    pub orientation: Option<i64>,
}

impl Db {
    /// Insert or replace the row for `record.path` (path is unique). Returns the
    /// row id. `missing` is cleared and `indexed_at` taken from the record.
    pub fn upsert_file(&self, record: &FileRecord) -> rusqlite::Result<i64> {
        self.conn().execute(
            "INSERT INTO files
               (path, content_hash, phash, size, mtime, width, height, format,
                date_taken, camera, orientation, indexed_at, missing)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(path) DO UPDATE SET
               content_hash=excluded.content_hash, phash=excluded.phash,
               size=excluded.size, mtime=excluded.mtime, width=excluded.width,
               height=excluded.height, format=excluded.format,
               date_taken=excluded.date_taken, camera=excluded.camera,
               orientation=excluded.orientation, indexed_at=excluded.indexed_at,
               missing=excluded.missing",
            rusqlite::params![
                record.path,
                record.content_hash,
                record.phash,
                record.size,
                record.mtime,
                record.width,
                record.height,
                record.format,
                record.date_taken,
                record.camera,
                record.orientation,
                record.indexed_at,
                record.missing as i64,
            ],
        )?;
        // ON CONFLICT means last_insert_rowid may be stale; fetch by path.
        self.conn().query_row(
            "SELECT id FROM files WHERE path = ?1",
            [&record.path],
            |r| r.get(0),
        )
    }

    /// Fetch a file row by exact path.
    pub fn file_by_path(&self, path: &str) -> rusqlite::Result<Option<FileRecord>> {
        self.conn()
            .query_row(
                &format!("SELECT {SELECT_COLS} FROM files WHERE path = ?1"),
                [path],
                FileRecord::from_row,
            )
            .optional()
    }

    /// All file rows with a given content hash (an exact-duplicate set).
    pub fn files_by_hash(&self, content_hash: &str) -> rusqlite::Result<Vec<FileRecord>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SELECT_COLS} FROM files WHERE content_hash = ?1 ORDER BY path"
        ))?;
        let rows = stmt.query_map([content_hash], FileRecord::from_row)?;
        rows.collect()
    }

    /// Move an existing row to a new path (a rename/move of the same content),
    /// clearing `missing`. Returns true if a row was updated.
    pub fn relink_path(
        &self,
        old_path: &str,
        new_path: &str,
        mtime: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "UPDATE files SET path = ?2, mtime = ?3, missing = 0 WHERE path = ?1",
            rusqlite::params![old_path, new_path, mtime],
        )?;
        Ok(n > 0)
    }

    /// Flag paths as missing (source vanished); kept for hash reconcile/tags.
    pub fn mark_missing(&self, path: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("UPDATE files SET missing = 1 WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Total number of indexed (non-missing) files.
    pub fn file_count(&self) -> rusqlite::Result<i64> {
        self.conn()
            .query_row("SELECT count(*) FROM files WHERE missing = 0", [], |r| {
                r.get(0)
            })
    }

    /// Write decode-derived fields for `path` (leaving identity/fs columns
    /// alone). No-op if the path is gone. Called by the app's enrichment pass.
    pub fn set_enrichment(&self, path: &str, e: &Enrichment) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE files SET width=?2, height=?3, phash=?4, format=?5,
                 date_taken=?6, camera=?7, orientation=?8 WHERE path=?1",
            rusqlite::params![
                path,
                e.width,
                e.height,
                e.phash,
                e.format,
                e.date_taken,
                e.camera,
                e.orientation,
            ],
        )?;
        Ok(())
    }

    /// Up to `limit` present files still awaiting enrichment (`width IS NULL`),
    /// oldest-indexed first. `width` is set (even to 0 on decode failure) once a
    /// file is processed, so this list drains monotonically as enrichment runs.
    pub fn paths_needing_enrichment(&self, limit: i64) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT path FROM files WHERE width IS NULL AND missing = 0
             ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, hash: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            content_hash: hash.into(),
            size: 100,
            mtime: 42,
            indexed_at: 1,
            ..Default::default()
        }
    }

    #[test]
    fn upsert_and_fetch() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_file(&rec("/a.jpg", "hashA")).unwrap();
        let got = db.file_by_path("/a.jpg").unwrap().unwrap();
        assert_eq!(got.id, Some(id));
        assert_eq!(got.content_hash, "hashA");
        assert!(db.file_by_path("/missing.jpg").unwrap().is_none());
    }

    #[test]
    fn upsert_updates_in_place_by_path() {
        let db = Db::open_in_memory().unwrap();
        let id1 = db.upsert_file(&rec("/a.jpg", "hashA")).unwrap();
        let mut updated = rec("/a.jpg", "hashB");
        updated.size = 999;
        let id2 = db.upsert_file(&updated).unwrap();
        assert_eq!(id1, id2, "same path keeps the same row id");
        let got = db.file_by_path("/a.jpg").unwrap().unwrap();
        assert_eq!(got.content_hash, "hashB");
        assert_eq!(got.size, 999);
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn files_by_hash_groups_duplicates() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/a.jpg", "dup")).unwrap();
        db.upsert_file(&rec("/b.jpg", "dup")).unwrap();
        db.upsert_file(&rec("/c.jpg", "other")).unwrap();
        let dups = db.files_by_hash("dup").unwrap();
        assert_eq!(dups.len(), 2);
        assert_eq!(dups[0].path, "/a.jpg");
        assert_eq!(dups[1].path, "/b.jpg");
    }

    #[test]
    fn enrichment_writes_and_drains_the_queue() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/a.jpg", "hA")).unwrap();
        db.upsert_file(&rec("/b.jpg", "hB")).unwrap();

        // Both start un-enriched (width NULL).
        assert_eq!(
            db.paths_needing_enrichment(10).unwrap(),
            vec!["/a.jpg".to_string(), "/b.jpg".to_string()]
        );

        db.set_enrichment(
            "/a.jpg",
            &Enrichment {
                width: 4000,
                height: 3000,
                phash: Some(-42),
                format: Some("JPEG".into()),
                date_taken: Some(1_000_000_000),
                camera: Some("SONY ILCE-7M3".into()),
                orientation: Some(1),
            },
        )
        .unwrap();

        // /a.jpg is now enriched → only /b.jpg remains in the queue.
        assert_eq!(db.paths_needing_enrichment(10).unwrap(), vec!["/b.jpg"]);
        let a = db.file_by_path("/a.jpg").unwrap().unwrap();
        assert_eq!(a.width, Some(4000));
        assert_eq!(a.phash, Some(-42));
        assert_eq!(a.format.as_deref(), Some("JPEG"));
        assert_eq!(a.camera.as_deref(), Some("SONY ILCE-7M3"));
        // Identity columns are untouched by enrichment.
        assert_eq!(a.content_hash, "hA");
    }

    #[test]
    fn failed_decode_marks_enriched_so_it_stops_retrying() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/broken.jpg", "hX")).unwrap();
        // Sentinel 0×0 write (a decode that failed) still clears width NULL.
        db.set_enrichment("/broken.jpg", &Enrichment::default())
            .unwrap();
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());
    }

    #[test]
    fn reindex_of_modified_file_resets_enrichment() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/a.jpg", "hA")).unwrap();
        db.set_enrichment(
            "/a.jpg",
            &Enrichment {
                width: 100,
                height: 100,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());

        // A modified file is re-upserted with a fresh (enrichment-less) record;
        // width goes back to NULL, so it re-enters the enrichment queue.
        db.upsert_file(&rec("/a.jpg", "hA-v2")).unwrap();
        assert_eq!(db.paths_needing_enrichment(10).unwrap(), vec!["/a.jpg"]);
    }

    #[test]
    fn relink_and_missing() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/old.jpg", "h")).unwrap();
        db.mark_missing("/old.jpg").unwrap();
        assert_eq!(db.file_count().unwrap(), 0); // missing excluded

        assert!(db.relink_path("/old.jpg", "/new.jpg", 55).unwrap());
        assert!(db.file_by_path("/old.jpg").unwrap().is_none());
        let moved = db.file_by_path("/new.jpg").unwrap().unwrap();
        assert!(!moved.missing);
        assert_eq!(moved.mtime, 55);
        assert_eq!(db.file_count().unwrap(), 1);
    }
    #[test]
    fn portal_path_detection() {
        assert!(is_portal_document_path("/run/user/1000/doc/abc/a.jpg"));
        assert!(!is_portal_document_path("/home/u/Pictures/a.jpg"));
        // Near misses that must not be treated as portal paths.
        assert!(!is_portal_document_path("/run/user/1000/other/a.jpg"));
        assert!(!is_portal_document_path("/run/user/notauid/doc/a.jpg"));
        assert!(!is_portal_document_path("/run/user/"));
    }

    #[test]
    fn durable_paths_win_over_portal_handles() {
        // The portal row points at a handle that stops resolving when the grant
        // lapses; the real row points at the file. Same bytes, so keep the real.
        let files = vec![
            FileRecord {
                path: "/run/user/1000/doc/xyz/a.jpg".into(),
                content_hash: "h1".into(),
                ..Default::default()
            },
            FileRecord {
                path: "/home/u/Pictures/a.jpg".into(),
                content_hash: "h1".into(),
                ..Default::default()
            },
            // Only reachable through the portal — nothing else stands for it.
            FileRecord {
                path: "/run/user/1000/doc/xyz/b.jpg".into(),
                content_hash: "h2".into(),
                ..Default::default()
            },
        ];
        let mut kept: Vec<String> = prefer_durable_paths(files)
            .into_iter()
            .map(|f| f.path)
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            ["/home/u/Pictures/a.jpg", "/run/user/1000/doc/xyz/b.jpg"]
        );
    }
}
