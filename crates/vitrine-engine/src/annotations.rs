//! Per-image annotations keyed by `content_hash`: **ratings** (0–5 stars) and
//! **comments** (free-text caption) — PLAN Phase 3 tasks 2 / 2a.
//!
//! Both survive renames (content-hash keyed) and both carry a `sync_state` seam
//! for the deferred v2 embedded-metadata write: ratings map to `Xmp.xmp.Rating`,
//! comments to `Xmp.dc.description`, so that write-back is a pure sync step.

use rusqlite::OptionalExtension;

use crate::db::{now_secs, Db};

impl Db {
    /// Set the 0–5 star rating for a content hash (upsert).
    pub fn set_rating(&self, content_hash: &str, rating: i64) -> rusqlite::Result<()> {
        let rating = rating.clamp(0, 5);
        self.conn().execute(
            "INSERT INTO ratings(content_hash, rating, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(content_hash) DO UPDATE SET
               rating = excluded.rating, updated_at = excluded.updated_at",
            rusqlite::params![content_hash, rating, now_secs()],
        )?;
        Ok(())
    }

    /// The star rating for a content hash, if any.
    pub fn rating(&self, content_hash: &str) -> rusqlite::Result<Option<i64>> {
        self.conn()
            .query_row(
                "SELECT rating FROM ratings WHERE content_hash = ?1",
                [content_hash],
                |r| r.get(0),
            )
            .optional()
    }

