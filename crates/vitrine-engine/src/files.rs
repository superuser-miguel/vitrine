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

/// Drop rows whose file is not reachable right now.
///
/// A path can be perfectly valid and still resolve to nothing: an external drive
/// is unplugged, or a document-portal handle points at a volume that is not
/// mounted. Those files are **offline**, not deleted — the rows stay in the
/// index and keep their annotations, they simply have nothing to show.
///
/// Checks each distinct parent directory once rather than each file, so a view
/// of a few hundred rows costs a handful of `stat` calls. Intended for the small
/// result sets a view actually renders (a collection, a tag browse), not for
/// sweeping the whole table.
///
/// This must never be used to set `missing`: that flag means "verified gone" and
/// drives reconciliation. Unplugging a drive is not a deletion.
pub fn drop_unreachable(files: Vec<FileRecord>) -> Vec<FileRecord> {
    let mut dir_ok: std::collections::HashMap<std::path::PathBuf, bool> =
        std::collections::HashMap::new();
    files
        .into_iter()
        .filter(|f| {
            let path = std::path::Path::new(&f.path);
            let Some(dir) = path.parent() else {
                return true;
            };
            let reachable = *dir_ok
                .entry(dir.to_path_buf())
                .or_insert_with(|| dir.is_dir());
            reachable && path.exists()
        })
        .collect()
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

/// The next batch of the enrichment backlog. Its WHERE is exactly the partial
/// index `idx_files_unenriched`'s (schema v6); change one, change both.
const ENRICHMENT_QUEUE_SQL: &str = "SELECT path FROM files
     WHERE width IS NULL AND missing = 0 AND unreachable_at IS NULL
     ORDER BY id LIMIT ?1";

/// The decode-derived fields written by the app's enrichment pass, once per file
/// (after identity indexing). `width`/`height` double as the "enriched" marker:
/// they are `NULL` until enrichment runs (see [`Db::paths_needing_enrichment`]),
/// so a failed decode still writes `0`×`0` to avoid re-attempting forever.
///
/// A file that could not even be *opened* is not a failed decode: see
/// [`EnrichOutcome::Unreachable`].
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

/// What one enrichment attempt found, as the writer records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichOutcome {
    /// Decoded: dimensions and whatever metadata came with them.
    Enriched(Enrichment),
    /// The file opened but would not decode. Written as the `0`×`0` sentinel,
    /// so it is never attempted again (until its bytes change).
    Undecodable,
    /// The file could not be stat'ed or opened: an unplugged drive, an expired
    /// portal grant, a dropped network share. Offline, not broken — the row
    /// keeps `width IS NULL` and leaves the queue until a walk sees the file
    /// again (see [`Db::requeue_unreachable_seen`]).
    Unreachable,
}

/// Whether `path` can be opened for reading right now.
///
/// The enrichment pass asks this when a decode fails, to tell "this file is
/// broken" (opens, won't decode → 0×0, never retried) from "this file is not
/// here right now" (unreachable → retried when it comes back). Any open error
/// counts as unreachable — ENOENT, EACCES, EIO, ENXIO, ESTALE, ENOTCONN alike:
/// a wrong guess that way costs one cheap failed `open` per walk, while a wrong
/// guess the other way buries a good file under a permanent 0×0.
pub fn is_reachable(path: &std::path::Path) -> bool {
    std::fs::File::open(path).is_ok()
}

impl EnrichOutcome {
    /// The outcome for a file whose decode just failed: undecodable if it can
    /// still be opened, unreachable if not. Checked *after* the failure, so a
    /// drive pulled mid-decode lands on the right side too.
    pub fn after_failed_decode(path: &std::path::Path) -> EnrichOutcome {
        if is_reachable(path) {
            EnrichOutcome::Undecodable
        } else {
            EnrichOutcome::Unreachable
        }
    }
}

/// `meta` key marking the one-time requeue of 0×0 rows as done (0.4).
pub const SENTINEL_REQUEUE_KEY: &str = "maintenance.enrichment-sentinels-requeued.v1";

