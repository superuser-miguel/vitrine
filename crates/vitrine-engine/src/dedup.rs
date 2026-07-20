//! Duplicate detection (PLAN Phase 4 / §10.1): cluster the indexed files into
//! groups of duplicates.
//!
//! - **Exact** — files sharing a `content_hash` (BLAKE3): byte-identical.
//! - **Near** — files whose perceptual hashes are within a Hamming distance
//!   (visually the same despite re-encode / resize / minor edits). Exact dups are
//!   a subset (identical bytes ⇒ identical pHash ⇒ distance 0).
//!
//! Clustering is union-find over the present files. A pairwise O(n²) pass is
//! fine at personal-collection scale (the `files` table is small); a BK-tree is
//! the noted optimisation if it ever isn't.

use std::collections::HashMap;

use crate::db::Db;
use crate::files::FileRecord;
use crate::hash::phash_distance;
use crate::query::Query;

/// A group of 2+ duplicate files, sorted largest-first so `files[0]` is the
/// natural "keeper".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateCluster {
    pub files: Vec<FileRecord>,
}

impl DuplicateCluster {
    /// The suggested file to keep (largest; ties broken by path).
    pub fn keeper(&self) -> &FileRecord {
        &self.files[0]
    }
}

impl Db {
    /// Exact-duplicate clusters: present files grouped by `content_hash`, only
    /// groups with more than one file.
    pub fn exact_duplicates(&self) -> rusqlite::Result<Vec<DuplicateCluster>> {
        let files = self.query(&Query::default())?;
        let mut by_hash: HashMap<String, Vec<FileRecord>> = HashMap::new();
        for file in files {
            by_hash
                .entry(file.content_hash.clone())
                .or_default()
                .push(file);
        }
        Ok(into_clusters(by_hash.into_values()))
    }

    /// Near-duplicate clusters: union-find over pHash Hamming distance
    /// `<= max_distance` (0 = identical). Files without a pHash only cluster by
    /// exact content hash. Includes exact duplicates.
    ///
    /// A brute-force O(n²) pass is quadratic in the whole library and does not
    /// scale (tens of thousands of files ⇒ billions of comparisons). Instead we
    /// union byte-identical files by content hash, then find near pHashes with a
    /// [`BkTree`], which only visits candidates the triangle inequality allows.
    pub fn near_duplicates(&self, max_distance: u32) -> rusqlite::Result<Vec<DuplicateCluster>> {
        let files = self.query(&Query::default())?;
        let n = files.len();
        let mut dsu = Dsu::new(n);

        // 1. Union byte-identical files (this also clusters pHash-less files).
        let mut by_hash: HashMap<&str, usize> = HashMap::new();
        for (i, file) in files.iter().enumerate() {
            match by_hash.entry(file.content_hash.as_str()) {
                std::collections::hash_map::Entry::Occupied(e) => dsu.union(*e.get(), i),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(i);
                }
            }
        }

        // 2. Union files whose pHashes are within `max_distance`. Bucket file
        //    indices by exact pHash so identical hashes collapse, index the
        //    distinct pHashes in a BK-tree, then for each bucket union it with
        //    every bucket the tree reports as near.
        let mut buckets: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, file) in files.iter().enumerate() {
            if let Some(phash) = file.phash {
                buckets.entry(phash).or_default().push(i);
            }
        }
        let mut tree = BkTree::default();
        for &phash in buckets.keys() {
            tree.insert(phash);
        }
        for (&phash, indices) in &buckets {
            // Collapse the identical-pHash bucket first.
            for pair in indices.windows(2) {
                dsu.union(pair[0], pair[1]);
            }
            // Then union with each near (distinct) pHash bucket.
            for neighbour in tree.within(phash, max_distance) {
                if neighbour != phash {
                    dsu.union(indices[0], buckets[&neighbour][0]);
                }
            }
        }

        let mut groups: HashMap<usize, Vec<FileRecord>> = HashMap::new();
        for (i, file) in files.into_iter().enumerate() {
            groups.entry(dsu.find(i)).or_default().push(file);
        }
        Ok(into_clusters(groups.into_values()))
    }
}

/// A [BK-tree](https://en.wikipedia.org/wiki/BK-tree) over pHash values under the
/// Hamming metric — a metric tree that answers "all values within radius r of q"
/// while visiting only the children the triangle inequality permits.
#[derive(Default)]
struct BkTree {
    root: Option<BkNode>,
}

struct BkNode {
    value: i64,
    /// Children keyed by their exact distance from `value`.
    children: HashMap<u32, BkNode>,
}

impl BkTree {
    /// Insert a distinct value (duplicates are ignored).
    fn insert(&mut self, value: i64) {
        match &mut self.root {
            None => self.root = Some(BkNode::new(value)),
            Some(root) => root.insert(value),
        }
    }

