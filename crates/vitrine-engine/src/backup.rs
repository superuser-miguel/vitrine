//! Backup and portable export (PLAN §5, Phase 2 acceptance).
//!
//! Because the DB is canonical in v1 (no embedded metadata), losing it loses
//! everything — so this ships from day one:
//! - [`Db::backup_to`] — a whole-database snapshot via `VACUUM INTO`.
//! - [`Db::export_json`] / [`Db::import_json`] — tags, ratings and collections
//!   keyed by **`content_hash`**, so they restore even into a fresh index built
//!   from moved/renamed files (import merges; it never deletes).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// Current export format version.
pub const EXPORT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Export {
    pub version: u32,
    pub file_tags: Vec<FileTagsExport>,
    pub ratings: Vec<RatingExport>,
    pub collections: Vec<CollectionExport>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct FileTagsExport {
    pub content_hash: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct RatingExport {
    pub content_hash: String,
    pub rating: i64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct CollectionExport {
    pub name: String,
    pub kind: String,
    pub query: Option<String>,
    pub items: Vec<String>,
}

impl Db {
    /// Snapshot the entire database to `path` via `VACUUM INTO` (a consistent,
    /// compacted copy; safe while the DB is open).
    pub fn backup_to(&self, path: impl AsRef<Path>) -> rusqlite::Result<()> {
        let dest = path.as_ref().to_string_lossy().replace('\'', "''");
        self.conn()
            .execute_batch(&format!("VACUUM INTO '{dest}'"))?;
        Ok(())
    }

    /// Export tags/ratings/collections (content-hash keyed) as JSON.
    pub fn export_json(&self) -> rusqlite::Result<String> {
        let export = self.export()?;
        serde_json::to_string_pretty(&export).map_err(to_sql_err)
    }

    fn export(&self) -> rusqlite::Result<Export> {
        let conn = self.conn();

        let mut stmt = conn.prepare(
            "SELECT ft.content_hash, t.name
             FROM file_tags ft JOIN tags t ON ft.tag_id = t.id
             ORDER BY ft.content_hash, t.name",
        )?;
        let mut file_tags: Vec<FileTagsExport> = Vec::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (hash, tag) = row?;
            match file_tags.last_mut() {
                Some(last) if last.content_hash == hash => last.tags.push(tag),
                _ => file_tags.push(FileTagsExport {
                    content_hash: hash,
                    tags: vec![tag],
                }),
            }
        }

        let mut stmt =
            conn.prepare("SELECT content_hash, rating FROM ratings ORDER BY content_hash")?;
        let ratings = stmt
            .query_map([], |r| {
                Ok(RatingExport {
                    content_hash: r.get(0)?,
                    rating: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = conn.prepare("SELECT id, name, kind, query FROM collections ORDER BY id")?;
        let colls = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut collections = Vec::new();
        for (id, name, kind, query) in colls {
            let mut items_stmt = conn.prepare(
                "SELECT content_hash FROM collection_items WHERE collection_id = ?1 ORDER BY position",
            )?;
            let items = items_stmt
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            collections.push(CollectionExport {
                name,
                kind,
                query,
                items,
            });
        }

        Ok(Export {
            version: EXPORT_VERSION,
            file_tags,
            ratings,
            collections,
        })
    }

    /// Merge a JSON export into this database (never deletes). Tags are created
    /// as needed; ratings upsert; collections are inserted with their items.
    pub fn import_json(&self, json: &str) -> rusqlite::Result<()> {
        let export: Export = serde_json::from_str(json).map_err(to_sql_err)?;
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = self.import(&export);
        if result.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            conn.execute_batch("ROLLBACK")?;
        }
        result
    }

    fn import(&self, export: &Export) -> rusqlite::Result<()> {
        let conn = self.conn();
        for entry in &export.file_tags {
            for tag in &entry.tags {
                conn.execute("INSERT OR IGNORE INTO tags(name) VALUES (?1)", [tag])?;
                let tag_id: i64 =
                    conn.query_row("SELECT id FROM tags WHERE name = ?1", [tag], |r| r.get(0))?;
                conn.execute(
                    "INSERT OR IGNORE INTO file_tags(content_hash, tag_id, created_at)
                     VALUES (?1, ?2, 0)",
                    rusqlite::params![entry.content_hash, tag_id],
                )?;
            }
        }
        for rating in &export.ratings {
            conn.execute(
                "INSERT INTO ratings(content_hash, rating, updated_at) VALUES (?1, ?2, 0)
                 ON CONFLICT(content_hash) DO UPDATE SET rating = excluded.rating",
                rusqlite::params![rating.content_hash, rating.rating],
            )?;
        }
        for coll in &export.collections {
            conn.execute(
                "INSERT INTO collections(name, kind, query, created_at) VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![coll.name, coll.kind, coll.query],
            )?;
            let coll_id = conn.last_insert_rowid();
            for (pos, hash) in coll.items.iter().enumerate() {
                conn.execute(
                    "INSERT OR IGNORE INTO collection_items(collection_id, content_hash, position)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![coll_id, hash, pos as i64],
                )?;
            }
        }
        Ok(())
    }
}

fn to_sql_err(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(db: &Db) {
        db.conn()
            .execute_batch(
                "INSERT INTO tags(name) VALUES ('fave'),('car');
                 INSERT INTO file_tags(content_hash, tag_id, created_at) VALUES
                   ('H1', 1, 0), ('H1', 2, 0), ('H2', 1, 0);
                 INSERT INTO ratings(content_hash, rating, updated_at) VALUES ('H1', 5, 0);
                 INSERT INTO collections(name, kind, query, created_at)
                   VALUES ('Cars','catalog',NULL,0);
                 INSERT INTO collection_items(collection_id, content_hash, position)
                   VALUES (1,'H1',0),(1,'H2',1);",
            )
            .unwrap();
    }

    #[test]
    fn export_import_round_trips() {
        let src = Db::open_in_memory().unwrap();
        seed(&src);
        let json = src.export_json().unwrap();

        let dst = Db::open_in_memory().unwrap();
        dst.import_json(&json).unwrap();

        // Same content-keyed data lands in the fresh DB.
        assert_eq!(dst.export().unwrap(), src.export().unwrap());
        let h1_tags: i64 = dst
            .conn()
            .query_row(
                "SELECT count(*) FROM file_tags WHERE content_hash='H1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(h1_tags, 2);
    }

    #[test]
    fn import_merges_and_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        let json = db.export_json().unwrap();
        // Re-importing the same export must not duplicate tags/ratings.
        db.import_json(&json).unwrap();
        let tag_count: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag_count, 2, "tags de-duped by name");
        let rating: i64 = db
            .conn()
            .query_row(
                "SELECT rating FROM ratings WHERE content_hash='H1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rating, 5);
    }

    #[test]
    fn backup_to_file_reopens_with_data() {
        let dir = std::env::temp_dir().join(format!("vitrine-backup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = Db::open_in_memory().unwrap();
        seed(&src);
        let backup = dir.join("backup.sqlite");
        src.backup_to(&backup).unwrap();

        let restored = Db::open(&backup).unwrap();
        assert_eq!(restored.export().unwrap(), src.export().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
