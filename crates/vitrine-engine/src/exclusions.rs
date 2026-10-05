//! Paths the index must never hold: thumbnail and app caches (V-31).
//!
//! A cache is not a library. On 2026-07-21 a scan reached `~/.cache/thumbnails`
//! and the app's own `~/.var/app/<id>/cache/thumbnails` and indexed 148,962
//! cache PNGs as images. The rule lives here so the scanner walk, the folder
//! open, and the one-time purge of those rows all agree on it. The app supplies
//! the real directories (home, the cache dirs); everything below is pure path
//! logic plus SQL, and testable without them.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::db::Db;

/// Freedesktop size buckets, plus the spec's `fail/` dir.
const THUMB_BUCKETS: &[&str] = &["normal", "large", "x-large", "xx-large", "fail"];

/// The index's exclusion rule. Built once by the app from the real dirs.
#[derive(Debug, Clone, Default)]
pub struct IndexExclusions {
    /// Cache roots: everything under them is excluded.
    roots: Vec<PathBuf>,
    /// `<home>/.var/app` — every Flatpak app's `cache/` beneath it is excluded.
    flatpak_apps: Option<PathBuf>,
}

impl IndexExclusions {
    /// Exclude everything under `cache_dirs` (e.g. `~/.cache` and the app's
    /// `$XDG_CACHE_HOME`), every `<home>/.var/app/*/cache`, and any file laid
    /// out like a freedesktop thumbnail wherever it is.
    pub fn new(home: &Path, cache_dirs: impl IntoIterator<Item = PathBuf>) -> IndexExclusions {
        IndexExclusions {
            roots: cache_dirs.into_iter().filter(|d| d.is_absolute()).collect(),
            flatpak_apps: Some(home.join(".var/app")),
        }
    }

    /// An exclusion that excludes nothing (tests, and callers with no rule).
    pub fn none() -> IndexExclusions {
        IndexExclusions::default()
    }

    /// Whether `path` (a file or a directory) must stay out of the index.
    pub fn excludes(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
            || self.is_flatpak_app_cache(path)
            || is_thumbnail_file(path)
    }

    /// `<home>/.var/app/<app-id>/cache[/…]`.
    fn is_flatpak_app_cache(&self, path: &Path) -> bool {
        let Some(apps) = &self.flatpak_apps else {
            return false;
        };
        let Ok(rest) = path.strip_prefix(apps) else {
            return false;
        };
        let mut parts = rest.components();
        matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), Some(Component::Normal(dir))) if dir == "cache"
        )
    }
}

/// A file laid out like a freedesktop thumbnail, anywhere: `…/thumbnails/
/// <bucket>/<32 hex>.png` (or `.sh_thumbnails`, the spec's travelling cache;
/// `fail/` adds one more level). An MD5-named PNG in a bucket dir is never a
/// photo someone put there, so this holds even outside the known cache roots —
/// a copied `~/.cache` on a backup drive, say.
fn is_thumbnail_file(path: &Path) -> bool {
    let is_md5_png = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".png"))
        .is_some_and(|stem| stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit()));
    if !is_md5_png {
        return false;
    }
    let dirs: Vec<&std::ffi::OsStr> = path
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    dirs.windows(2).any(|w| {
        (w[0] == "thumbnails" || w[0] == ".sh_thumbnails")
            && w[1].to_str().is_some_and(|b| THUMB_BUCKETS.contains(&b))
    })
}

/// What a cache-row purge found and did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachePurge {
    /// `files` rows before the purge.
    pub before: usize,
    /// Rows under an excluded path.
    pub cache_rows: usize,
    /// Cache rows kept because they are the only anchor of an annotation.
    pub kept_annotated: usize,
    /// Rows deleted.
    pub deleted: usize,
    /// `files` rows after the purge.
    pub after: usize,
}

/// The rows a purge would delete — computed first, so the app can take a
/// backup between deciding and deleting.
#[derive(Debug, Clone, Default)]
pub struct CachePurgePlan {
    pub before: usize,
    pub cache_rows: usize,
    pub kept_annotated: usize,
    /// `files.id`s to delete.
    pub delete_ids: Vec<i64>,
}

/// Tables whose rows annotate a `content_hash`.
const ANNOTATION_TABLES: &[&str] = &[
    "file_tags",
    "ratings",
    "comments",
    "collection_items",
    "orientations",
    "crops",
];