    /// Remove the rating for a content hash (→ unrated).
    pub fn clear_rating(&self, content_hash: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "DELETE FROM ratings WHERE content_hash = ?1",
            [content_hash],
        )?;
        Ok(())
    }

    /// Set the comment for a content hash. An empty/whitespace body clears it
    /// (so there's no distinction between "" and "no comment").
    pub fn set_comment(&self, content_hash: &str, body: &str) -> rusqlite::Result<()> {
        let body = body.trim();
        if body.is_empty() {
            return self.clear_comment(content_hash);
        }
        self.conn().execute(
            "INSERT INTO comments(content_hash, body, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(content_hash) DO UPDATE SET
               body = excluded.body, updated_at = excluded.updated_at",
            rusqlite::params![content_hash, body, now_secs()],
        )?;
        Ok(())
    }

    /// The comment for a content hash, if any.
    pub fn comment(&self, content_hash: &str) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT body FROM comments WHERE content_hash = ?1",
                [content_hash],
                |r| r.get(0),
            )
            .optional()
    }

    /// Remove the comment for a content hash.
    pub fn clear_comment(&self, content_hash: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "DELETE FROM comments WHERE content_hash = ?1",
            [content_hash],
        )?;
        Ok(())
    }

    /// Set the user's non-destructive orientation (EXIF 1–8); 1 clears the row.
    pub fn set_orientation(&self, content_hash: &str, orientation: i64) -> rusqlite::Result<()> {
        if orientation <= 1 {
            self.conn().execute(
                "DELETE FROM orientations WHERE content_hash = ?1",
                [content_hash],
            )?;
            return Ok(());
        }
        self.conn().execute(
            "INSERT INTO orientations(content_hash, orientation, updated_at)
             VALUES (?1, ?2, unixepoch())
             ON CONFLICT(content_hash) DO UPDATE
             SET orientation = excluded.orientation, updated_at = excluded.updated_at",
            rusqlite::params![content_hash, orientation.clamp(1, 8)],
        )?;
        Ok(())
    }

    /// Set the non-destructive crop rect (normalized display-space [0,1]).
    pub fn set_crop(&self, content_hash: &str, r: (f64, f64, f64, f64)) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO crops(content_hash, x, y, w, h, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(content_hash) DO UPDATE SET
               x = excluded.x, y = excluded.y, w = excluded.w, h = excluded.h,
               updated_at = excluded.updated_at",
            rusqlite::params![content_hash, r.0, r.1, r.2, r.3, now_secs()],
        )?;
        Ok(())
    }

    /// Remove the crop instruction (→ full frame).
    pub fn clear_crop(&self, content_hash: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM crops WHERE content_hash = ?1", [content_hash])?;
        Ok(())
    }

    /// Carry every annotation from `old` to `new` content hash — the Save
    /// (bake-in-place) path: the rewritten file at `saved_path` has a new
    /// identity, and the user's ratings/tags/comments/collections must follow it.
    ///
    /// A hash is shared by every copy of the same bytes, so the annotations
    /// belong to all of them. If any *other* present row still holds `old` (a
    /// duplicate elsewhere, or the same file under a portal handle), they are
    /// **copied** so that copy keeps them; only when the saved file was the last
    /// holder are they moved. Orientation and crop instructions are never carried
    /// (they were just baked into the pixels), and are dropped from `old` only on
    /// a move — on a copy they still describe the duplicate's unbaked pixels.
    ///
    /// All of it is one transaction: a failure midway leaves nothing rekeyed.
    pub fn rekey_annotations_from(
        &self,
        saved_path: &str,
        old: &str,
        new: &str,
    ) -> rusqlite::Result<()> {
        self.rekey(Some(saved_path), old, new)
    }

    /// [`Db::rekey_annotations_from`] without the saved path: every present row
    /// holding `old` counts as another holder, so this copies whenever the index
    /// still knows the hash. Never loses annotations; at worst it leaves them on
    /// a hash that the next rescan orphans. Prefer the path-aware form.
    pub fn rekey_annotations(&self, old: &str, new: &str) -> rusqlite::Result<()> {
        self.rekey(None, old, new)
    }

    fn rekey(&self, saved_path: Option<&str>, old: &str, new: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        // IMMEDIATE: this reads before it writes, and a deferred transaction
        // that has to upgrade its lock fails outright instead of waiting.
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            // `path IS NOT NULL` is always true, so with no saved path every
            // present holder counts.
            let shared: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM files
                               WHERE content_hash = ?1 AND missing = 0 AND path IS NOT ?2)",
                rusqlite::params![old, saved_path],
                |r| r.get(0),
            )?;
            let statements: &[&str] = if shared {
                &[
                    "INSERT OR IGNORE INTO ratings(content_hash, rating, sync_state, updated_at)
                     SELECT ?2, rating, sync_state, updated_at FROM ratings WHERE content_hash = ?1",
                    "INSERT OR IGNORE INTO comments(content_hash, body, sync_state, updated_at)
                     SELECT ?2, body, sync_state, updated_at FROM comments WHERE content_hash = ?1",
                    "INSERT OR IGNORE INTO file_tags(content_hash, tag_id, sync_state, source, created_at)
                     SELECT ?2, tag_id, sync_state, source, created_at FROM file_tags
                     WHERE content_hash = ?1",
                    "INSERT OR IGNORE INTO collection_items(collection_id, content_hash, position)
                     SELECT collection_id, ?2, position FROM collection_items
                     WHERE content_hash = ?1",
                ]
            } else {
                &[
                    "UPDATE OR REPLACE ratings SET content_hash = ?2 WHERE content_hash = ?1",
                    "UPDATE OR REPLACE comments SET content_hash = ?2 WHERE content_hash = ?1",
                    "UPDATE OR REPLACE file_tags SET content_hash = ?2 WHERE content_hash = ?1",
                    "UPDATE OR REPLACE collection_items SET content_hash = ?2 WHERE content_hash = ?1",
                ]
            };
            for sql in statements {
                conn.execute(sql, rusqlite::params![old, new])?;
            }
            if !shared {
                // One bound parameter here: handing these the pair is an
                // InvalidParameterCount error, which is how they once never ran.
                conn.execute("DELETE FROM orientations WHERE content_hash = ?1", [old])?;
                conn.execute("DELETE FROM crops WHERE content_hash = ?1", [old])?;
            }
            Ok(())
        })();
        if result.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
        result
    }

    /// `(path, content_hash, rating, orientation, crop)` for present files under
    /// `folder` — one query to stamp the grid's in-memory items, so cell rating
    /// overlays and rating writes need no per-cell database hit. `rating` is 0
    /// when unrated; `orientation` is 1 (identity) when never rotated.
    #[allow(clippy::type_complexity)]
    pub fn ratings_under(
        &self,
        folder: &str,
    ) -> rusqlite::Result<
        Vec<(
            String,
            String,
            i64,
            i64,
            Option<(f64, f64, f64, f64)>,
            Option<i64>,
        )>,
    > {
        // Runs on the main thread at every folder open — the path range (vs a
        // LIKE prefix) is what lets it use the path index instead of scanning
        // the whole files table (see `subtree_range`).
        let (lo, hi) = crate::query::subtree_range(folder);
        let mut stmt = self.conn().prepare(
            "SELECT f.path, f.content_hash, COALESCE(r.rating, 0), COALESCE(o.orientation, 1),
                    c.x, c.y, c.w, c.h, f.date_taken
             FROM files f
             LEFT JOIN ratings r ON r.content_hash = f.content_hash
             LEFT JOIN orientations o ON o.content_hash = f.content_hash
             LEFT JOIN crops c ON c.content_hash = f.content_hash
             WHERE f.missing = 0 AND f.path >= ?1 AND f.path < ?2",
        )?;
        let rows = stmt.query_map([lo, hi], |r| {
            let crop = match (r.get::<_, Option<f64>>(4)?, r.get(5)?, r.get(6)?, r.get(7)?) {
                (Some(x), Some(y), Some(w), Some(h)) => Some((x, y, w, h)),
                _ => None,
            };
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, crop, r.get(8)?))
        })?;
        rows.collect()
    }
}

