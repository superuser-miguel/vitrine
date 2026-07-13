//! A size-bounded LRU cache.
//!
//! Hosted in the engine (not the app) so the eviction policy is unit-testable
//! without GTK (PLAN Phase 1). The app instantiates it as
//! `SizedLru<TextureKey, gdk::Texture>` for the viewer's decoded-texture cache
//! (keyed by path + mip level, cost = width·height·4 bytes, ~256 MB default),
//! but nothing here knows about GTK — values are opaque, cost is caller-supplied.
//!
//! Recency is tracked with a monotonic sequence number and a `BTreeMap` from
//! sequence to key, so selecting the least-recently-used victim is O(log n).
//! A single entry larger than the whole capacity is retained (so the viewer can
//! always show the current image); it is evicted as soon as anything else is
//! inserted.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

struct Entry<V> {
    value: V,
    cost: u64,
    seq: u64,
}

/// An LRU cache bounded by the summed byte-cost of its entries.
pub struct SizedLru<K: Eq + Hash + Clone, V> {
    capacity_bytes: u64,
    used_bytes: u64,
    next_seq: u64,
    entries: HashMap<K, Entry<V>>,
    /// seq -> key, ascending: the first entry is the least-recently-used.
    order: BTreeMap<u64, K>,
}

impl<K: Eq + Hash + Clone, V> SizedLru<K, V> {
    /// Create a cache holding at most `capacity_bytes` of summed entry cost.
    pub fn new(capacity_bytes: u64) -> Self {
        Self {
            capacity_bytes,
            used_bytes: 0,
            next_seq: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    /// Bytes currently held.
    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    /// Configured capacity.
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True if `key` is present (without touching recency).
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Fetch a value, marking it most-recently-used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let seq = {
            let entry = self.entries.get_mut(key)?;
            self.order.remove(&entry.seq);
            entry.seq = self.next_seq;
            self.next_seq += 1;
            entry.seq
        };
        self.order.insert(seq, key.clone());
        Some(&self.entries.get(key).unwrap().value)
    }

    /// Insert or replace `key`, then evict LRU entries until within capacity.
    ///
    /// The just-inserted entry is never the eviction victim, so an item larger
    /// than the whole capacity is kept until the next insertion.
    pub fn put(&mut self, key: K, value: V, cost: u64) {
        if let Some(old) = self.entries.remove(&key) {
            self.used_bytes -= old.cost;
            self.order.remove(&old.seq);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.used_bytes += cost;
        self.entries.insert(key.clone(), Entry { value, cost, seq });
        self.order.insert(seq, key);
        self.evict_to_fit();
    }

    /// Remove an entry, returning its value if present.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.remove(key)?;
        self.used_bytes -= entry.cost;
        self.order.remove(&entry.seq);
        Some(entry.value)
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.used_bytes = 0;
    }

    /// Change the capacity, evicting immediately if it shrank.
    pub fn set_capacity(&mut self, capacity_bytes: u64) {
        self.capacity_bytes = capacity_bytes;
        self.evict_to_fit();
    }

    /// Evict least-recently-used entries until at capacity, always keeping at
    /// least the single most-recently-used entry.
    fn evict_to_fit(&mut self) {
        while self.used_bytes > self.capacity_bytes && self.entries.len() > 1 {
            let Some((&seq, victim)) = self.order.iter().next() else {
                break;
            };
            let victim = victim.clone();
            self.order.remove(&seq);
            if let Some(entry) = self.entries.remove(&victim) {
                self.used_bytes -= entry.cost;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get() {
        let mut c: SizedLru<&str, i32> = SizedLru::new(100);
        c.put("a", 1, 10);
        c.put("b", 2, 10);
        assert_eq!(c.get(&"a"), Some(&1));
        assert_eq!(c.get(&"b"), Some(&2));
        assert_eq!(c.get(&"missing"), None);
        assert_eq!(c.used_bytes(), 20);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut c: SizedLru<&str, i32> = SizedLru::new(30);
        c.put("a", 1, 10);
        c.put("b", 2, 10);
        c.put("c", 3, 10); // full: a,b,c
        c.put("d", 4, 10); // over by 10 -> evict LRU ("a")
        assert!(!c.contains(&"a"));
        assert!(c.contains(&"b") && c.contains(&"c") && c.contains(&"d"));
        assert_eq!(c.used_bytes(), 30);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c: SizedLru<&str, i32> = SizedLru::new(30);
        c.put("a", 1, 10);
        c.put("b", 2, 10);
        c.put("c", 3, 10);
        assert_eq!(c.get(&"a"), Some(&1)); // "a" now most-recent; "b" is LRU
        c.put("d", 4, 10); // evicts "b", not "a"
        assert!(c.contains(&"a"));
        assert!(!c.contains(&"b"));
    }

    #[test]
    fn replacing_key_updates_cost() {
        let mut c: SizedLru<&str, i32> = SizedLru::new(100);
        c.put("a", 1, 10);
        c.put("a", 9, 40);
        assert_eq!(c.get(&"a"), Some(&9));
        assert_eq!(c.used_bytes(), 40);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn oversize_entry_is_retained_until_next_insert() {
        let mut c: SizedLru<&str, i32> = SizedLru::new(100);
        c.put("huge", 1, 500); // bigger than capacity, but it's the only entry
        assert!(c.contains(&"huge"));
        assert_eq!(c.used_bytes(), 500);
        c.put("small", 2, 10); // now "huge" can be evicted
        assert!(!c.contains(&"huge"));
        assert!(c.contains(&"small"));
        assert_eq!(c.used_bytes(), 10);
    }

    #[test]
    fn set_capacity_shrinks_and_evicts() {
        let mut c: SizedLru<i32, i32> = SizedLru::new(100);
        for i in 0..10 {
            c.put(i, i, 10);
        }
        assert_eq!(c.used_bytes(), 100);
        c.set_capacity(30);
        assert_eq!(c.used_bytes(), 30);
        // The three most-recently-inserted survive.
        assert!(c.contains(&7) && c.contains(&8) && c.contains(&9));
        assert!(!c.contains(&0));
    }

    #[test]
    fn remove_and_clear() {
        let mut c: SizedLru<&str, i32> = SizedLru::new(100);
        c.put("a", 1, 10);
        c.put("b", 2, 10);
        assert_eq!(c.remove(&"a"), Some(1));
        assert_eq!(c.used_bytes(), 10);
        assert!(!c.contains(&"a"));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.used_bytes(), 0);
    }
}
