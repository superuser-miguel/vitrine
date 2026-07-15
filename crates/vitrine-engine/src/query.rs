//! Sort/filter queries over the `files` table.
//!
//! This is the layer the app's grid uses to reorder or narrow a browsed folder
//! by *indexed* metadata — sort by capture date, filter to one camera, and so
//! on — reading the columns the enrichment pass backfilled. It's pure: a typed
//! [`Query`] compiles to **parameterized** SQL (no string interpolation of
//! values; the sort column and direction come from closed enums), so it is
//! injection-safe and fully testable headless.
//!
//! Un-enriched or `missing` files don't disappear from the browser — the grid
//! still shows the folder as enumerated — but a metadata sort naturally sinks
//! rows whose key is still `NULL` to the end (NULLS LAST) until enrichment fills
//! them in.

use rusqlite::types::Value;

use crate::db::Db;
use crate::files::{FileRecord, SELECT_COLS};

/// What to order results by. Closed set → the column name is never user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// File path (the browser's default; always present).
    #[default]
    Name,
    /// EXIF capture time (nullable → NULLS LAST).
    DateTaken,
    /// File size in bytes.
    Size,
    /// Filesystem modification time.
    Mtime,
    /// Pixel area, `width * height` (nullable → NULLS LAST).
    Area,
    /// When the row was indexed (recently-added first, with `Desc`).
    IndexedAt,
}

impl SortKey {
    /// The SQL expression and whether it can be `NULL` (so we sink NULLs last).
    fn column(self) -> (&'static str, bool) {
        match self {
            SortKey::Name => ("path", false),
            SortKey::DateTaken => ("date_taken", true),
            SortKey::Size => ("size", false),
            SortKey::Mtime => ("mtime", false),
            SortKey::Area => ("(width * height)", true),
            SortKey::IndexedAt => ("indexed_at", false),
        }
    }
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}

impl Direction {
    fn keyword(self) -> &'static str {
        match self {
            Direction::Asc => "ASC",
            Direction::Desc => "DESC",
        }
    }
}

/// A sort/filter over indexed files. Build with `..Default::default()` and set
/// only the fields you need; the default is every present file, by name ascending.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Restrict to a folder subtree (files whose path is under this directory).
    pub under: Option<String>,
    /// Exact camera label match (as stored, "Make Model").
    pub camera: Option<String>,
    /// Exact format/content-type match.
    pub format: Option<String>,
    /// Capture time lower bound (unix seconds, inclusive).
    pub date_from: Option<i64>,
    /// Capture time upper bound (unix seconds, inclusive).
    pub date_to: Option<i64>,
    /// Exact EXIF orientation (1..=8).
    pub orientation: Option<i64>,
    pub sort: SortKey,
    pub direction: Direction,
    /// Cap on rows returned.
    pub limit: Option<i64>,
}

impl Query {
    /// Compile to `(sql, params)`. Present-only (`missing = 0`); a stable `path`
    /// tiebreak makes ordering deterministic across equal keys.
    fn build(&self) -> (String, Vec<Value>) {
        let mut sql = format!("SELECT {SELECT_COLS} FROM files WHERE missing = 0");
        let mut params: Vec<Value> = Vec::new();

        if let Some(under) = &self.under {
            // Match the directory's subtree via a prefix LIKE, escaping any LIKE
            // metacharacters in the (arbitrary) path.
            let prefix = format!("{}/%", escape_like(under.trim_end_matches('/')));
            sql.push_str(" AND path LIKE ? ESCAPE '\\'");
            params.push(Value::Text(prefix));
        }
        if let Some(camera) = &self.camera {
            sql.push_str(" AND camera = ?");
            params.push(Value::Text(camera.clone()));
        }
        if let Some(format) = &self.format {
            sql.push_str(" AND format = ?");
            params.push(Value::Text(format.clone()));
        }
        if let Some(from) = self.date_from {
            sql.push_str(" AND date_taken >= ?");
            params.push(Value::Integer(from));
        }
        if let Some(to) = self.date_to {
            sql.push_str(" AND date_taken <= ?");
            params.push(Value::Integer(to));
        }
        if let Some(orientation) = self.orientation {
            sql.push_str(" AND orientation = ?");
            params.push(Value::Integer(orientation));
        }

        let (expr, nullable) = self.sort.column();
        sql.push_str(" ORDER BY ");
        if nullable {
            // Keep NULL keys at the end regardless of direction.
            sql.push_str(&format!("({expr} IS NULL), "));
        }
        sql.push_str(&format!("{expr} {}, path ASC", self.direction.keyword()));

        if let Some(limit) = self.limit {
            sql.push_str(" LIMIT ?");
            params.push(Value::Integer(limit));
        }

        (sql, params)
    }
}

impl Db {
    /// Run a [`Query`], returning matching file rows in the requested order.
    pub fn query(&self, query: &Query) -> rusqlite::Result<Vec<FileRecord>> {
        let (sql, params) = query.build();
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), FileRecord::from_row)?;
        rows.collect()
    }
}

