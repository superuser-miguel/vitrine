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

    /// Move every annotation row from `old` to `new` content hash — the Save
    /// (bake-in-place) path: the rewritten file has a new identity, and the
    /// user's ratings/tags/comments/collections must follow it. Orientation and
    /// crop instructions are NOT moved (they were just baked into the pixels).
    pub fn rekey_annotations(&self, old: &str, new: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        for sql in [
            "UPDATE OR REPLACE ratings SET content_hash = ?2 WHERE content_hash = ?1",
            "UPDATE OR REPLACE comments SET content_hash = ?2 WHERE content_hash = ?1",
            "UPDATE OR REPLACE file_tags SET content_hash = ?2 WHERE content_hash = ?1",
            "UPDATE OR REPLACE collection_items SET content_hash = ?2 WHERE content_hash = ?1",
            "DELETE FROM orientations WHERE content_hash = ?1",
            "DELETE FROM crops WHERE content_hash = ?1",
        ] {
            conn.execute(sql, rusqlite::params![old, new])?;
        }
        Ok(())
    }

    /// `(path, content_hash, rating, orientation, crop)` for present files under
    /// `folder` — one query to stamp the grid's in-memory items, so cell rating
    /// overlays and rating writes need no per-cell database hit. `rating` is 0
    /// when unrated; `orientation` is 1 (identity) when never rotated.
    #[allow(clippy::type_complexity)]
    pub fn ratings_under(
        &self,
        folder: &str,
    ) -> rusqlite::Result<Vec<(String, String, i64, i64, Option<(f64, f64, f64, f64)>)>> {
        // Runs on the main thread at every folder open — the path range (vs a
        // LIKE prefix) is what lets it use the path index instead of scanning
        // the whole files table (see `subtree_range`).
        let (lo, hi) = crate::query::subtree_range(folder);
        let mut stmt = self.conn().prepare(
            "SELECT f.path, f.content_hash, COALESCE(r.rating, 0), COALESCE(o.orientation, 1),
                    c.x, c.y, c.w, c.h
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
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, crop))
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
                ("/p/a.jpg".to_string(), "ha".to_string(), 4, 1, None),
                ("/p/b.jpg".to_string(), "hb".to_string(), 0, 1, None),
            ]
        );
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