/// A user transform op from the edit card, composed onto an EXIF 1–8 state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrientOp {
    RotateCw,
    RotateCcw,
    FlipH,
    FlipV,
}

/// Compose `op` onto EXIF orientation `state` (1–8), returning the new state.
/// Lookup tables for the dihedral group D4 — indexed by `state - 1`.
pub fn compose_orientation(state: i64, op: OrientOp) -> i64 {
    let i = (state.clamp(1, 8) - 1) as usize;
    let table: [i64; 8] = match op {
        OrientOp::RotateCw => [6, 7, 8, 5, 2, 3, 4, 1],
        OrientOp::RotateCcw => [8, 5, 6, 7, 4, 1, 2, 3],
        OrientOp::FlipH => [2, 1, 4, 3, 6, 5, 8, 7],
        OrientOp::FlipV => [4, 3, 2, 1, 8, 7, 6, 5],
    };
    table[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_upserts_and_clamps_and_clears() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.rating("h1").unwrap(), None);
        db.set_rating("h1", 4).unwrap();
        assert_eq!(db.rating("h1").unwrap(), Some(4));
        db.set_rating("h1", 2).unwrap(); // upsert, not a second row
        assert_eq!(db.rating("h1").unwrap(), Some(2));
        db.set_rating("h1", 99).unwrap(); // clamped to 5 (CHECK would else reject)
        assert_eq!(db.rating("h1").unwrap(), Some(5));
        db.clear_rating("h1").unwrap();
        assert_eq!(db.rating("h1").unwrap(), None);
    }

    #[test]
    fn ratings_under_scopes_and_joins() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO files(path,content_hash,size,mtime,indexed_at,missing) VALUES
                 ('/p/a.jpg','ha',1,1,1,0),('/p/b.jpg','hb',1,1,1,0),
                 ('/other/c.jpg','hc',1,1,1,0);",
            )
            .unwrap();
        db.set_rating("ha", 4).unwrap();
        let mut rows = db.ratings_under("/p").unwrap();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            rows,
            vec![
                ("/p/a.jpg".to_string(), "ha".to_string(), 4, 1, None, None),
                ("/p/b.jpg".to_string(), "hb".to_string(), 0, 1, None, None),
            ]
        );
    }

    #[test]
    fn ratings_under_carries_date_taken() {
        // The Date Taken sort reads this. It is NULL until background enrichment
        // decodes the file, which is exactly why the sort has to handle None.
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO files(path,content_hash,size,mtime,indexed_at,missing,date_taken)
                 VALUES ('/p/a.jpg','ha',1,1,1,0,1700000000),
                        ('/p/b.jpg','hb',1,1,1,0,NULL);",
            )
            .unwrap();
        let mut rows = db.ratings_under("/p").unwrap();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            rows[0].5,
            Some(1700000000),
            "enriched file carries its date"
        );
        assert_eq!(rows[1].5, None, "un-enriched file has no date yet");
    }

    /// Seed `paths` as present rows holding `hash`, and give the hash one of
    /// every annotation plus an orientation and a crop instruction.
    fn annotated(paths: &[&str], hash: &str) -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        for (i, path) in paths.iter().enumerate() {
            db.conn()
                .execute(
                    "INSERT INTO files(path,content_hash,size,mtime,indexed_at,missing)
                     VALUES (?1,?2,1,1,?3,0)",
                    rusqlite::params![path, hash, i as i64],
                )
                .unwrap();
        }
        db.set_rating(hash, 4).unwrap();
        db.set_comment(hash, "golden hour").unwrap();
        db.apply_tag("keeper", &[hash.to_string()]).unwrap();
        let cat = db.create_catalog("Best").unwrap();
        db.add_to_catalog(cat, &[hash.to_string()]).unwrap();
        db.set_orientation(hash, 6).unwrap();
        db.set_crop(hash, (0.1, 0.1, 0.5, 0.5)).unwrap();
        (db, cat)
    }

    /// What `hash` carries: rating, comment, tags, catalog membership, and
    /// whether it has an orientation / a crop instruction.
    type Carried = (Option<i64>, Option<String>, Vec<String>, bool, bool, bool);

    fn carried(db: &Db, hash: &str, cat: i64) -> Carried {
        let any = |sql: &str| -> bool {
            db.conn()
                .query_row(sql, rusqlite::params![hash], |r| r.get::<_, i64>(0))
                .unwrap()
                > 0
        };
        let in_catalog = db
            .conn()
            .query_row(
                "SELECT count(*) FROM collection_items
                 WHERE collection_id = ?1 AND content_hash = ?2",
                rusqlite::params![cat, hash],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        (
            db.rating(hash).unwrap(),
            db.comment(hash).unwrap(),
            db.tags_for_hash(hash).unwrap(),
            in_catalog,
            any("SELECT count(*) FROM orientations WHERE content_hash = ?1"),
            any("SELECT count(*) FROM crops WHERE content_hash = ?1"),
        )
    }

    fn all_but_instructions() -> Carried {
        (
            Some(4),
            Some("golden hour".into()),
            vec!["keeper".into()],
            true,
            false,
            false,
        )
    }

    fn nothing() -> Carried {
        (None, None, vec![], false, false, false)
    }

    #[test]
    fn rekey_moves_when_the_saved_file_is_the_only_holder() {
        let (db, cat) = annotated(&["/p/a.jpg"], "old");
        db.rekey_annotations_from("/p/a.jpg", "old", "new").unwrap();
        assert_eq!(
            carried(&db, "new", cat),
            all_but_instructions(),
            "annotations follow the new identity; baked instructions are not carried"
        );
        assert_eq!(
            carried(&db, "old", cat),
            nothing(),
            "nothing else held the old hash, so nothing is left on it"
        );
    }

    #[test]
    fn rekey_copies_when_a_duplicate_still_holds_the_old_hash() {
        // Two copies of the same bytes share one hash, so they share its
        // annotations. Saving an edit to one must not strip the other.
        let (db, cat) = annotated(&["/p/a.jpg", "/backup/a.jpg"], "old");
        db.rekey_annotations_from("/p/a.jpg", "old", "new").unwrap();
        assert_eq!(
            carried(&db, "new", cat),
            all_but_instructions(),
            "the saved file keeps its annotations under the new hash"
        );
        assert_eq!(
            carried(&db, "old", cat),
            (
                Some(4),
                Some("golden hour".into()),
                vec!["keeper".into()],
                true,
                true,
                true
            ),
            "the duplicate keeps everything, its unbaked instructions included"
        );
    }

    #[test]
    fn rekey_holder_check_ignores_missing_rows_and_the_saved_row() {
        // A missing row is not a holder, and the saved file's own row (which a
        // rescan may or may not have updated yet) is not "another" holder.
        let (db, cat) = annotated(&["/p/a.jpg", "/gone/a.jpg"], "old");
        db.mark_missing("/gone/a.jpg").unwrap();
        db.rekey_annotations_from("/p/a.jpg", "old", "new").unwrap();
        assert_eq!(carried(&db, "old", cat), nothing(), "moved, not copied");
        assert_eq!(carried(&db, "new", cat), all_but_instructions());
    }

    #[test]
    fn rekey_without_a_path_never_drops_a_holder() {
        // The path-less form can't tell the saved row from a duplicate, so it
        // keeps the old hash's annotations whenever any present row holds it.
        let (db, cat) = annotated(&["/p/a.jpg"], "old");
        db.rekey_annotations("old", "new").unwrap();
        assert_eq!(carried(&db, "old", cat).0, Some(4));
        assert_eq!(carried(&db, "new", cat), all_but_instructions());
    }

    #[test]
    fn a_failed_rekey_changes_nothing() {
        // Fail the catalog step, after ratings, comments and tags have already
        // been rewritten, in both the move and the copy shape.
        for paths in [&["/p/a.jpg"][..], &["/p/a.jpg", "/backup/a.jpg"][..]] {
            let (db, cat) = annotated(paths, "old");
            let before = carried(&db, "old", cat);
            db.conn()
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_ins BEFORE INSERT ON collection_items
                     BEGIN SELECT RAISE(ABORT, 'injected'); END;
                     CREATE TEMP TRIGGER fail_upd BEFORE UPDATE ON collection_items
                     BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                )
                .unwrap();
            assert!(db.rekey_annotations_from("/p/a.jpg", "old", "new").is_err());
            assert_eq!(carried(&db, "old", cat), before, "{paths:?}: old intact");
            assert_eq!(
                carried(&db, "new", cat),
                nothing(),
                "{paths:?}: nothing half-carried to the new hash"
            );
            assert!(
                db.conn().is_autocommit(),
                "{paths:?}: no transaction left open"
            );
        }
    }

    #[test]
    fn comment_upserts_and_empty_clears() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.comment("h1").unwrap(), None);
        db.set_comment("h1", "  golden hour  ").unwrap();
        assert_eq!(db.comment("h1").unwrap().as_deref(), Some("golden hour"));
        db.set_comment("h1", "revised").unwrap();
        assert_eq!(db.comment("h1").unwrap().as_deref(), Some("revised"));
        db.set_comment("h1", "   ").unwrap(); // whitespace → clear
        assert_eq!(db.comment("h1").unwrap(), None);
    }
}
