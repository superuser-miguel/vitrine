//! Collections (PLAN Phase 3 / §10.2): one list, two kinds.
//!
//! - **Smart** — a stored [`Query`] predicate (JSON in `collections.query`);
//!   membership is recomputed by running the query, so it updates live as tags /
//!   ratings / metadata change.
//! - **Catalog** — a hand-curated, ordered list in `collection_items`.
//!
//! Everything keys on `content_hash`, so catalog membership survives renames just
//! like tags and ratings.

use rusqlite::OptionalExtension;

use crate::db::{now_secs, Db};
use crate::files::FileRecord;
use crate::query::Query;

/// A smart predicate or a hand-curated list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Smart,
    Catalog,
}

impl CollectionKind {
    fn from_str(s: &str) -> CollectionKind {
        match s {
            "smart" => CollectionKind::Smart,
            _ => CollectionKind::Catalog,
        }
    }
}

/// A collection row plus its live count of present files.
#[derive(Debug, Clone)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub kind: CollectionKind,
    /// The smart predicate JSON (`None` for catalogs).
    pub query: Option<String>,
    pub count: i64,
}

impl Db {
    /// Create a smart collection from a [`Query`] predicate. Returns its id.
    pub fn create_smart_collection(&self, name: &str, query: &Query) -> rusqlite::Result<i64> {
        let json = serde_json::to_string(query).map_err(ser_err)?;
        self.conn().execute(
            "INSERT INTO collections(name, kind, query, created_at) VALUES (?1,'smart',?2,?3)",
            rusqlite::params![name, json, now_secs()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Create an empty hand-curated catalog. Returns its id.
    pub fn create_catalog(&self, name: &str) -> rusqlite::Result<i64> {
        self.conn().execute(
            "INSERT INTO collections(name, kind, query, created_at) VALUES (?1,'catalog',NULL,?2)",
            rusqlite::params![name, now_secs()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    pub fn delete_collection(&self, id: i64) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM collections WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn rename_collection(&self, id: i64, name: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE collections SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name],
        )?;
        Ok(())
    }

    /// All collections with live present-file counts, ordered by name.
    pub fn list_collections(&self) -> rusqlite::Result<Vec<Collection>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, kind, query FROM collections ORDER BY name COLLATE NOCASE",
        )?;
        let rows: Vec<(i64, String, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, name, kind, query) in rows {
            let count = self.collection_files(id)?.len() as i64;
            out.push(Collection {
                id,
                name,
                kind: CollectionKind::from_str(&kind),
                query,
                count,
            });
        }
        Ok(out)
    }

    /// Resolve a collection to its present files: a smart collection runs its
    /// stored query; a catalog returns its items joined to files, in the curated
    /// order. Missing files are excluded from both.
    pub fn collection_files(&self, id: i64) -> rusqlite::Result<Vec<FileRecord>> {
        let row: Option<(String, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT kind, query FROM collections WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((kind, query)) = row else {
            return Ok(Vec::new());
        };
        match CollectionKind::from_str(&kind) {
            CollectionKind::Smart => {
                let q: Query = match query {
                    Some(json) => serde_json::from_str(&json).map_err(ser_err)?,
                    None => Query::default(),
                };
                self.query(&q)
            }
            CollectionKind::Catalog => {
                let mut stmt = self.conn().prepare(
                    "SELECT f.* FROM collection_items ci
                     JOIN files f ON f.content_hash = ci.content_hash AND f.missing = 0
                     WHERE ci.collection_id = ?1
                     GROUP BY ci.content_hash
                     ORDER BY ci.position",
                )?;
                let rows = stmt.query_map([id], FileRecord::from_row)?;
                rows.collect()
            }
        }
    }

    /// Append hashes to a catalog (in the given order, after the current last
    /// item), in one transaction. Idempotent per (collection, hash).
    pub fn add_to_catalog(&self, id: i64, hashes: &[String]) -> rusqlite::Result<()> {
        let conn = self.conn();
        let mut next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM collection_items WHERE collection_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        conn.execute_batch("BEGIN")?;
        let result = (|| {
            let mut stmt = conn.prepare(
                "INSERT OR IGNORE INTO collection_items(collection_id, content_hash, position)
                 VALUES (?1, ?2, ?3)",
            )?;
            for hash in hashes {
                if stmt.execute(rusqlite::params![id, hash, next])? > 0 {
                    next += 1;
                }
            }
            Ok(())
        })();
        commit_or_rollback(conn, result)
    }

    pub fn remove_from_catalog(&self, id: i64, hashes: &[String]) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| {
            let mut stmt = conn.prepare(
                "DELETE FROM collection_items WHERE collection_id = ?1 AND content_hash = ?2",
            )?;
            for hash in hashes {
                stmt.execute(rusqlite::params![id, hash])?;
            }
            Ok(())
        })();
        commit_or_rollback(conn, result)
    }

    /// Set the catalog's order to exactly `hashes` (positions 0..n); hashes not
    /// listed are left untouched at their old positions after the reordered set.
    pub fn set_catalog_order(&self, id: i64, hashes: &[String]) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| {
            let mut stmt = conn.prepare(
                "UPDATE collection_items SET position = ?3
                 WHERE collection_id = ?1 AND content_hash = ?2",
            )?;
            for (pos, hash) in hashes.iter().enumerate() {
                stmt.execute(rusqlite::params![id, hash, pos as i64])?;
            }
            Ok(())
        })();
        commit_or_rollback(conn, result)
    }
}

