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
use crate::exclusions::IndexExclusions;
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
    /// Path known but size/mtime differ — re-decode + re-hash.
    Modified,
    /// Path known as missing, back with the same size and mtime (a replugged
    /// drive) — clear `missing`, keep hash and enrichment: `(size, mtime)` is
    /// trusted here exactly as it is for [`Change::Unchanged`].
    Reappeared,
}

/// True if `path` has an indexable image extension.
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Everything one walk of a library root learned: the image files, and where
/// the tree it saw may not be the whole story (code review #6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Walk {
    pub files: Vec<ScannedFile>,
    /// Directories on a different device from their parent: mount points,
    /// including the root itself when it is one.
    pub mounts: Vec<PathBuf>,
    /// Directories the walk could not list (permissions, I/O errors).
    pub unreadable: Vec<PathBuf>,
}

/// Recursively list image files under `root` (following the tree, skipping
/// unreadable entries). Order is filesystem-dependent. Excluded paths (caches,
/// V-31) are pruned as whole subtrees, so a `~/.cache` under the root costs one
/// check, not a walk of every thumbnail in it.
pub fn walk_images(root: &Path, exclusions: &IndexExclusions) -> Vec<ScannedFile> {
    walk_library(root, exclusions).files
}

/// [`walk_images`], also noting the mount points it crossed and the
/// directories it could not read — what [`Db::reconcile_walk`] needs to tell
/// "deleted" from "not here right now". One extra `stat` per directory.
pub fn walk_library(root: &Path, exclusions: &IndexExclusions) -> Walk {
    use std::os::unix::fs::MetadataExt;

    let mut walk = Walk::default();
    if is_mount_point(root) {
        walk.mounts.push(root.to_path_buf());
    }
    // Device of each directory on the current path, indexed by depth.
    let mut devs: Vec<Option<u64>> = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !exclusions.excludes(e.path()))
    {
        let e = match entry {
            Ok(e) => e,
            Err(err) => {
                if let Some(path) = err.path() {
                    walk.unreadable.push(path.to_path_buf());
                }
                continue;
            }
        };
        if e.file_type().is_dir() {
            let dev = e.metadata().ok().map(|m| m.dev());
            devs.truncate(e.depth());
            let parent = e.depth().checked_sub(1).and_then(|d| devs.get(d).copied());
            if let (Some(Some(parent)), Some(dev)) = (parent, dev) {
                if parent != dev {
                    walk.mounts.push(e.path().to_path_buf());
                }
            }
            devs.push(dev);
            continue;
        }
        if !(e.file_type().is_file() && is_image_path(e.path())) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let Some(mtime) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        else {
            continue;
        };
        walk.files.push(ScannedFile {
            path: e.into_path(),
            size: meta.len() as i64,
            mtime: mtime.as_secs() as i64,
        });
    }
    walk
}

/// Whether `dir` sits on a different device from its parent.
fn is_mount_point(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = dir.parent() else {
        return false;
    };
    match (std::fs::metadata(dir), std::fs::metadata(parent)) {
        (Ok(d), Ok(p)) => d.dev() != p.dev(),
        _ => false,
    }
}

/// Whether `dir` looks like nothing is there right now: gone, unreadable, or
/// an empty directory — what a mount point is when its volume is unplugged.
fn looks_absent(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// The outcome of [`Db::reconcile_walk`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconcile {
    /// Rows marked missing: verified gone.
    pub flagged: usize,
    /// Unseen rows left alone because the part of the tree they live in was
    /// not there to look at (offline is not deleted).
    pub held_back: usize,
    /// The root itself could not be read (or is a known mount point that is
    /// not mounted); everything under it was held back.
    pub root_unavailable: bool,
}

