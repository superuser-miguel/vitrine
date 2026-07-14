//! Disk-cache eviction policy (pure logic).
//!
//! Vitrine's app-private thumbnail cache (the higher-resolution buckets GNOME
//! never generates) would otherwise grow without bound. Given the size and
//! last-use time of each cached file plus a byte budget, this decides which
//! files to delete — least-recently-used first — to get back under budget. The
//! app supplies the filesystem facts and performs the deletions; the decision is
//! here so it is UI-free and testable.

/// Indices of entries to evict so the remaining total is within `cap_bytes`.
///
/// `entries` is `(size_bytes, last_used)` per cached file (`last_used` in any
/// monotonic unit — e.g. unix seconds; smaller = older). Evicts oldest first and
/// stops as soon as the total fits; returns empty if already within budget.
pub fn evict_lru(entries: &[(u64, i64)], cap_bytes: u64) -> Vec<usize> {
    let mut total: u64 = entries.iter().map(|(size, _)| size).sum();
    if total <= cap_bytes {
        return Vec::new();
    }

    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| entries[i].1); // oldest last_used first

    let mut evict = Vec::new();
    for &i in &order {
        if total <= cap_bytes {
            break;
        }
        total -= entries[i].0;
        evict.push(i);
    }
    evict
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_evict_when_within_cap() {
        assert!(evict_lru(&[(10, 1), (10, 2)], 100).is_empty());
        assert!(evict_lru(&[], 0).is_empty());
    }

    #[test]
    fn evicts_oldest_first_until_within_cap() {
        // sizes 30 each, cap 60 → must drop one; the oldest (last_used = 1).
        let entries = [(30, 3), (30, 1), (30, 2)];
        let evicted = evict_lru(&entries, 60);
        assert_eq!(evicted, vec![1]);
    }

    #[test]
    fn evicts_multiple_oldest() {
        // total 100, cap 40 → drop the two oldest (10@1, 20@2) leaves 70 (>40),
        // keep going: drop 30@3 leaves 40 (==cap) → stop. Evicts [.. by age].
        let entries = [(40, 4), (30, 3), (20, 2), (10, 1)];
        let evicted = evict_lru(&entries, 40);
        // oldest→newest: idx3(10,1), idx2(20,2), idx1(30,3), idx0(40,4)
        // total 100 → -10=90 → -20=70 → -30=40 (stop). Evicts 3,2,1.
        assert_eq!(evicted, vec![3, 2, 1]);
    }
}
