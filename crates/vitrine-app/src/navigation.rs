//! Small, widget-free bookkeeping for the window's navigation lifecycle.
//!
//! Kept out of `window.rs` so the rules can be unit-tested without a display:
//! which async result is still wanted, and when indexing is really over.

use std::cell::Cell;
use std::path::Path;

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

/// How many index scans are running right now.
///
/// Scans nest (a user-opened folder runs inside a launch root rescan) and the
/// roots queue one after another, so "a scan finished" is not "indexing is
/// over". The banner and the `scanning` flag follow this count instead.
#[derive(Debug, Default)]
pub struct ScanTracker(Cell<usize>);

impl ScanTracker {
    /// A scan started.
    pub fn started(&self) {
        self.0.set(self.0.get() + 1);
    }

    /// A scan finished. Returns `true` when that was the last one running.
    ///
    /// Saturates: a `Finished` without its `Started` (never sent today, but
    /// cheap to tolerate) must not wrap the count and pin the banner open.
    pub fn finished(&self) -> bool {
        let left = self.0.get().saturating_sub(1);
        self.0.set(left);
        left == 0
    }

    /// Whether any scan is still running.
    pub fn active(&self) -> bool {
        self.0.get() > 0
    }
}

/// Whether a finished scan of `scanned` can have changed what the grid shows
/// for the open folder — i.e. the open folder lies inside the scanned subtree.
///
/// Nothing open (a collection or tag view, or the launch state) is never
/// covered: those views are built from the index and stamped as they load,
/// and the folder-scoped stamp has nothing to act on.
pub fn scan_covers(scanned: &Path, open_folder: Option<&Path>) -> bool {
    open_folder.is_some_and(|open| open.starts_with(scanned))
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

    #[test]
    fn scans_are_over_only_when_the_last_one_finishes() {
        let scans = ScanTracker::default();
        assert!(!scans.active());
        for _ in 0..3 {
            scans.started(); // three library roots
        }
        assert!(!scans.finished());
        assert!(!scans.finished());
        assert!(scans.active());
        assert!(scans.finished());
        assert!(!scans.active());
    }

    #[test]
    fn nested_scan_does_not_end_the_outer_one() {
        let scans = ScanTracker::default();
        scans.started(); // root rescan
        scans.started(); // user folder, nested at a checkpoint
        assert!(!scans.finished());
        assert!(scans.active());
        assert!(scans.finished());
    }

    #[test]
    fn unmatched_finish_does_not_wrap() {
        let scans = ScanTracker::default();
        assert!(scans.finished());
        assert!(!scans.active());
        scans.started();
        assert!(scans.active());
    }

    #[test]
    fn scan_covers_the_open_folder_and_its_ancestors_only() {
        let open = Path::new("/media/usb/photos/2024");
        assert!(scan_covers(Path::new("/media/usb/photos/2024"), Some(open)));
        assert!(scan_covers(Path::new("/media/usb"), Some(open)));
        // A sibling root, a subfolder, or a mere string prefix: not covered.
        assert!(!scan_covers(Path::new("/home/me/Pictures"), Some(open)));
        assert!(!scan_covers(
            Path::new("/media/usb/photos/2024/raw"),
            Some(open)
        ));
        assert!(!scan_covers(Path::new("/media/usb/photos/20"), Some(open)));
        assert!(!scan_covers(Path::new("/media/usb"), None));
    }
}
