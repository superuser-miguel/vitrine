//! The SQLite index: open, migrate, and low-level access (PLAN §5).
//!
//! One writer connection is sufficient at personal-collection scale (WAL mode,
//! all multi-row mutations in explicit transactions). This module owns opening,
//! applying [`schema::MIGRATIONS`], and small `meta` helpers; higher-level
//! file/tag/collection/query logic builds on [`Db::conn`].

use std::path::Path;

use rusqlite::Connection;

use crate::schema::MIGRATIONS;

/// Current unix time in seconds — the `created_at` / `updated_at` stamp for
/// annotations written here.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A handle to the Vitrine index database.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database at `path` and migrate it.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Db> {
        Self::init(Connection::open(path)?)
    }

    /// Open a fresh in-memory database (for tests).
    pub fn open_in_memory() -> rusqlite::Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Apply any migrations newer than the database's current `user_version`,
    /// each atomically, then advance the version.
    fn migrate(&self) -> rusqlite::Result<()> {
        let current = self.schema_version()?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            self.conn.execute_batch(&format!(
                "BEGIN;\n{sql}\nPRAGMA user_version = {};\nCOMMIT;",
                i + 1
            ))?;
        }
        Ok(())
    }

    /// The database's current schema version.
    pub fn schema_version(&self) -> rusqlite::Result<usize> {
        let v: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        Ok(v as usize)
    }

    /// Read a `meta` value by key.
    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
    }

    /// Upsert a `meta` value.
    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// The underlying connection, for higher-level modules in this crate.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_to_current_version() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), crate::schema::SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        // Running migrate again from the current version is a no-op.
        db.migrate().unwrap();
        assert_eq!(db.schema_version().unwrap(), crate::schema::SCHEMA_VERSION);
    }

    #[test]
    fn expected_tables_exist() {
        let db = Db::open_in_memory().unwrap();
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type='table' AND name IN
                 ('meta','files','tags','file_tags','ratings','collections',
                  'collection_items','comments')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 8);
    }

    #[test]
    fn meta_round_trips() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta("created_at").unwrap(), None);
        db.set_meta("created_at", "1700000000").unwrap();
        assert_eq!(
            db.meta("created_at").unwrap().as_deref(),
            Some("1700000000")
        );
        db.set_meta("created_at", "1700000001").unwrap();
        assert_eq!(
            db.meta("created_at").unwrap().as_deref(),
            Some("1700000001")
        );
    }

    #[test]
    fn reopen_preserves_version_and_data() {
        let dir = std::env::temp_dir().join(format!("vitrine-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.sqlite");
        {
            let db = Db::open(&path).unwrap();
            db.set_meta("k", "v").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), crate::schema::SCHEMA_VERSION);
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("v"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