/// What [`Db::requeue_enrichment_sentinels_once`] did with the 0×0 rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SentinelRequeue {
    /// Reachable now: back in the enrichment queue.
    pub requeued: usize,
    /// Not reachable now: parked as unreachable until a walk sees them.
    pub parked: usize,
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
               missing=excluded.missing, unreachable_at=NULL",
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
            "UPDATE files SET path = ?2, mtime = ?3, missing = 0, unreachable_at = NULL
             WHERE path = ?1",
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

    /// A missing file is back at its old path with the same size and mtime
    /// (a replugged drive): clear `missing`, keeping its hash and enrichment —
    /// the same trust `(size, mtime)` earns an unchanged file. It is visibly
    /// reachable, so any unreachable mark goes too. Returns true if a row
    /// changed.
    pub fn clear_missing(&self, path: &str) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "UPDATE files SET missing = 0, unreachable_at = NULL WHERE path = ?1",
            [path],
        )?;
        Ok(n > 0)
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
                 date_taken=?6, camera=?7, orientation=?8, unreachable_at=NULL
             WHERE path=?1",
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

    /// Record one enrichment attempt for `path`: the decoded fields, the 0×0
    /// "undecodable" sentinel, or the unreachable mark. Either way the row
    /// leaves the queue, so the backlog still drains monotonically.
    pub fn record_enrichment(&self, path: &str, outcome: &EnrichOutcome) -> rusqlite::Result<()> {
        match outcome {
            EnrichOutcome::Enriched(e) => self.set_enrichment(path, e),
            EnrichOutcome::Undecodable => self.set_enrichment(path, &Enrichment::default()),
            EnrichOutcome::Unreachable => self.mark_unreachable(path, crate::db::now_secs()),
        }
    }

    /// Note that `path` could not be opened at `when`. Leaves `width` (and every
    /// other enrichment column) untouched: an earlier good enrichment stays, and
    /// an un-enriched row stays `NULL` — never a size the UI could show.
    pub fn mark_unreachable(&self, path: &str, when: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE files SET unreachable_at = ?2 WHERE path = ?1",
            rusqlite::params![path, when],
        )?;
        Ok(())
    }

    /// Put back in the enrichment queue every unreachable row under `root`
    /// that this walk saw: being listed by the walk is proof it is reachable
    /// again. Returns how many rows were requeued.
    pub fn requeue_unreachable_seen(
        &self,
        root: &str,
        seen: &std::collections::HashSet<String>,
    ) -> rusqlite::Result<usize> {
        let (lo, hi) = crate::query::subtree_range(root);
        let parked: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT path FROM files
                 WHERE unreachable_at IS NOT NULL AND path >= ?1 AND path < ?2",
            )?;
            let rows = stmt.query_map([lo, hi], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut requeued = 0;
        for path in parked.iter().filter(|p| seen.contains(*p)) {
            requeued += self.conn().execute(
                "UPDATE files SET unreachable_at = NULL WHERE path = ?1",
                [path],
            )?;
        }
        Ok(requeued)
    }

    /// One-time (0.4): give every 0×0 row a fresh start under the rules that
    /// tell unreachable from undecodable. Before 0.4 an unreachable file got
    /// the "decode failed, never retry" sentinel too, and nothing ever looked
    /// at it again.
    ///
    /// Each row is triaged with one [`is_reachable`] check, the same test the
    /// enrichment pass applies: a file that opens goes back in the enrichment
    /// queue (it enriches, or earns 0×0 again if it really is broken); one that
    /// doesn't is parked as unreachable straight away, to be retried when a
    /// walk sees it. Doing that here, in one transaction, costs about a second
    /// for 64k rows; leaving it to the enrichment pass meant one committed
    /// write per dead path, ~3 minutes of writer time.
    ///
    /// Missing rows are triaged too, so a reappearing file gets its retry then.
    /// Nothing is deleted or marked missing, and no annotation is touched: the
    /// only values cleared are 0×0 sentinels, which carry no information. The
    /// changes and the done-marker commit together.
    ///
    /// Returns `None` if it already ran.
    pub fn requeue_enrichment_sentinels_once(&self) -> rusqlite::Result<Option<SentinelRequeue>> {
        if self.meta(SENTINEL_REQUEUE_KEY)?.is_some() {
            return Ok(None);
        }
        let now = crate::db::now_secs();
        let tx = self.conn().unchecked_transaction()?;
        let stuck: Vec<(i64, String)> = {
            let mut stmt =
                tx.prepare("SELECT id, path FROM files WHERE width = 0 AND height = 0")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut done = SentinelRequeue::default();
        {
            let mut reset = tx.prepare(
                "UPDATE files SET width = NULL, height = NULL, unreachable_at = ?2
                 WHERE id = ?1",
            )?;
            for (id, path) in &stuck {
                if is_reachable(std::path::Path::new(path)) {
                    reset.execute(rusqlite::params![id, None::<i64>])?;
                    done.requeued += 1;
                } else {
                    reset.execute(rusqlite::params![id, now])?;
                    done.parked += 1;
                }
            }
        }
        self.set_meta(SENTINEL_REQUEUE_KEY, &now.to_string())?;
        tx.commit()?;
        Ok(Some(done))
    }

    /// Undo what a pre-0.4 build does to parked rows when it shares this
    /// database (the dev build and the installed app use the same index): it
    /// knows nothing of `unreachable_at`, so its enrichment sees a parked row's
    /// `width IS NULL`, fails to open the file and writes the old 0×0 sentinel,
    /// leaving the mark set. Under 0.4 a 0×0 write clears the mark, so a marked
    /// row at 0×0 can only come from such a build: park it again. (A row it
    /// enriched for real keeps a stale mark, which is the same harmless state
    /// as an enriched file that went offline.)
    ///
    /// Runs on every launch; it reads only the small `idx_files_unreachable`
    /// set. Returns how many rows it re-parked.
    pub fn repair_parked_rows(&self) -> rusqlite::Result<usize> {
        self.conn().execute(
            "UPDATE files SET width = NULL, height = NULL
             WHERE unreachable_at IS NOT NULL AND width = 0 AND height = 0",
            [],
        )
    }

    /// Up to `limit` present files still awaiting enrichment (`width IS NULL`,
    /// not parked as unreachable), oldest-indexed first. Every attempt either
    /// sets `width` (even to 0 on decode failure) or parks the row as
    /// unreachable, so this list drains monotonically as enrichment runs.
    ///
    /// The WHERE clause is exactly `idx_files_unenriched`'s (schema v6), which
    /// is what lets this read the backlog instead of scanning `files`; keep the
    /// two in step.
    pub fn paths_needing_enrichment(&self, limit: i64) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn().prepare(ENRICHMENT_QUEUE_SQL)?;
        let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vitrine-files-{tag}-{}-{}",
            std::process::id(),
            crate::db::now_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn width_height(db: &Db, path: &str) -> (Option<i64>, Option<i64>) {
        let r = db.file_by_path(path).unwrap().unwrap();
        (r.width, r.height)
    }

    /// #1: a file that can't be opened at enrichment time (unplugged drive,
    /// lapsed portal grant) is parked, not given the never-retry 0×0 — and a
    /// later walk that sees it puts it back in the queue.
    #[test]
    fn unreachable_file_is_parked_then_retried_when_seen() {
        let dir = temp_dir("unreachable");
        let file = dir.join("offline.jpg");
        let path = file.to_string_lossy().into_owned();
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec(&path, "hU")).unwrap();

        // The drive is gone: the decode fails and the file won't open.
        let outcome = EnrichOutcome::after_failed_decode(&file);
        assert_eq!(outcome, EnrichOutcome::Unreachable);
        db.record_enrichment(&path, &outcome).unwrap();

        assert_eq!(width_height(&db, &path), (None, None), "no 0×0 sentinel");
        assert!(
            db.paths_needing_enrichment(10).unwrap().is_empty(),
            "parked rows leave the queue, or the driver would spin on them"
        );

        // A walk that doesn't list it changes nothing.
        let root = dir.to_string_lossy();
        assert_eq!(
            db.requeue_unreachable_seen(&root, &HashSet::new()).unwrap(),
            0
        );
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());

        // The drive is back: the walk lists the file, which requeues it…
        std::fs::write(&file, b"bytes").unwrap();
        let seen: HashSet<String> = [path.clone()].into_iter().collect();
        assert_eq!(db.requeue_unreachable_seen(&root, &seen).unwrap(), 1);
        assert_eq!(db.paths_needing_enrichment(10).unwrap(), vec![path.clone()]);

        // …and this time it enriches.
        db.record_enrichment(
            &path,
            &EnrichOutcome::Enriched(Enrichment {
                width: 640,
                height: 480,
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(width_height(&db, &path), (Some(640), Some(480)));
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unreachable mark never erases an earlier good enrichment.
    #[test]
    fn unreachable_keeps_an_earlier_enrichment() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/a.jpg", "hA")).unwrap();
        db.set_enrichment(
            "/a.jpg",
            &Enrichment {
                width: 10,
                height: 20,
                ..Default::default()
            },
        )
        .unwrap();
        db.record_enrichment("/a.jpg", &EnrichOutcome::Unreachable)
            .unwrap();
        assert_eq!(width_height(&db, "/a.jpg"), (Some(10), Some(20)));
    }

    /// A file that opens but won't decode keeps the never-retry 0×0, and a
    /// walk seeing it again does not requeue it.
    #[test]
    fn undecodable_file_still_gets_the_zero_sentinel() {
        let dir = temp_dir("undecodable");
        let file = dir.join("broken.jpg");
        std::fs::write(&file, b"not an image").unwrap();
        let path = file.to_string_lossy().into_owned();
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec(&path, "hB")).unwrap();

        let outcome = EnrichOutcome::after_failed_decode(&file);
        assert_eq!(outcome, EnrichOutcome::Undecodable);
        db.record_enrichment(&path, &outcome).unwrap();

        assert_eq!(width_height(&db, &path), (Some(0), Some(0)));
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());
        let seen: HashSet<String> = [path.clone()].into_iter().collect();
        assert_eq!(
            db.requeue_unreachable_seen(&dir.to_string_lossy(), &seen)
                .unwrap(),
            0
        );
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #4: the one-time step triages every 0×0 row (present or missing):
    /// reachable ones go back in the queue, unreachable ones are parked (and
    /// come back when a walk sees them). It leaves real enrichments and
    /// annotations alone, deletes nothing, marks nothing missing, and runs
    /// exactly once.
    #[test]
    fn sentinel_requeue_runs_once_and_triages_zero_rows() {
        let dir = temp_dir("requeue");
        let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let (stuck, dead, gone, good, new) = (
            p("stuck.jpg"),
            p("dead.jpg"),
            p("gone.jpg"),
            p("good.jpg"),
            p("new.jpg"),
        );
        // Only `stuck` is on disk: an old 0×0 that is reachable after all.
        std::fs::write(&stuck, b"bytes").unwrap();

        let db = Db::open_in_memory().unwrap();
        for (path, h) in [
            (&stuck, "h1"),
            (&dead, "h2"),
            (&gone, "h3"),
            (&good, "h4"),
            (&new, "h5"),
        ] {
            db.upsert_file(&rec(path, h)).unwrap();
        }
        for path in [&stuck, &dead, &gone] {
            db.set_enrichment(path, &Enrichment::default()).unwrap();
        }
        db.mark_missing(&gone).unwrap();
        db.set_enrichment(
            &good,
            &Enrichment {
                width: 3,
                height: 4,
                ..Default::default()
            },
        )
        .unwrap();
        db.set_rating("h2", 4).unwrap();
        assert_eq!(db.paths_needing_enrichment(10).unwrap(), vec![new.clone()]);

        assert_eq!(
            db.requeue_enrichment_sentinels_once().unwrap(),
            Some(SentinelRequeue {
                requeued: 1,
                parked: 2
            })
        );
        assert_eq!(
            db.paths_needing_enrichment(10).unwrap(),
            vec![stuck.clone(), new.clone()],
            "the reachable 0×0 row is back in the queue, oldest first"
        );
        for path in [&stuck, &dead, &gone] {
            assert_eq!(width_height(&db, path), (None, None), "{path}");
        }
        assert!(db.file_by_path(&gone).unwrap().unwrap().missing);
        assert!(!db.file_by_path(&dead).unwrap().unwrap().missing);
        assert_eq!(width_height(&db, &good), (Some(3), Some(4)));
        assert_eq!(db.rating("h2").unwrap(), Some(4));
        let rows: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 5);

        // The parked row returns the moment a walk lists it.
        let seen: HashSet<String> = [dead.clone()].into_iter().collect();
        assert_eq!(
            db.requeue_unreachable_seen(&dir.to_string_lossy(), &seen)
                .unwrap(),
            1
        );
        assert!(db.paths_needing_enrichment(10).unwrap().contains(&dead));

        // Re-enriched as undecodable, a row must stay that way: the step does
        // not run twice.
        db.set_enrichment(&stuck, &Enrichment::default()).unwrap();
        assert_eq!(db.requeue_enrichment_sentinels_once().unwrap(), None);
        assert_eq!(width_height(&db, &stuck), (Some(0), Some(0)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The per-scan requeue reads the small partial index of parked rows, not
    /// the whole subtree of `files`.
    #[test]
    fn a_parked_row_sentinelled_by_an_older_build_is_parked_again() {
        let db = Db::open_in_memory().unwrap();
        for (path, h) in [("/gone/a.jpg", "hA"), ("/ok/b.jpg", "hB")] {
            db.upsert_file(&rec(path, h)).unwrap();
        }
        db.mark_unreachable("/gone/a.jpg", 100).unwrap();
        // What 0.3.x enrichment writes: 0×0, mark untouched.
        db.conn()
            .execute(
                "UPDATE files SET width = 0, height = 0 WHERE path = '/gone/a.jpg'",
                [],
            )
            .unwrap();
        // A genuinely undecodable file under 0.4: 0×0 with no mark.
        db.set_enrichment("/ok/b.jpg", &Enrichment::default())
            .unwrap();

        assert_eq!(db.repair_parked_rows().unwrap(), 1);
        assert_eq!(width_height(&db, "/gone/a.jpg"), (None, None));
        assert_eq!(width_height(&db, "/ok/b.jpg"), (Some(0), Some(0)));
        assert!(
            db.paths_needing_enrichment(10).unwrap().is_empty(),
            "parked again, not back in the queue"
        );
        assert_eq!(db.repair_parked_rows().unwrap(), 0);
    }

    #[test]
    fn the_parked_rows_query_reads_its_partial_index() {
        let db = Db::open_in_memory().unwrap();
        let plan: Vec<String> = db
            .conn()
            .prepare(
                "EXPLAIN QUERY PLAN SELECT path FROM files
                 WHERE unreachable_at IS NOT NULL AND path >= ?1 AND path < ?2",
            )
            .unwrap()
            .query_map(["/a/", "/a0"], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            plan,
            ["SEARCH files USING INDEX idx_files_unreachable (path>? AND path<?)"]
        );
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
    fn the_enrichment_queue_reads_its_partial_index() {
        // Asked once per batch for the whole enrichment pass; a plain SCAN of
        // `files` was ~75ms on a 432k-row index. The partial index only applies
        // while the query's WHERE matches its own, so this fails if either drifts.
        let db = Db::open_in_memory().unwrap();
        let plan: Vec<String> = db
            .conn()
            .prepare(&format!("EXPLAIN QUERY PLAN {ENRICHMENT_QUEUE_SQL}"))
            .unwrap()
            .query_map([64], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(plan, ["SCAN files USING INDEX idx_files_unenriched"]);
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

    #[test]
    fn unreachable_rows_are_dropped_not_marked_missing() {
        // An unplugged drive leaves the row valid and the file absent. The row
        // must not render, and — critically — nothing here touches `missing`,
        // which means "verified gone" and drives reconciliation.
        let dir = std::env::temp_dir().join(format!("vitrine-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("here.jpg");
        std::fs::write(&present, b"x").unwrap();

        let rows = vec![
            FileRecord {
                path: present.to_string_lossy().into_owned(),
                content_hash: "h1".into(),
                ..Default::default()
            },
            FileRecord {
                path: "/run/media/u/UNPLUGGED/gone.jpg".into(),
                content_hash: "h2".into(),
                ..Default::default()
            },
            FileRecord {
                path: dir.join("deleted.jpg").to_string_lossy().into_owned(),
                content_hash: "h3".into(),
                ..Default::default()
            },
        ];
        let kept = drop_unreachable(rows);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].content_hash, "h1");
        assert!(
            !kept[0].missing,
            "reachability must not touch the missing flag"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
