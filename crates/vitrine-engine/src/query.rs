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
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::files::{FileRecord, SELECT_COLS};

/// What to order results by. Closed set → the column name is never user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
///
/// It is also a **smart collection's stored predicate**: `#[serde(default)]` lets
/// a saved query hold only the fields it constrains, and every value is a bound
/// parameter (tag names, camera, etc.), so a persisted query can never inject SQL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Query {
    /// Restrict to a folder subtree (files whose path is under this directory).
    pub under: Option<String>,
    /// Must carry *all* of these tags (case-insensitive).
    pub tags_all: Vec<String>,
    /// Must carry *at least one* of these tags (case-insensitive).
    pub tags_any: Vec<String>,
    /// Minimum star rating (inclusive).
    pub rating_min: Option<i64>,
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
            // Match the directory's subtree via an index-friendly path range.
            let (lo, hi) = subtree_range(under);
            sql.push_str(" AND path >= ? AND path < ?");
            params.push(Value::Text(lo));
            params.push(Value::Text(hi));
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
        // Tags — names bound as parameters (never interpolated). Matching uses the
        // tags.name NOCASE collation (case-insensitive).
        //
        // Both predicates are written as `content_hash IN (…)` rather than a
        // correlated subquery on `files`. A correlated form makes the tag the
        // *inner* term, so SQLite scans every row of `files` and probes per row —
        // a full-library scan to answer a question about a handful of tagged
        // files. Driving from `file_tags` instead lets the planner start from the
        // few matching rows and hit `idx_files_hash`, which on a 193k-file index
        // is the difference between ~100ms and under a millisecond.
        if !self.tags_all.is_empty() {
            let names = dedup_ci(&self.tags_all);
            sql.push_str(&format!(
                " AND content_hash IN (SELECT ft.content_hash FROM file_tags ft
                                       JOIN tags t ON t.id = ft.tag_id
                                       WHERE t.name IN ({})
                                       GROUP BY ft.content_hash
                                       HAVING count(DISTINCT t.name) = ?)",
                placeholders(names.len())
            ));
            params.extend(names.iter().map(|n| Value::Text(n.clone())));
            params.push(Value::Integer(names.len() as i64));
        }
        if !self.tags_any.is_empty() {
            sql.push_str(&format!(
                " AND content_hash IN (SELECT ft.content_hash FROM file_tags ft
                                       JOIN tags t ON t.id = ft.tag_id
                                       WHERE t.name IN ({}))",
                placeholders(self.tags_any.len())
            ));
            params.extend(self.tags_any.iter().map(|n| Value::Text(n.clone())));
        }
        if let Some(min) = self.rating_min {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM ratings r
                              WHERE r.content_hash = files.content_hash AND r.rating >= ?)",
            );
            params.push(Value::Integer(min));
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

/// `?,?,…` — `n` bound-parameter placeholders for an `IN (...)` list.
fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// De-duplicate tag names case-insensitively (so `tags_all` count matching is
/// correct when a caller passes "Cat" and "cat").
fn dedup_ci(names: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for n in names {
        if seen.insert(n.to_lowercase()) {
            out.push(n.clone());
        }
    }
    out
}

