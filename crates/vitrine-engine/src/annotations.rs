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
