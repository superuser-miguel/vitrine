//! Directory scanning and change detection (PLAN §7 Phase 2, task 2).
//!
//! The scanner walks a library root, classifies each image against the index
//! by cheap `(size, mtime)` comparison, and reconciles moves and deletions.
//! The heavy per-file work (decode, BLAKE3, pHash, EXIF) is orchestrated by the
//! app around this; the pure logic here is what the tests pin down — including
//! the core promise: **a tag survives a file move.**

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::db::Db;
use crate::files::FileRecord;

/// Image file extensions the scanner indexes (lowercase, no dot).
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "avif", "heif", "heic", "jxl", "gif", "tiff", "tif", "bmp", "svg",
];

/// One file found on disk during a walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub size: i64,
    pub mtime: i64,
}

/// How a scanned file compares to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Present and unchanged — skip.
    Unchanged,
    /// Not in the index — decode + hash + insert.
    New,
    /// Path known but size/mtime differ (or was missing) — re-decode + re-hash.
    Modified,
}

/// True if `path` has an indexable image extension.
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Recursively list image files under `root` (following the tree, skipping
/// unreadable entries). Order is filesystem-dependent.
pub fn walk_images(root: &Path) -> Vec<ScannedFile> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_image_path(e.path()))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some(ScannedFile {
                path: e.into_path(),
                size: meta.len() as i64,
                mtime,
            })
        })
        .collect()
}

/// Classify a scanned file against its existing index row (if any).
pub fn classify(existing: Option<&FileRecord>, size: i64, mtime: i64) -> Change {
    match existing {
        None => Change::New,
        Some(r) if !r.missing && r.size == size && r.mtime == mtime => Change::Unchanged,
        Some(_) => Change::Modified,
    }
}

impl Db {
    /// Paths of non-missing files whose path begins with `root` + separator.
    pub fn paths_under(&self, root: &str) -> rusqlite::Result<Vec<String>> {
        let (lo, hi) = crate::query::subtree_range(root);
        let mut stmt = self
            .conn()
            .prepare("SELECT path FROM files WHERE missing = 0 AND path >= ?1 AND path < ?2")?;
        let rows = stmt.query_map([lo, hi], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Mark as missing every non-missing file under `root` not in `seen`.
    /// Returns how many were flagged. This is the "deleted" arm of a rescan.
    pub fn reconcile_deleted(&self, root: &str, seen: &HashSet<String>) -> rusqlite::Result<usize> {
        let mut flagged = 0;
        for path in self.paths_under(root)? {
            if !seen.contains(&path) {
                self.mark_missing(&path)?;
                flagged += 1;
            }
        }
        Ok(flagged)
    }

    /// A `missing` file with this content hash — a candidate move source, so a
    /// newly-seen file with the same bytes can be relinked instead of re-added.
    pub fn missing_file_by_hash(&self, content_hash: &str) -> rusqlite::Result<Option<FileRecord>> {
        self.files_by_hash(content_hash)
            .map(|files| files.into_iter().find(|f| f.missing))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, hash: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            content_hash: hash.into(),
            size: 100,
            mtime: 42,
            indexed_at: 1,
            ..Default::default()
        }
    }

    #[test]
    fn classify_new_unchanged_modified() {
        assert_eq!(classify(None, 100, 42), Change::New);
        let r = rec("/a.jpg", "h");
        assert_eq!(classify(Some(&r), 100, 42), Change::Unchanged);
        assert_eq!(classify(Some(&r), 200, 42), Change::Modified); // size changed
        assert_eq!(classify(Some(&r), 100, 99), Change::Modified); // mtime changed
        let mut missing = r.clone();
        missing.missing = true;
        assert_eq!(classify(Some(&missing), 100, 42), Change::Modified); // was missing
    }

    #[test]
    fn is_image_path_by_extension() {
        assert!(is_image_path(Path::new("/x/a.JPG")));
        assert!(is_image_path(Path::new("/x/a.avif")));
        assert!(!is_image_path(Path::new("/x/a.txt")));
        assert!(!is_image_path(Path::new("/x/noext")));
    }

    #[test]
    fn walk_finds_images_recursively() {
        let dir = std::env::temp_dir().join(format!("vitrine-scan-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        std::fs::write(dir.join("sub/b.png"), b"yy").unwrap();
        std::fs::write(dir.join("note.txt"), b"skip").unwrap();
        let mut found: Vec<_> = walk_images(&dir)
            .into_iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, vec!["a.jpg", "b.png"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reconcile_deleted_flags_unseen() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/root/a.jpg", "h1")).unwrap();
        db.upsert_file(&rec("/root/b.jpg", "h2")).unwrap();
        db.upsert_file(&rec("/other/c.jpg", "h3")).unwrap();
        let seen: HashSet<String> = ["/root/a.jpg".to_string()].into_iter().collect();
        let flagged = db.reconcile_deleted("/root", &seen).unwrap();
        assert_eq!(flagged, 1); // b.jpg gone; a.jpg seen; c.jpg is under /other
        assert!(db.file_by_path("/root/b.jpg").unwrap().unwrap().missing);
        assert!(!db.file_by_path("/root/a.jpg").unwrap().unwrap().missing);
        assert!(!db.file_by_path("/other/c.jpg").unwrap().unwrap().missing);
    }

    /// THE core promise: a tag applied to a file survives the file being moved.
    #[test]
    fn tag_survives_a_move() {
        let db = Db::open_in_memory().unwrap();
        // Index /old.jpg and tag its content.
        db.upsert_file(&rec("/lib/old.jpg", "HASH")).unwrap();
        db.conn()
            .execute("INSERT INTO tags(name) VALUES ('fave')", [])
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO file_tags(content_hash, tag_id, created_at)
                 VALUES ('HASH', 1, 1)",
                [],
            )
            .unwrap();

        // Rescan: /old.jpg is gone → missing; /new.jpg appears with the same bytes.
        let seen: HashSet<String> = ["/lib/new.jpg".to_string()].into_iter().collect();
        db.reconcile_deleted("/lib", &seen).unwrap();
        assert!(db.file_by_path("/lib/old.jpg").unwrap().unwrap().missing);

        // A "new" file hashes to HASH → it's a move → relink instead of insert.
        let moved_from = db.missing_file_by_hash("HASH").unwrap().unwrap();
        assert_eq!(moved_from.path, "/lib/old.jpg");
        db.relink_path(&moved_from.path, "/lib/new.jpg", 77)
            .unwrap();

        // The file lives at the new path, is no longer missing…
        assert!(db.file_by_path("/lib/old.jpg").unwrap().is_none());
        assert!(!db.file_by_path("/lib/new.jpg").unwrap().unwrap().missing);
        // …and the tag (keyed by content_hash) is still attached.
        let tag_count: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM file_tags WHERE content_hash = 'HASH'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_count, 1, "tag must survive the move");
    }
}