/// Classify a scanned file against its existing index row (if any).
pub fn classify(existing: Option<&FileRecord>, size: i64, mtime: i64) -> Change {
    match existing {
        None => Change::New,
        Some(r) if r.size == size && r.mtime == mtime => {
            if r.missing {
                Change::Reappeared
            } else {
                Change::Unchanged
            }
        }
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
        self.reconcile_deleted_excluding(root, seen, &IndexExclusions::none())
    }

    /// [`Db::reconcile_deleted`], leaving rows under excluded paths alone: the
    /// walk never looked there, so not seeing them proves nothing.
    pub fn reconcile_deleted_excluding(
        &self,
        root: &str,
        seen: &HashSet<String>,
        exclusions: &IndexExclusions,
    ) -> rusqlite::Result<usize> {
        let mut flagged = 0;
        for path in self.paths_under(root)? {
            if !seen.contains(&path) && !exclusions.excludes(Path::new(&path)) {
                self.mark_missing(&path)?;
                flagged += 1;
            }
        }
        Ok(flagged)
    }

    /// The "deleted" arm of a rescan, aware that a tree can be partly offline
    /// (code review #6). Marks missing every unseen row under `root`, except
    /// where the walk's view of the tree can't be trusted:
    ///
    /// - **The root is unavailable** — unreadable, or a known mount point that
    ///   now looks absent. Everything is held.
    /// - **An unreadable directory** — its subtree is held.
    /// - **A known mount point that now looks absent** (gone, unreadable or
    ///   empty — an unmounted volume leaves an empty directory behind). Mount
    ///   points are remembered in `mount_points` from every walk that crossed
    ///   one, so this holds however the directory looks today.
    /// - **A foreign mount** — a mount the walk did cross, under which rows
    ///   are indexed but *none* of them was seen: a different disk on the same
    ///   mount point, not the one whose files the index knows. (This applies
    ///   to a root that is itself a mount point, too.)
    ///
    /// A mount point is recorded the first time any walk crosses it (before
    /// the decisions above are made), so a drive mounted during any scan is
    /// protected from then on. An empty plain directory that was never seen as
    /// a mount point is just an emptied folder: its unseen rows are marked
    /// missing, so moves out of it still relink. A drive never seen mounted
    /// can therefore be marked missing once; [`Change::Reappeared`] makes that
    /// cheap to undo (no re-hash, enrichment kept).
    ///
    /// Rows under excluded (cache) paths are left alone as well: the walk
    /// skipped them, so their absence from it means nothing.
    pub fn reconcile_walk(
        &self,
        root: &Path,
        walk: &Walk,
        exclusions: &IndexExclusions,
    ) -> rusqlite::Result<Reconcile> {
        let now = crate::db::now_secs();
        for mount in &walk.mounts {
            self.conn().execute(
                "INSERT INTO mount_points(path, last_seen) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET last_seen = excluded.last_seen",
                rusqlite::params![mount.to_string_lossy(), now],
            )?;
        }
        let known: Vec<PathBuf> = {
            let mut stmt = self.conn().prepare("SELECT path FROM mount_points")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|r| r.map(PathBuf::from))
                .collect::<rusqlite::Result<_>>()?
        };

        let seen: HashSet<&Path> = walk.files.iter().map(|f| f.path.as_path()).collect();
        let indexed = self.paths_under(&root.to_string_lossy())?;
        let unseen: Vec<&String> = indexed
            .iter()
            .filter(|p| !seen.contains(Path::new(p.as_str())))
            .filter(|p| !exclusions.excludes(Path::new(p.as_str())))
            .collect();
        if unseen.is_empty() {
            return Ok(Reconcile::default());
        }

        let root_unavailable = std::fs::read_dir(root).is_err()
            || known.iter().any(|m| root.starts_with(m) && looks_absent(m));
        if root_unavailable {
            return Ok(Reconcile {
                flagged: 0,
                held_back: unseen.len(),
                root_unavailable: true,
            });
        }

        // Subtrees whose unseen rows prove nothing.
        let mut held: Vec<&Path> = walk.unreadable.iter().map(PathBuf::as_path).collect();
        for m in &known {
            if m.starts_with(root) && m.as_path() != root && looks_absent(m) {
                held.push(m);
            }
        }
        for m in &walk.mounts {
            let under: Vec<&String> = indexed
                .iter()
                .filter(|p| Path::new(p.as_str()).starts_with(m))
                .collect();
            if !under.is_empty() && !under.iter().any(|p| seen.contains(Path::new(p.as_str()))) {
                held.push(m);
            }
        }

        let mut report = Reconcile::default();
        for path in unseen {
            if held.iter().any(|h| Path::new(path.as_str()).starts_with(h)) {
                report.held_back += 1;
            } else {
                self.mark_missing(path)?;
                report.flagged += 1;
            }
        }
        Ok(report)
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

    /// A fresh library root under the temp dir holding `a.jpg`.
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vitrine-scan-{tag}-{}-{}",
            std::process::id(),
            crate::db::now_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        dir
    }

    /// Index `rel` (relative to `root`) as a present row.
    fn index(db: &Db, root: &Path, rel: &str) -> String {
        let path = root.join(rel).to_string_lossy().into_owned();
        db.upsert_file(&rec(&path, rel)).unwrap();
        path
    }

    fn is_missing(db: &Db, path: &str) -> bool {
        db.file_by_path(path).unwrap().unwrap().missing
    }

    /// #6: a mount point inside a readable root is recorded the first time a
    /// walk crosses it; unmounted later, it walks as an empty directory and
    /// its rows are held. An emptied plain folder beside it — never seen as a
    /// mount — reconciles like any deletion, as do deleted files and folders.
    #[test]
    fn an_unmounted_inner_mount_point_is_held_an_emptied_folder_is_not() {
        let root = temp_root("inner-mount");
        let drive = root.join("drive");
        std::fs::create_dir_all(drive.join("2024")).unwrap();
        std::fs::write(drive.join("x.jpg"), b"x").unwrap();
        std::fs::write(drive.join("2024/y.jpg"), b"y").unwrap();
        std::fs::create_dir(root.join("emptied")).unwrap();
        let db = Db::open_in_memory().unwrap();
        let a = index(&db, &root, "a.jpg");
        let on_drive = index(&db, &root, "drive/x.jpg");
        let deep = index(&db, &root, "drive/2024/y.jpg");
        let emptied = index(&db, &root, "emptied/e.jpg");
        let deleted = index(&db, &root, "b.jpg");
        let deleted_folder = index(&db, &root, "old/c.jpg");

        // First 0.4 scan with the drive mounted: the walk crosses the device
        // change (simulated — tests can't mount) and records it.
        let mut walk = walk_library(&root, &IndexExclusions::none());
        walk.mounts.push(drive.clone());
        db.reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        assert!(!is_missing(&db, &on_drive) && !is_missing(&db, &deep));

        // Unplugged: the mount point is left behind, empty.
        std::fs::remove_dir_all(&drive).unwrap();
        std::fs::create_dir(&drive).unwrap();
        let walk = walk_library(&root, &IndexExclusions::none());
        assert!(walk.mounts.is_empty());
        let got = db
            .reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();

        assert_eq!(got.held_back, 2);
        assert!(!got.root_unavailable);
        assert!(!is_missing(&db, &a));
        assert!(!is_missing(&db, &on_drive), "offline is not deleted");
        assert!(!is_missing(&db, &deep));
        assert!(is_missing(&db, &emptied), "an emptied folder reconciles");
        assert!(is_missing(&db, &deleted), "a real deletion still records");
        assert!(is_missing(&db, &deleted_folder), "so does a deleted folder");
        std::fs::remove_dir_all(&root).ok();
    }

    /// #6: a mount point the index has seen before is held whenever it looks
    /// absent — even when the unmount removed the directory itself, which an
    /// emptiness check alone would miss.
    #[test]
    fn a_known_mount_point_that_is_gone_holds_its_subtree() {
        let root = temp_root("known-mount");
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::write(root.join("other/keep.txt"), b"not empty").unwrap();
        let db = Db::open_in_memory().unwrap();
        let drive = root.join("other/usb");
        let on_drive = index(&db, &root, "other/usb/x.jpg");
        let mut walk = walk_library(&root, &IndexExclusions::none());
        // An earlier walk crossed it while it was mounted.
        walk.mounts.push(drive.clone());
        db.reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        assert!(!is_missing(&db, &on_drive));

        // Unplugged: the directory is gone and the parent is not empty.
        let walk = walk_library(&root, &IndexExclusions::none());
        assert!(walk.mounts.is_empty());
        let got = db
            .reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        assert_eq!(got.held_back, 1);
        assert!(!is_missing(&db, &on_drive), "offline is not deleted");

        // Back, and the file really was deleted from it: now it reconciles.
        std::fs::create_dir_all(&drive).unwrap();
        std::fs::write(drive.join("other.jpg"), b"y").unwrap();
        let walk = walk_library(&root, &IndexExclusions::none());
        let got = db
            .reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        assert_eq!(got.flagged, 1);
        assert!(is_missing(&db, &on_drive));
        std::fs::remove_dir_all(&root).ok();
    }

    /// #6: a mount the walk did cross, under which none of the indexed rows
    /// was seen, is a different disk on the same mount point — hold. Once any
    /// indexed file under it is seen, it is the known disk and reconciles.
    #[test]
    fn a_foreign_disk_on_a_mount_point_is_held() {
        let root = temp_root("foreign");
        let usb = root.join("usb");
        std::fs::create_dir(&usb).unwrap();
        std::fs::write(usb.join("stick-b.jpg"), b"b").unwrap();
        let db = Db::open_in_memory().unwrap();
        let stick_a = index(&db, &root, "usb/stick-a.jpg");
        let stick_a2 = index(&db, &root, "usb/stick-a2.jpg");

        let mut walk = walk_library(&root, &IndexExclusions::none());
        walk.mounts.push(usb.clone()); // the walk saw a device change here
        let got = db
            .reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        assert_eq!(got.held_back, 2);
        assert!(!is_missing(&db, &stick_a) && !is_missing(&db, &stick_a2));

        // Stick A is back, minus one file: the other is a real deletion.
        std::fs::remove_file(usb.join("stick-b.jpg")).unwrap();
        std::fs::write(usb.join("stick-a.jpg"), b"a").unwrap();
        let mut walk = walk_library(&root, &IndexExclusions::none());
        walk.mounts.push(usb.clone());
        let got = db
            .reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        assert_eq!(got.flagged, 1);
        assert!(!is_missing(&db, &stick_a));
        assert!(is_missing(&db, &stick_a2));
        std::fs::remove_dir_all(&root).ok();
    }

    /// #6: a subfolder the walk can't read holds its subtree.
    #[test]
    fn an_unreadable_subfolder_is_not_a_deletion() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("unreadable");
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("x.jpg"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: permissions don't bite, nothing to test.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        let db = Db::open_in_memory().unwrap();
        let inside = index(&db, &root, "locked/x.jpg");

        let walk = walk_library(&root, &IndexExclusions::none());
        let readable = std::fs::Permissions::from_mode(0o755);
        assert_eq!(walk.unreadable, vec![locked.clone()]);
        let got = db
            .reconcile_walk(&root, &walk, &IndexExclusions::none())
            .unwrap();
        std::fs::set_permissions(&locked, readable).unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(got.held_back, 1);
        assert!(!is_missing(&db, &inside));
    }

    /// The root itself: unreadable, or a known mount point now empty — hold
    /// everything. A root the user emptied (not a known mount) still
    /// reconciles, as before.
    #[test]
    fn an_unavailable_root_holds_everything_an_emptied_one_does_not() {
        let root = temp_root("root");
        let db = Db::open_in_memory().unwrap();
        let x = index(&db, &root, "x.jpg");

        let gone = root.join("nonexistent");
        let y = index(&db, &gone, "y.jpg");
        let walk = walk_library(&gone, &IndexExclusions::none());
        let got = db
            .reconcile_walk(&gone, &walk, &IndexExclusions::none())
            .unwrap();
        assert!(got.root_unavailable);
        assert!(!is_missing(&db, &y));

        // Emptied by the user (a.jpg removed): x.jpg is a real deletion.
        std::fs::remove_file(root.join("a.jpg")).unwrap();
        let empty = Walk::default();
        let got = db
            .reconcile_walk(&root, &empty, &IndexExclusions::none())
            .unwrap();
        assert!(!got.root_unavailable);
        assert!(is_missing(&db, &x));

        // The same empty root, once seen as a mount point: held.
        db.clear_missing(&x).unwrap();
        db.conn()
            .execute(
                "INSERT INTO mount_points(path, last_seen) VALUES (?1, 1)",
                [root.to_string_lossy()],
            )
            .unwrap();
        let got = db
            .reconcile_walk(&root, &empty, &IndexExclusions::none())
            .unwrap();
        assert!(got.root_unavailable);
        assert!(!is_missing(&db, &x));
        std::fs::remove_dir_all(&root).ok();
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
        assert_eq!(classify(Some(&missing), 100, 42), Change::Reappeared); // back as it was
        assert_eq!(classify(Some(&missing), 100, 99), Change::Modified); // back, but changed
    }

    /// #5: clearing `missing` on a reappeared row keeps everything else.
    #[test]
    fn reappeared_row_keeps_hash_and_enrichment() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec("/drive/a.jpg", "HASH")).unwrap();
        db.set_enrichment(
            "/drive/a.jpg",
            &crate::files::Enrichment {
                width: 10,
                height: 20,
                camera: Some("CAM".into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.reconcile_deleted("/drive", &HashSet::new()).unwrap();
        let gone = db.file_by_path("/drive/a.jpg").unwrap().unwrap();
        assert!(gone.missing);

        assert_eq!(classify(Some(&gone), 100, 42), Change::Reappeared);
        assert!(db.clear_missing("/drive/a.jpg").unwrap());
        let back = db.file_by_path("/drive/a.jpg").unwrap().unwrap();
        assert_eq!(
            back,
            FileRecord {
                missing: false,
                ..gone
            }
        );
        assert!(db.paths_needing_enrichment(10).unwrap().is_empty());
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
        let mut found: Vec<_> = walk_images(&dir, &IndexExclusions::none())
            .into_iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, vec!["a.jpg", "b.png"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// V-31: a cache dir under the root is skipped whole, and its rows are
    /// not marked missing just because the walk didn't look there.
    #[test]
    fn walk_and_reconcile_skip_excluded_dirs() {
        let dir = std::env::temp_dir().join(format!("vitrine-scan-excl-{}", std::process::id()));
        let cache = dir.join(".cache/thumbnails/large");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        let cached = cache.join("d40775e596682f2a16d1b834c221c0a2.png");
        std::fs::write(&cached, b"y").unwrap();
        let excl = IndexExclusions::new(&dir, [dir.join(".cache")]);

        let found = walk_images(&dir, &excl);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("a.jpg"));

        let db = Db::open_in_memory().unwrap();
        db.upsert_file(&rec(&cached.to_string_lossy(), "h"))
            .unwrap();
        let seen: HashSet<String> = found
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect();
        let root = dir.to_string_lossy();
        assert_eq!(
            db.reconcile_deleted_excluding(&root, &seen, &excl).unwrap(),
            0
        );
        let row = db.file_by_path(&cached.to_string_lossy()).unwrap().unwrap();
        assert!(!row.missing);
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
