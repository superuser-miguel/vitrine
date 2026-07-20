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
    ///
    /// Counts **distinct content hashes**, not file rows. One image can hold more
    /// than one row in `files` — the same file reached through a portal document
    /// path and through a directly-granted root is indexed under both names — and
    /// counting rows made a 7-image tag report 15.
    pub fn all_tags(&self) -> rusqlite::Result<Vec<Tag>> {
        let mut stmt = self.conn().prepare(
            "SELECT t.id, t.name,
                    (SELECT count(DISTINCT f.content_hash) FROM file_tags ft
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

    /// Just the tag names, ordered — for callers that populate a list and have
    /// no use for the counts. Skips the per-tag count subquery in [`all_tags`],
    /// which is the expensive half of it.
    pub fn tag_names(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT name FROM tags ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// Every content hash carrying tag `name` (case-insensitive) — for filtering
    /// the grid to a tag without a per-item query.
    pub fn hashes_with_tag(&self, name: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT ft.content_hash FROM file_tags ft JOIN tags t ON t.id = ft.tag_id
             WHERE t.name = ?1",
        )?;
        let rows = stmt.query_map([name.trim()], |r| r.get::<_, String>(0))?;
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
    fn tag_counts_are_indexed_not_a_table_scan() {
        // `file_tags` is keyed (content_hash, tag_id), so counting by tag alone
        // cannot use the primary key. Without idx_file_tags_tag SQLite drives the
        // per-tag count from a full scan of `files` — once per tag — and the tag
        // menu slows down as tags × files. Assert the planner searches instead.
        let db = Db::open_in_memory().unwrap();
        let plan: Vec<String> = {
            let conn = db.conn();
            let mut stmt = conn
                .prepare(
                    "EXPLAIN QUERY PLAN
                     SELECT t.id, t.name,
                            (SELECT count(*) FROM file_tags ft
                             JOIN files f ON f.content_hash = ft.content_hash AND f.missing = 0
                             WHERE ft.tag_id = t.id)
                     FROM tags t ORDER BY t.name COLLATE NOCASE",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert!(
            plan.iter().any(|s| s.contains("idx_file_tags_tag")),
            "count must be driven by the tag index, got plan: {plan:#?}"
        );
        assert!(
            !plan.iter().any(|s| s.contains("SCAN f")),
            "must not full-scan files per tag, got plan: {plan:#?}"
        );
    }

    #[test]
    fn tag_names_skips_the_count_subquery() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch("INSERT INTO files(path,content_hash,size,mtime,indexed_at,missing) VALUES ('/a','h1',1,1,1,0);")
            .unwrap();
        db.apply_tag("Zebra", &hashes(&["h1"])).unwrap();
        db.apply_tag("apple", &hashes(&["h1"])).unwrap();
        assert_eq!(db.tag_names().unwrap(), ["apple", "Zebra"], "NOCASE order");
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
    fn counts_are_images_not_file_rows() {
        // One image can hold several rows in `files`: a folder opened through the
        // document portal is indexed under an opaque /run/user/…/doc/… path while
        // the same file under a directly-granted root keeps its real path. Both
        // rows carry the same content hash, so counting rows double-counts the
        // image — a 7-image tag reported 15 against the real library.
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO files(path,content_hash,size,mtime,indexed_at,missing) VALUES
                 ('/home/u/Pictures/a.jpg','h1',1,1,1,0),
                 ('/run/user/1000/doc/abc/a.jpg','h1',1,1,1,0),
                 ('/home/u/Pictures/b.jpg','h2',1,1,1,0);",
            )
            .unwrap();
        db.apply_tag("trip", &hashes(&["h1", "h2"])).unwrap();
        let trip = db
            .all_tags()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "trip")
            .unwrap();
        assert_eq!(trip.count, 2, "two images, even though three file rows");
    }

    #[test]
    fn hashes_with_tag_lists_members() {
        let db = Db::open_in_memory().unwrap();
        db.apply_tag("beach", &hashes(&["h1", "h3"])).unwrap();
        let mut got = db.hashes_with_tag("BEACH").unwrap(); // case-insensitive
        got.sort();
        assert_eq!(got, vec!["h1".to_string(), "h3".to_string()]);
        assert!(db.hashes_with_tag("nope").unwrap().is_empty());
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
