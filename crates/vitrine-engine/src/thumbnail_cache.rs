//! Freedesktop thumbnail-cache addressing (pure logic).
//!
//! The [shared thumbnail cache spec][spec] stores a thumbnail at
//! `$XDG_CACHE_HOME/thumbnails/<size>/<md5>.png`, where `<md5>` is the MD5 hex
//! of the source file's **URI** (not its path). Getting this byte-identical to
//! the spec is what lets Vitrine reuse thumbnails Nautilus/GNOME already
//! generated (and vice-versa) — a wrong key silently means zero sharing. So the
//! keying lives here, UI-free and tested against the spec's canonical example.
//!
//! The app supplies the URI, the cache directory, and the filesystem I/O; this
//! module only decides *where* a thumbnail lives and *whether* it is current.
//!
//! [spec]: https://specifications.freedesktop.org/thumbnail-spec/latest/

use std::fmt::Write as _;

use md5::{Digest, Md5};

/// A freedesktop thumbnail size class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbBucket {
    Normal,
    Large,
    XLarge,
    XxLarge,
}

impl ThumbBucket {
    /// The subdirectory name under `thumbnails/`.
    pub fn dir(self) -> &'static str {
        match self {
            ThumbBucket::Normal => "normal",
            ThumbBucket::Large => "large",
            ThumbBucket::XLarge => "x-large",
            ThumbBucket::XxLarge => "xx-large",
        }
    }

    /// The bucket's edge size in pixels (thumbnails fit within this box).
    pub fn pixels(self) -> u32 {
        match self {
            ThumbBucket::Normal => 128,
            ThumbBucket::Large => 256,
            ThumbBucket::XLarge => 512,
            ThumbBucket::XxLarge => 1024,
        }
    }

    /// The smallest bucket that can display `target` px without upscaling.
    pub fn for_target(target: u32) -> ThumbBucket {
        [
            ThumbBucket::Normal,
            ThumbBucket::Large,
            ThumbBucket::XLarge,
            ThumbBucket::XxLarge,
        ]
        .into_iter()
        .find(|b| b.pixels() >= target)
        .unwrap_or(ThumbBucket::XxLarge)
    }
}

/// The MD5-hex thumbnail key for a file URI (the spec's filename stem).
pub fn cache_key(uri: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(uri.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The thumbnail's path relative to the `thumbnails/` directory,
/// e.g. `large/d40775e596682f2a16d1b834c221c0a2.png`.
pub fn relative_path(uri: &str, bucket: ThumbBucket) -> String {
    format!("{}/{}.png", bucket.dir(), cache_key(uri))
}

/// A cached thumbnail is current when it was written no earlier than the
/// source's last modification (both unix seconds). If the source was edited
/// after the thumbnail was written, the thumbnail is stale and must be redrawn.
pub fn is_current(thumbnail_mtime: i64, source_mtime: i64) -> bool {
    thumbnail_mtime >= source_mtime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_spec_example() {
        // The spec's example URI; hash verified byte-for-byte against `md5sum`,
        // so our keys match the system-generated thumbnails.
        assert_eq!(
            cache_key("file:///home/jens/photo/me.png"),
            "d40775e596682f2a16d1b834c221c0a2"
        );
    }

    #[test]
    fn relative_path_uses_bucket_and_key() {
        assert_eq!(
            relative_path("file:///home/jens/photo/me.png", ThumbBucket::Large),
            "large/d40775e596682f2a16d1b834c221c0a2.png"
        );
    }

    #[test]
    fn bucket_for_target_picks_smallest_that_fits() {
        assert_eq!(ThumbBucket::for_target(96), ThumbBucket::Normal);
        assert_eq!(ThumbBucket::for_target(128), ThumbBucket::Normal);
        assert_eq!(ThumbBucket::for_target(129), ThumbBucket::Large);
        assert_eq!(ThumbBucket::for_target(256), ThumbBucket::Large);
        assert_eq!(ThumbBucket::for_target(400), ThumbBucket::XLarge);
        assert_eq!(ThumbBucket::for_target(1024), ThumbBucket::XxLarge);
        assert_eq!(ThumbBucket::for_target(9999), ThumbBucket::XxLarge);
    }

    #[test]
    fn currency_is_mtime_ordering() {
        assert!(is_current(100, 100));
        assert!(is_current(101, 100));
        assert!(!is_current(99, 100));
    }
}