    /// Every stored value within Hamming distance `radius` of `query`.
    fn within(&self, query: i64, radius: u32) -> Vec<i64> {
        let mut out = Vec::new();
        let mut stack: Vec<&BkNode> = self.root.iter().collect();
        while let Some(node) = stack.pop() {
            let d = phash_distance(node.value, query);
            if d <= radius {
                out.push(node.value);
            }
            let (lo, hi) = (d.saturating_sub(radius), d.saturating_add(radius));
            for (&edge, child) in &node.children {
                if (lo..=hi).contains(&edge) {
                    stack.push(child);
                }
            }
        }
        out
    }
}

impl BkNode {
    fn new(value: i64) -> BkNode {
        BkNode {
            value,
            children: HashMap::new(),
        }
    }

    fn insert(&mut self, value: i64) {
        let d = phash_distance(self.value, value);
        if d == 0 {
            return; // already present
        }
        match self.children.entry(d) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().insert(value),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(BkNode::new(value));
            }
        }
    }
}

/// Whether `path` is an XDG document-portal path (`/run/user/<uid>/doc/<id>/…`).
///
/// The portal exposes a host file under an opaque per-document path. A folder
/// opened through the file chooser is therefore indexed under that path, while
/// the *same* file reached through a directly-granted root keeps its real one —
/// so the index holds two rows with one set of bytes behind them.
pub fn is_portal_document_path(path: &str) -> bool {
    let rest = match path.strip_prefix("/run/user/") {
        Some(rest) => rest,
        None => return false,
    };
    // /run/user/<uid>/doc/...
    match rest.split_once('/') {
        Some((uid, tail)) => uid.chars().all(|c| c.is_ascii_digit()) && tail.starts_with("doc/"),
        None => false,
    }
}

/// Drop the portal/real pairings of one file from a cluster.
///
/// A group holding both a document-portal path and a non-portal path for the
/// same content is one file seen through two routes, not two copies. Reporting
/// it as a duplicate is wrong under any definition of "duplicate", and acting on
/// it is worse: trashing the "other copy" targets the same bytes the keeper
/// points at.
///
/// Only the portal rows are dropped, and only when a non-portal row with the
/// **same content hash** is present to stand for the file. Two guards matter
/// here: a near-duplicate cluster mixes differing hashes, so collapsing across
/// the whole cluster could discard a genuinely distinct image that happens to be
/// portal-indexed; and a file reachable *only* through a portal path keeps its
/// row, since nothing else represents it.
fn collapse_portal_aliases(files: Vec<FileRecord>) -> Vec<FileRecord> {
    let mut by_hash: HashMap<String, Vec<FileRecord>> = HashMap::new();
    for file in files {
        by_hash
            .entry(file.content_hash.clone())
            .or_default()
            .push(file);
    }
    let mut kept = Vec::new();
    for (_, mut group) in by_hash {
        if group.iter().any(|f| !is_portal_document_path(&f.path)) {
            group.retain(|f| !is_portal_document_path(&f.path));
        }
        kept.append(&mut group);
    }
    kept
}

/// Turn groups into clusters: collapse portal aliases, drop singletons, sort each
/// group largest-first and the clusters by descending size (biggest waste first).
fn into_clusters(groups: impl Iterator<Item = Vec<FileRecord>>) -> Vec<DuplicateCluster> {
    let mut clusters: Vec<DuplicateCluster> = groups
        .map(collapse_portal_aliases)
        .filter(|files| files.len() > 1)
        .map(|mut files| {
            files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
            DuplicateCluster { files }
        })
        .collect();
    clusters.sort_by(|a, b| {
        b.files
            .len()
            .cmp(&a.files.len())
            .then_with(|| a.keeper().path.cmp(&b.keeper().path))
    });
    clusters
}