/// Half-open range `[lo, hi)` containing exactly the paths under `root` (those
/// prefixed by `root` + `/`): `lo` = `root/`, `hi` = `root0` — `'0'` is the byte
/// after `'/'`, so under BINARY collation every subtree path and nothing else
/// sorts inside. Unlike a `LIKE 'root/%'` (whose default case-insensitive match
/// defeats the BINARY `path` index → full table scan, 36ms at 143k rows on the
/// main thread per folder open), this range-scans the index (~2ms) and needs no
/// wildcard escaping.
pub(crate) fn subtree_range(root: &str) -> (String, String) {
    let base = root.trim_end_matches('/');
    (format!("{base}/"), format!("{base}0"))
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
    fn wildcard_named_dirs_match_literally() {
        let db = Db::open_in_memory().unwrap();
        // A directory whose name contains a LIKE wildcard — the subtree range
        // must treat it literally (a naive LIKE would let '_' match any char).
        seed(&db, "/a_b/1.jpg", 1, 1, None);
        seed(&db, "/axb/2.jpg", 1, 1, None);

        let q = Query {
            under: Some("/a_b".into()),
            ..Default::default()
        };
        // Only the literal "/a_b" subtree, not "/axb".
        assert_eq!(paths(db.query(&q).unwrap()), ["/a_b/1.jpg"]);
    }

    #[test]
    fn filter_by_tags_all_and_any() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", 1, 1, None);
        seed(&db, "/b.jpg", 1, 1, None);
        seed(&db, "/c.jpg", 1, 1, None);
        // seed sets content_hash = "h{path}".
        db.apply_tag("beach", &["h/a.jpg".into(), "h/b.jpg".into()])
            .unwrap();
        db.apply_tag("sunset", &["h/a.jpg".into(), "h/c.jpg".into()])
            .unwrap();

        // tags_all: must have BOTH → only /a.jpg.
        let q = Query {
            tags_all: vec!["beach".into(), "SUNSET".into()], // case-insensitive
            ..Default::default()
        };
        assert_eq!(paths(db.query(&q).unwrap()), ["/a.jpg"]);

        // tags_any: at least one → all three.
        let q = Query {
            tags_any: vec!["beach".into(), "sunset".into()],
            ..Default::default()
        };
        assert_eq!(paths(db.query(&q).unwrap()), ["/a.jpg", "/b.jpg", "/c.jpg"]);
    }

    #[test]
    fn filter_by_rating_min() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", 1, 1, None);
        seed(&db, "/b.jpg", 1, 1, None);
        db.set_rating("h/a.jpg", 5).unwrap();
        db.set_rating("h/b.jpg", 3).unwrap();

        let q = Query {
            rating_min: Some(4),
            ..Default::default()
        };
        assert_eq!(paths(db.query(&q).unwrap()), ["/a.jpg"]);
    }

    #[test]
    fn tag_name_injection_is_a_harmless_literal() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", 1, 1, None);
        db.apply_tag("safe", &["h/a.jpg".into()]).unwrap();

        // A malicious "tag name" is bound as a parameter — a literal that matches
        // nothing; no injected SQL runs.
        let q = Query {
            tags_any: vec!["x'); DROP TABLE files;--".into()],
            ..Default::default()
        };
        assert!(db.query(&q).unwrap().is_empty());
        // files still intact.
        assert_eq!(paths(db.query(&Query::default()).unwrap()), ["/a.jpg"]);
    }

    #[test]
    fn query_serde_round_trips_partial_json() {
        // A stored smart-collection predicate may hold only the fields it constrains.
        let q: Query = serde_json::from_str(r#"{"tags_all":["fave"],"rating_min":4}"#).unwrap();
        assert_eq!(q.tags_all, vec!["fave".to_string()]);
        assert_eq!(q.rating_min, Some(4));
        assert_eq!(q.sort, SortKey::Name); // defaulted
        assert!(q.tags_any.is_empty());
    }

    #[test]
    fn tag_filters_do_not_scan_the_whole_library() {
        // A correlated subquery here makes `files` the outer loop, so filtering
        // by tag scans every row in the library to answer a question about a
        // handful of tagged files — ~100ms on a 193k-file index, on the UI
        // thread. Assert the planner drives from the tag side instead.
        let db = Db::open_in_memory().unwrap();
        for q in [
            Query {
                tags_any: vec!["trip".into()],
                ..Default::default()
            },
            Query {
                tags_all: vec!["trip".into()],
                ..Default::default()
            },
        ] {
            let (sql, params) = q.build();
            let conn = db.conn();
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(params), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN files")),
                "tag filter must not scan the library, got: {plan:#?}"
            );
        }
    }
}
