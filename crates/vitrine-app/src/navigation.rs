//! Small, widget-free bookkeeping for the window's navigation lifecycle.
//!
//! Kept out of `window.rs` so the rules can be unit-tested without a display:
//! which async result is still wanted.

use std::cell::Cell;

/// A token identifying the navigation a piece of async work belongs to.
///
/// Every navigation (open a folder, a collection, a tag, close everything)
/// bumps it. Work that finishes later — a folder enumeration on a slow USB
/// drive — checks its token before writing the grid's store, so a result that
/// lost the race is dropped instead of overwriting whatever the user moved to
/// in the meantime.
#[derive(Debug, Default)]
pub struct LoadGeneration(Cell<u64>);

impl LoadGeneration {
    /// Start a new navigation; earlier tokens stop being current.
    pub fn bump(&self) -> u64 {
        let next = self.0.get().wrapping_add(1);
        self.0.set(next);
        next
    }

    /// Whether `token` still belongs to the latest navigation.
    pub fn is_current(&self, token: u64) -> bool {
        self.0.get() == token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_latest_navigation_is_current() {
        let generation = LoadGeneration::default();
        let slow_usb = generation.bump();
        let fast_bookmark = generation.bump();
        assert!(
            !generation.is_current(slow_usb),
            "stale load must be dropped"
        );
        assert!(generation.is_current(fast_bookmark));
    }

    #[test]
    fn a_navigation_with_no_load_still_invalidates() {
        // Opening a collection (synchronous) after a folder load is in flight
        // must stop that folder's enumeration from landing on top of it.
        let generation = LoadGeneration::default();
        let folder = generation.bump();
        generation.bump(); // open_collection
        assert!(!generation.is_current(folder));
    }
}
