//! # vitrine-engine
//!
//! UI-free core for **Vitrine**, a catalog-aware image browser + reviewer.
//!
//! All parsing, hashing, indexing, dedup clustering, and query logic lives
//! here. The `vitrine-app` crate is a thin GTK4/libadwaita shell over it.
//!
//! ## Boundary (PLAN §1, house rule 2)
//!
//! This crate MUST NOT depend on GTK, GLib, Gio, libadwaita, or ashpd. It
//! accepts plain paths — including opaque document-portal paths under
//! `/run/user/*/doc/` — without normalization; the app owns all portal
//! interaction. The boundary is enforced by `build-aux/checks.sh` and CI:
//!
//! ```text
//! cargo tree -p vitrine-engine -e normal | grep -E "gtk4|glib|gio|libadwaita|ashpd"
//! ```
//!
//! must return nothing.
//!
//! ## Roadmap (populated phase-by-phase, see PLAN.md)
//!
//! - Phase 2: `db`, `schema`, `scanner`, `hash`, `query`
//! - Phase 4: `dedup`
//! - Phase 1: LRU texture-cache eviction policy is hosted here so it is
//!   testable without GTK.

pub mod annotations;
pub mod backup;
pub mod cache_evict;
pub mod collections;
pub mod db;
pub mod dedup;
pub mod exif;
pub mod files;
pub mod hash;
pub mod lru;
pub mod png_meta;
pub mod query;
pub mod resize;
pub mod scanner;
pub mod schema;
pub mod tags;
pub mod thumbnail_cache;
pub mod xmp;

pub use collections::{Collection, CollectionKind};
pub use db::Db;
pub use dedup::DuplicateCluster;
pub use exif::{parse_exif, ExifData};
pub use files::{Enrichment, FileRecord};
pub use hash::{blake3_bytes, blake3_file, phash_distance, phash_rgb8};
pub use query::{Direction, Query, SortKey};
pub use resize::resize_rgba;
pub use scanner::{classify, walk_images, Change, ScannedFile};
pub use tags::Tag;
pub use xmp::{sidecar_path, XmpMetadata};

pub use lru::SizedLru;
pub use thumbnail_cache::{cache_key, relative_path, ThumbBucket};

/// Crate version, surfaced by the app's About dialog to prove the app↔engine
/// wiring end-to-end in Phase 0.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!VERSION.is_empty());
    }
}
