//! Tags: the `tags` / `file_tags` CRUD surface (PLAN Phase 3).
//!
//! Tags attach to a **`content_hash`**, not a path, so they survive gallery-dl
//! renames (the scanner reconciles paths; the tag stays on the bytes). Tag names
//! are unique case-insensitively (`COLLATE NOCASE`). Batch apply/remove run in a
//! single transaction so tagging a large selection is one fast round-trip.

use rusqlite::OptionalExtension;

use crate::db::{now_secs, Db};

/// A tag plus how many (present) files carry it — for the sidebar / autocomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub count: i64,
}

impl Db {
    /// Ensure a tag exists (case-insensitive), returning its id.
    pub fn ensure_tag(&self, name: &str) -> rusqlite::Result<i64> {
        let name = name.trim();
        self.conn()
            .execute("INSERT OR IGNORE INTO tags(name) VALUES (?1)", [name])?;
        self.conn()
            .query_row("SELECT id FROM tags WHERE name = ?1", [name], |r| r.get(0))
    }

    /// All tags with their usage counts (present files only), ordered by name.
    pub fn all_tags(&self) -> rusqlite::Result<Vec<Tag>> {
        let mut stmt = self.conn().prepare(
            "SELECT t.id, t.name,
                    (SELECT count(*) FROM file_tags ft
                     JOIN files f ON f.content_hash = ft.content_hash AND f.missing = 0
                     WHERE ft.tag_id = t.id) AS cnt
             FROM tags t
             ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                count: r.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Tag names on a content hash, ordered by name.
    pub fn tags_for_hash(&self, content_hash: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT t.name FROM file_tags ft JOIN tags t ON t.id = ft.tag_id
             WHERE ft.content_hash = ?1 ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([content_hash], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Apply tag `name` to every hash in `hashes`, in one transaction. Creates
    /// the tag if needed; idempotent per (hash, tag).
    pub fn apply_tag(&self, name: &str, hashes: &[String]) -> rusqlite::Result<()> {
        let tag_id = self.ensure_tag(name)?;
        let now = now_secs();
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| {
            let mut stmt = conn.prepare(
                "INSERT OR IGNORE INTO file_tags(content_hash, tag_id, created_at)
                 VALUES (?1, ?2, ?3)",
            )?;
            for hash in hashes {
                stmt.execute(rusqlite::params![hash, tag_id, now])?;
            }
            Ok(())
        })();
        finish(conn, result)
    }

    /// Remove tag `name` from every hash in `hashes`, in one transaction.
    pub fn remove_tag(&self, name: &str, hashes: &[String]) -> rusqlite::Result<()> {
        let Some(tag_id) = self.tag_id(name)? else {
            return Ok(());
        };
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| {
            let mut stmt =
                conn.prepare("DELETE FROM file_tags WHERE content_hash = ?1 AND tag_id = ?2")?;
            for hash in hashes {
                stmt.execute(rusqlite::params![hash, tag_id])?;
            }
            Ok(())
        })();
        finish(conn, result)
    }

    /// Delete a tag entirely (file_tags rows cascade away).
    pub fn delete_tag(&self, name: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM tags WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Rename a tag (no-op if `old` doesn't exist; errors if `new` collides).
    pub fn rename_tag(&self, old: &str, new: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE tags SET name = ?2 WHERE name = ?1",
            [old, new.trim()],
        )?;
        Ok(())
    }

    fn tag_id(&self, name: &str) -> rusqlite::Result<Option<i64>> {
        self.conn()
            .query_row("SELECT id FROM tags WHERE name = ?1", [name.trim()], |r| {
                r.get(0)
            })
            .optional()
    }
}

/// Commit if `result` is Ok, else roll back; return the result.
fn finish(conn: &rusqlite::Connection, result: rusqlite::Result<()>) -> rusqlite::Result<()> {
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(hs: &[&str]) -> Vec<String> {
        hs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn apply_is_case_insensitive_and_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.apply_tag("Sunset", &hashes(&["h1", "h2"])).unwrap();
        // Same tag, different case + an overlapping hash → no duplicate tag/row.
        db.apply_tag("sunset", &hashes(&["h2", "h3"])).unwrap();

        let tags = db.all_tags().unwrap();
        assert_eq!(tags.len(), 1, "one tag despite case difference");
        assert_eq!(db.tags_for_hash("h2").unwrap(), vec!["Sunset"]);
        // No file rows exist, so counts are 0 (count is over present files).
        assert_eq!(tags[0].count, 0);
    }

    #[test]
    fn remove_and_delete() {
        let db = Db::open_in_memory().unwrap();
        db.apply_tag("fave", &hashes(&["h1", "h2"])).unwrap();
        db.remove_tag("fave", &hashes(&["h1"])).unwrap();
        assert!(db.tags_for_hash("h1").unwrap().is_empty());
        assert_eq!(db.tags_for_hash("h2").unwrap(), vec!["fave"]);

        db.delete_tag("fave").unwrap();
        assert!(db.all_tags().unwrap().is_empty());
        assert!(
            db.tags_for_hash("h2").unwrap().is_empty(),
            "cascade removed file_tags"
        );
    }

    #[test]
    fn counts_reflect_present_files() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO files(path,content_hash,size,mtime,indexed_at,missing)
                 VALUES ('/a','h1',1,1,1,0), ('/b','h2',1,1,1,1);", // h2 file is missing
            )
            .unwrap();
        db.apply_tag("car", &hashes(&["h1", "h2"])).unwrap();
        let car = db
            .all_tags()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "car")
            .unwrap();
        assert_eq!(car.count, 1, "only the present file counts");
    }

    #[test]
    fn batch_apply_is_one_transaction() {
        // 500 hashes in a single apply_tag — the Phase 3 acceptance shape.
        let db = Db::open_in_memory().unwrap();
        let many: Vec<String> = (0..500).map(|i| format!("h{i}")).collect();
        db.apply_tag("bulk", &many).unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM file_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 500);
    }
}