/// Escape `\`, `%`, `_` for use inside a `LIKE ... ESCAPE '\'` pattern.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::Enrichment;
    use crate::FileRecord;

    /// Seed a file with size/mtime and (optionally) enrichment, return its path.
    fn seed(db: &Db, path: &str, size: i64, mtime: i64, enrich: Option<Enrichment>) {
        db.upsert_file(&FileRecord {
            path: path.into(),
            content_hash: format!("h{path}"),
            size,
            mtime,
            indexed_at: mtime,
            ..Default::default()
        })
        .unwrap();
        if let Some(e) = enrich {
            db.set_enrichment(path, &e).unwrap();
        }
    }

    fn dated(width: i64, height: i64, date: Option<i64>, camera: Option<&str>) -> Enrichment {
        Enrichment {
            width,
            height,
            date_taken: date,
            camera: camera.map(str::to_string),
            ..Default::default()
        }
    }

    fn paths(rows: Vec<FileRecord>) -> Vec<String> {
        rows.into_iter().map(|r| r.path).collect()
    }

    #[test]
    fn sort_by_date_taken_puts_nulls_last() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/c.jpg", 1, 1, Some(dated(4, 4, Some(300), None)));
        seed(&db, "/a.jpg", 1, 1, Some(dated(4, 4, Some(100), None)));
        seed(&db, "/b.jpg", 1, 1, Some(dated(4, 4, Some(200), None)));
        seed(&db, "/z.jpg", 1, 1, None); // un-enriched → NULL date

        let q = Query {
            sort: SortKey::DateTaken,
            ..Default::default()
        };
        // Ascending by date, then the NULL-date file last.
        assert_eq!(
            paths(db.query(&q).unwrap()),
            ["/a.jpg", "/b.jpg", "/c.jpg", "/z.jpg"]
        );

        let q_desc = Query {
            sort: SortKey::DateTaken,
            direction: Direction::Desc,
            ..Default::default()
        };
        // Descending by date, NULL still last (not first).
        assert_eq!(
            paths(db.query(&q_desc).unwrap()),
            ["/c.jpg", "/b.jpg", "/a.jpg", "/z.jpg"]
        );
    }

    #[test]
    fn filter_by_camera_and_scope_to_subtree() {
        let db = Db::open_in_memory().unwrap();
        seed(
            &db,
            "/pics/trip/1.jpg",
            1,
            1,
            Some(dated(4, 4, Some(10), Some("SONY A7"))),
        );
        seed(
            &db,
            "/pics/trip/2.jpg",
            1,
            1,
            Some(dated(4, 4, Some(20), Some("NIKON Z6"))),
        );
        seed(
            &db,
            "/pics/other/3.jpg",
            1,
            1,
            Some(dated(4, 4, Some(30), Some("SONY A7"))),
        );

        // Only SONY, only under /pics/trip.
        let q = Query {
            under: Some("/pics/trip".into()),
            camera: Some("SONY A7".into()),
            ..Default::default()
        };
        assert_eq!(paths(db.query(&q).unwrap()), ["/pics/trip/1.jpg"]);

        // Subtree alone catches both files directly under it, not the sibling dir.
        let q_dir = Query {
            under: Some("/pics/trip/".into()), // trailing slash tolerated
            ..Default::default()
        };
        assert_eq!(
            paths(db.query(&q_dir).unwrap()),
            ["/pics/trip/1.jpg", "/pics/trip/2.jpg"]
        );
    }

    #[test]
    fn date_range_and_size_sort() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", 300, 1, Some(dated(4, 4, Some(100), None)));
        seed(&db, "/b.jpg", 100, 1, Some(dated(4, 4, Some(200), None)));
        seed(&db, "/c.jpg", 200, 1, Some(dated(4, 4, Some(300), None)));

        // Date window [150, 350] excludes /a.jpg; largest first.
        let q = Query {
            date_from: Some(150),
            date_to: Some(350),
            sort: SortKey::Size,
            direction: Direction::Desc,
            ..Default::default()
        };
        assert_eq!(paths(db.query(&q).unwrap()), ["/c.jpg", "/b.jpg"]);
    }

    #[test]
    fn area_sort_and_limit() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/small.jpg", 1, 1, Some(dated(100, 100, None, None))); // 10k
        seed(&db, "/big.jpg", 1, 1, Some(dated(4000, 3000, None, None))); // 12M
        seed(&db, "/mid.jpg", 1, 1, Some(dated(1000, 1000, None, None))); // 1M

        let q = Query {
            sort: SortKey::Area,
            direction: Direction::Desc,
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(paths(db.query(&q).unwrap()), ["/big.jpg", "/mid.jpg"]);
    }

    #[test]
    fn missing_files_are_excluded() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/here.jpg", 1, 1, Some(dated(4, 4, Some(10), None)));
        seed(&db, "/gone.jpg", 1, 1, Some(dated(4, 4, Some(20), None)));
        db.mark_missing("/gone.jpg").unwrap();

        let rows = db.query(&Query::default()).unwrap();
        assert_eq!(paths(rows), ["/here.jpg"]);
    }

    #[test]
    fn like_metacharacters_in_scope_are_escaped() {
        let db = Db::open_in_memory().unwrap();
        // A directory whose name contains a LIKE wildcard.
        seed(&db, "/a_b/1.jpg", 1, 1, None);
        seed(&db, "/axb/2.jpg", 1, 1, None); // '_' would match 'x' if unescaped

        let q = Query {
            under: Some("/a_b".into()),
            ..Default::default()
        };
        // Only the literal "/a_b" subtree, not "/axb".
        assert_eq!(paths(db.query(&q).unwrap()), ["/a_b/1.jpg"]);
    }
}
