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
    pub fn near_duplicates(&self, max_distance: u32) -> rusqlite::Result<Vec<DuplicateCluster>> {
        let files = self.query(&Query::default())?;
        let n = files.len();
        let mut dsu = Dsu::new(n);
        for i in 0..n {
            for j in (i + 1)..n {
                if should_union(&files[i], &files[j], max_distance) {
                    dsu.union(i, j);
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

fn should_union(a: &FileRecord, b: &FileRecord, max_distance: u32) -> bool {
    if a.content_hash == b.content_hash {
        return true; // byte-identical
    }
    match (a.phash, b.phash) {
        (Some(x), Some(y)) => phash_distance(x, y) <= max_distance,
        _ => false,
    }
}

/// Turn groups into clusters: drop singletons, sort each group largest-first and
/// the clusters by descending size (biggest waste first).
fn into_clusters(groups: impl Iterator<Item = Vec<FileRecord>>) -> Vec<DuplicateCluster> {
    let mut clusters: Vec<DuplicateCluster> = groups
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
}