/// Disjoint-set union (union by size, path halving).
struct Dsu {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Dsu {
    fn new(n: usize) -> Dsu {
        Dsu {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (mut ra, mut rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.size[ra] < self.size[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        self.size[ra] += self.size[rb];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::Enrichment;

    fn seed(db: &Db, path: &str, hash: &str, size: i64, phash: Option<i64>) {
        db.upsert_file(&FileRecord {
            path: path.into(),
            content_hash: hash.into(),
            size,
            mtime: 1,
            indexed_at: 1,
            ..Default::default()
        })
        .unwrap();
        if let Some(phash) = phash {
            // set_enrichment keys on path.
            db.set_enrichment(
                path,
                &Enrichment {
                    width: 4,
                    height: 4,
                    phash: Some(phash),
                    ..Default::default()
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn exact_groups_by_content_hash() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/a.jpg", "dup", 300, None);
        seed(&db, "/b.jpg", "dup", 100, None);
        seed(&db, "/c.jpg", "unique", 50, None);

        let clusters = db.exact_duplicates().unwrap();
        assert_eq!(clusters.len(), 1);
        // Sorted largest-first → /a.jpg is the keeper.
        let paths: Vec<_> = clusters[0].files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["/a.jpg", "/b.jpg"]);
        assert_eq!(clusters[0].keeper().path, "/a.jpg");
    }

    #[test]
    fn near_clusters_by_phash_and_subsumes_exact() {
        let db = Db::open_in_memory().unwrap();
        // Two visually-similar images (phash distance 1) with different bytes.
        seed(&db, "/x1.jpg", "hx1", 10, Some(0b0000));
        seed(&db, "/x2.jpg", "hx2", 20, Some(0b0001)); // 1 bit apart
                                                       // A far-apart image.
        seed(&db, "/y.jpg", "hy", 30, Some(0b1111_1111));
        // An exact pair (same content hash).
        seed(&db, "/e1.jpg", "he", 5, None);
        seed(&db, "/e2.jpg", "he", 7, None);

        // Threshold 1 → {x1,x2} cluster and {e1,e2} cluster; y stands alone.
        let clusters = db.near_duplicates(1).unwrap();
        assert_eq!(clusters.len(), 2);
        let mut sets: Vec<Vec<String>> = clusters
            .iter()
            .map(|c| {
                let mut p: Vec<String> = c.files.iter().map(|f| f.path.clone()).collect();
                p.sort();
                p
            })
            .collect();
        sets.sort();
        assert_eq!(
            sets,
            vec![
                vec!["/e1.jpg".to_string(), "/e2.jpg".to_string()],
                vec!["/x1.jpg".to_string(), "/x2.jpg".to_string()],
            ]
        );

        // Threshold 0 → the near pair (distance 1) no longer clusters; only the
        // exact pair remains.
        let strict = db.near_duplicates(0).unwrap();
        assert_eq!(strict.len(), 1);
        assert_eq!(strict[0].files.len(), 2);
        assert_eq!(strict[0].keeper().content_hash, "he");
    }

    #[test]
    fn near_clusters_are_transitive() {
        let db = Db::open_in_memory().unwrap();
        // A chain a—b—c where each neighbour is 2 bits apart but a—c is 4 apart:
        // with threshold 2 they must still land in ONE cluster (union-find is
        // transitive even though a and c are never directly compared).
        seed(&db, "/a.jpg", "ha", 30, Some(0b0000_0000));
        seed(&db, "/b.jpg", "hb", 20, Some(0b0000_0011));
        seed(&db, "/c.jpg", "hc", 10, Some(0b0000_1111));
        let clusters = db.near_duplicates(2).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].files.len(), 3);
        assert_eq!(clusters[0].keeper().path, "/a.jpg"); // largest
    }

    #[test]
    fn bktree_within_finds_neighbours() {
        let mut tree = BkTree::default();
        for v in [0b0000, 0b0001, 0b0011, 0b1111_1111] {
            tree.insert(v);
        }
        let mut near = tree.within(0b0000, 2);
        near.sort();
        assert_eq!(near, vec![0b0000, 0b0001, 0b0011]);
        assert_eq!(tree.within(0b0000, 0), vec![0b0000]);
        // The far value is reachable at a large enough radius.
        assert!(tree.within(0b0000, 8).contains(&0b1111_1111));
    }

    #[test]
    fn portal_path_detection() {
        assert!(is_portal_document_path("/run/user/1000/doc/abc/a.jpg"));
        assert!(!is_portal_document_path("/home/u/Pictures/a.jpg"));
        // Near misses that must not be treated as portal paths.
        assert!(!is_portal_document_path("/run/user/1000/other/a.jpg"));
        assert!(!is_portal_document_path("/run/user/notauid/doc/a.jpg"));
        assert!(!is_portal_document_path("/run/user/"));
    }

    #[test]
    fn one_file_seen_through_the_portal_is_not_a_duplicate() {
        // The same bytes indexed under both a directly-granted root and a
        // document-portal path is one file, not two copies. Reporting it as a
        // duplicate is wrong, and acting on it would trash the keeper's bytes.
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/home/u/Pictures/a.jpg", "h1", 10, None);
        seed(&db, "/run/user/1000/doc/xyz/a.jpg", "h1", 10, None);
        assert!(
            db.exact_duplicates().unwrap().is_empty(),
            "portal alias of a real path must not be a duplicate"
        );
    }

    #[test]
    fn genuine_duplicates_still_reported_alongside_portal_aliases() {
        let db = Db::open_in_memory().unwrap();
        // Two real copies of the same bytes — a true duplicate.
        seed(&db, "/home/u/Pictures/a.jpg", "h1", 10, None);
        seed(&db, "/home/u/Pictures/copy/a.jpg", "h1", 10, None);
        // Plus a portal alias, which must drop out without hiding the pair.
        seed(&db, "/run/user/1000/doc/xyz/a.jpg", "h1", 10, None);

        let clusters = db.exact_duplicates().unwrap();
        assert_eq!(clusters.len(), 1);
        let paths: Vec<&str> = clusters[0].files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["/home/u/Pictures/a.jpg", "/home/u/Pictures/copy/a.jpg"]
        );
    }

    #[test]
    fn portal_only_files_keep_their_rows() {
        // Nothing else represents these, so they are still real duplicates.
        let db = Db::open_in_memory().unwrap();
        seed(&db, "/run/user/1000/doc/aaa/a.jpg", "h1", 10, None);
        seed(&db, "/run/user/1000/doc/bbb/a.jpg", "h1", 10, None);
        assert_eq!(db.exact_duplicates().unwrap().len(), 1);
    }
}