impl Db {
    /// Decide which `files` rows under excluded paths can go. A cache row is
    /// kept only when its `content_hash` carries an annotation (tag, rating,
    /// comment, collection membership, orientation, crop) **and** no non-cache
    /// row shares that hash — i.e. when deleting it would orphan the
    /// annotation. Annotation rows themselves are never touched.
    pub fn plan_cache_purge(
        &self,
        exclusions: &IndexExclusions,
    ) -> rusqlite::Result<CachePurgePlan> {
        let mut plan = CachePurgePlan::default();
        // (id, hash) of every cache row; plus every hash a non-cache row holds.
        let mut cache_rows: Vec<(i64, String)> = Vec::new();
        let mut kept_hashes: HashSet<String> = HashSet::new();
        {
            let mut stmt = self
                .conn()
                .prepare("SELECT id, path, content_hash FROM files")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, path, hash) = row?;
                plan.before += 1;
                if exclusions.excludes(Path::new(&path)) {
                    cache_rows.push((id, hash));
                } else {
                    kept_hashes.insert(hash);
                }
            }
        }
        plan.cache_rows = cache_rows.len();

        let annotated = self.annotated_hashes()?;
        for (id, hash) in cache_rows {
            if annotated.contains(&hash) && !kept_hashes.contains(&hash) {
                plan.kept_annotated += 1;
            } else {
                plan.delete_ids.push(id);
            }
        }
        Ok(plan)
    }

    /// Delete the planned rows in one transaction.
    pub fn apply_cache_purge(&self, plan: &CachePurgePlan) -> rusqlite::Result<CachePurge> {
        let tx = self.conn().unchecked_transaction()?;
        let mut deleted = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM files WHERE id = ?1")?;
            for id in &plan.delete_ids {
                deleted += stmt.execute([id])?;
            }
        }
        tx.commit()?;
        let after: i64 = self
            .conn()
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;
        Ok(CachePurge {
            before: plan.before,
            cache_rows: plan.cache_rows,
            kept_annotated: plan.kept_annotated,
            deleted,
            after: after as usize,
        })
    }

    /// Plan and apply in one go (no backup step — tests and tools).
    pub fn purge_cache_rows(&self, exclusions: &IndexExclusions) -> rusqlite::Result<CachePurge> {
        let plan = self.plan_cache_purge(exclusions)?;
        self.apply_cache_purge(&plan)
    }

    /// Every `content_hash` that carries at least one annotation.
    fn annotated_hashes(&self) -> rusqlite::Result<HashSet<String>> {
        let mut out = HashSet::new();
        for table in ANNOTATION_TABLES {
            let mut stmt = self
                .conn()
                .prepare(&format!("SELECT DISTINCT content_hash FROM {table}"))?;
            for hash in stmt.query_map([], |r| r.get::<_, String>(0))? {
                out.insert(hash?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileRecord;

    fn excl() -> IndexExclusions {
        IndexExclusions::new(
            Path::new("/home/me"),
            [
                PathBuf::from("/home/me/.cache"),
                PathBuf::from("/home/me/.var/app/io.github.x.Vitrine/cache"),
            ],
        )
    }

    #[test]
    fn excludes_cache_roots_and_their_contents() {
        let e = excl();
        for p in [
            "/home/me/.cache",
            "/home/me/.cache/thumbnails",
            "/home/me/.cache/thumbnails/large/0123.png",
            "/home/me/.cache/some-app/img.jpg",
            "/home/me/.var/app/io.github.x.Vitrine/cache/thumbnails/x-large/a.png",
            // Any other Flatpak app's cache, too.
            "/home/me/.var/app/org.gnome.Loupe/cache/img.png",
            "/home/me/.var/app/org.gnome.Loupe/cache",
        ] {
            assert!(e.excludes(Path::new(p)), "{p} should be excluded");
        }
    }

    #[test]
    fn keeps_library_paths() {
        let e = excl();
        for p in [
            "/home/me/Pictures/a.jpg",
            "/home/me/Pictures/thumbnails/large/holiday.png", // not MD5-named
            "/home/me/.cache-not/a.jpg",                      // a prefix, not a parent
            "/home/me/.var/app/org.gnome.Loupe/data/img.png", // not cache/
            "/home/me/.var/app/cache/x.png",                  // no app-id level
            "/run/user/1000/doc/abcd/photo.jpg",
        ] {
            assert!(!e.excludes(Path::new(p)), "{p} should be kept");
        }
    }

    #[test]
    fn thumbnail_layout_is_excluded_anywhere() {
        let e = IndexExclusions::none();
        let md5 = "d40775e596682f2a16d1b834c221c0a2";
        for p in [
            format!("/run/media/me/USB/backup/.cache/thumbnails/large/{md5}.png"),
            format!("/mnt/x/.sh_thumbnails/normal/{md5}.png"),
            format!("/x/thumbnails/fail/gnome-thumbnail-factory/{md5}.png"),
        ] {
            assert!(e.excludes(Path::new(&p)), "{p} should be excluded");
        }
        assert!(!e.excludes(Path::new(&format!("/x/photos/{md5}.png"))));
        assert!(!e.excludes(Path::new(&format!("/x/thumbnails/{md5}.png"))));
        assert!(!e.excludes(Path::new("/x/thumbnails/large/not-hex-name.png")));
    }

    #[test]
    fn none_excludes_nothing_ordinary() {
        let e = IndexExclusions::none();
        assert!(!e.excludes(Path::new("/home/me/.cache/a.jpg")));
    }

    fn add(db: &Db, path: &str, hash: &str) {
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

    fn count(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn purge_deletes_unannotated_cache_rows_only() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "/home/me/Pictures/a.jpg", "A");
        add(&db, "/home/me/.cache/thumbnails/large/1.png", "C1");
        add(&db, "/home/me/.cache/thumbnails/large/2.png", "C2");
        add(&db, "/home/me/.cache/thumbnails/large/3.png", "C3");
        add(&db, "/home/me/.cache/thumbnails/large/4.png", "A"); // dup of a.jpg
                                                                 // C2: rated, only anchored by a cache row → keep.
        db.set_rating("C2", 4).unwrap();
        // A: tagged, but a.jpg also holds it → its cache copy can go.
        db.apply_tag("fave", &["A".to_string()]).unwrap();
        // C3: in a catalog, only anchored by a cache row → keep.
        let id = db.create_catalog("cat").unwrap();
        db.add_to_catalog(id, &["C3".to_string()]).unwrap();

        let r = db.purge_cache_rows(&excl()).unwrap();
        assert_eq!(
            r,
            CachePurge {
                before: 5,
                cache_rows: 4,
                kept_annotated: 2,
                deleted: 2,
                after: 3,
            }
        );
        assert!(db
            .file_by_path("/home/me/Pictures/a.jpg")
            .unwrap()
            .is_some());
        assert!(db
            .file_by_path("/home/me/.cache/thumbnails/large/1.png")
            .unwrap()
            .is_none());
        assert!(db
            .file_by_path("/home/me/.cache/thumbnails/large/2.png")
            .unwrap()
            .is_some());
        // Annotations untouched.
        let tags: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM file_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags, 1);
    }

    #[test]
    fn purge_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "/home/me/Pictures/a.jpg", "A");
        add(
            &db,
            "/home/me/.var/app/io.github.x.Vitrine/cache/thumbnails/large/1.png",
            "C1",
        );
        assert_eq!(db.purge_cache_rows(&excl()).unwrap().deleted, 1);
        let again = db.purge_cache_rows(&excl()).unwrap();
        assert_eq!(again.deleted, 0);
        assert_eq!(again.cache_rows, 0);
        assert_eq!(count(&db), 1);
    }

    /// The safety copy the app takes before purging round-trips the rows.
    #[test]
    fn purge_after_backup_keeps_the_rows_in_the_copy() {
        let dir = std::env::temp_dir().join(format!("vitrine-purge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("index.sqlite")).unwrap();
        add(&db, "/home/me/Pictures/a.jpg", "A");
        add(&db, "/home/me/.cache/thumbnails/large/1.png", "C1");
        let plan = db.plan_cache_purge(&excl()).unwrap();
        let backup = dir.join("backup.sqlite");
        db.backup_to(&backup).unwrap();
        db.apply_cache_purge(&plan).unwrap();
        assert_eq!(count(&db), 1);
        assert_eq!(count(&Db::open(&backup).unwrap()), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