fn commit_or_rollback(
    conn: &rusqlite::Connection,
    result: rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

fn ser_err(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(db: &Db, path: &str, hash: &str) {
        db.upsert_file(&FileRecord {
            path: path.into(),
            content_hash: hash.into(),
            size: 1,
            mtime: 1,
            indexed_at: 1,
            ..Default::default()
        })
        .unwrap();
    }
    fn h(hs: &[&str]) -> Vec<String> {
        hs.iter().map(|s| s.to_string()).collect()
    }
    fn paths(rows: Vec<FileRecord>) -> Vec<String> {
        rows.into_iter().map(|r| r.path).collect()
    }

    #[test]
    fn smart_collection_resolves_live() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", "ha");
        seed(&db, "/b.jpg", "hb");
        db.set_rating("ha", 5).unwrap();

        let id = db
            .create_smart_collection(
                "Top rated",
                &Query {
                    rating_min: Some(4),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(paths(db.collection_files(id).unwrap()), ["/a.jpg"]);

        // Live: rate /b.jpg → it joins the smart collection with no edit to it.
        db.set_rating("hb", 4).unwrap();
        assert_eq!(
            paths(db.collection_files(id).unwrap()),
            ["/a.jpg", "/b.jpg"]
        );
    }

    #[test]
    fn catalog_preserves_and_reorders() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", "ha");
        seed(&db, "/b.jpg", "hb");
        seed(&db, "/c.jpg", "hc");
        let id = db.create_catalog("Trip").unwrap();
        db.add_to_catalog(id, &h(&["hc", "ha"])).unwrap();
        db.add_to_catalog(id, &h(&["hb", "ha"])).unwrap(); // ha ignored (dup)
        assert_eq!(
            paths(db.collection_files(id).unwrap()),
            ["/c.jpg", "/a.jpg", "/b.jpg"],
            "curated insertion order"
        );

        db.set_catalog_order(id, &h(&["ha", "hb", "hc"])).unwrap();
        assert_eq!(
            paths(db.collection_files(id).unwrap()),
            ["/a.jpg", "/b.jpg", "/c.jpg"]
        );

        db.remove_from_catalog(id, &h(&["hb"])).unwrap();
        assert_eq!(
            paths(db.collection_files(id).unwrap()),
            ["/a.jpg", "/c.jpg"]
        );

        let coll = db
            .list_collections()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        assert_eq!(coll.count, 2);
        assert_eq!(coll.kind, CollectionKind::Catalog);
    }

    #[test]
    fn removing_from_a_catalog_keeps_the_file_and_its_annotations() {
        // A collection is curation, not storage: dropping a member must leave the
        // file indexed, present, and annotated — it is still on disk, and may
        // still be in other catalogs. The UI once had no way to reach this, so
        // Delete in a collection view trashed the original instead.
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", "ha");
        seed(&db, "/b.jpg", "hb");
        db.set_rating("ha", 5).unwrap();
        db.apply_tag("keeper", &h(&["ha"])).unwrap();

        let trip = db.create_catalog("Trip").unwrap();
        let best = db.create_catalog("Best").unwrap();
        db.add_to_catalog(trip, &h(&["ha", "hb"])).unwrap();
        db.add_to_catalog(best, &h(&["ha"])).unwrap();

        db.remove_from_catalog(trip, &h(&["ha"])).unwrap();

        assert_eq!(
            paths(db.collection_files(trip).unwrap()),
            ["/b.jpg"],
            "dropped from the catalog it was removed from"
        );
        assert_eq!(
            paths(db.collection_files(best).unwrap()),
            ["/a.jpg"],
            "still a member of every other catalog"
        );

        let file = db.file_by_path("/a.jpg").unwrap().expect("still indexed");
        assert_eq!(file.content_hash, "ha");
        assert!(!file.missing, "removal must never mark the file missing");
        assert_eq!(db.tags_for_hash("ha").unwrap(), ["keeper"]);
    }

    #[test]
    fn tag_rating_and_catalog_all_survive_a_move() {
        // The Phase 3 acceptance: annotations follow content across a rename.
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/old.jpg", "H");
        db.apply_tag("keeper", &h(&["H"])).unwrap();
        db.set_rating("H", 5).unwrap();
        db.set_comment("H", "great shot").unwrap();
        let cat = db.create_catalog("Best").unwrap();
        db.add_to_catalog(cat, &h(&["H"])).unwrap();

        // Move the file: old path vanishes, same bytes reappear at a new path.
        db.mark_missing("/old.jpg").unwrap();
        assert!(db.relink_path("/old.jpg", "/new.jpg", 2).unwrap());

        assert_eq!(db.tags_for_hash("H").unwrap(), vec!["keeper"]);
        assert_eq!(db.rating("H").unwrap(), Some(5));
        assert_eq!(db.comment("H").unwrap().as_deref(), Some("great shot"));
        assert_eq!(paths(db.collection_files(cat).unwrap()), ["/new.jpg"]);
    }
}
