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

/// The freedesktop metadata a cached thumbnail carries about its source: the
/// `Thumb::URI`, `Thumb::MTime` and `Thumb::Size` `tEXt` chunks. Vitrine, GNOME
/// (gnome-desktop / glycin thumbnailers) and GLib all write these as decimal
/// integers — mtime in whole seconds since the epoch, size in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThumbMeta {
    pub uri: Option<String>,
    /// `Some(None)`: the chunk is there but isn't an integer — it can't be
    /// verified, so the thumbnail is treated as stale.
    pub mtime: Option<Option<i64>>,
    pub size: Option<Option<u64>>,
}

impl ThumbMeta {
    /// Read the metadata from an encoded thumbnail (or a prefix of one). One
    /// walk of the chunk headers ahead of `IDAT`: no pixels are decoded, and
    /// the bytes are the ones the caller is about to decode anyway.
    pub fn parse(png: &[u8]) -> ThumbMeta {
        let mut meta = ThumbMeta::default();
        crate::png_meta::for_each_text_chunk(png, |key, value| {
            match key {
                b"Thumb::URI" => meta.uri = Some(String::from_utf8_lossy(value).into_owned()),
                b"Thumb::MTime" => meta.mtime = Some(parse_int(value)),
                b"Thumb::Size" => meta.size = Some(parse_int(value)),
                _ => {}
            }
            meta.uri.is_none() || meta.mtime.is_none() || meta.size.is_none()
        });
        meta
    }
}

/// A decimal integer chunk value. A fractional part (not in the spec, but
/// harmless) is dropped, so `"1700000000.25"` reads as whole seconds.
fn parse_int<T: std::str::FromStr>(value: &[u8]) -> Option<T> {
    let text = std::str::from_utf8(value).ok()?.trim();
    text.split('.').next()?.parse().ok()
}

/// Whether a cached thumbnail still depicts its source, per the spec's
/// "Detect Modifications" section (and GLib's `thumbnail-verify.c`):
///
/// - `Thumb::MTime` must **equal** the source's mtime — not merely be older.
///   A source restored with an older mtime (`cp -p`, `rsync -t`, a backup) or
///   moved over the original is caught; an ordering check misses it.
/// - `Thumb::Size`, when present and the source size is known
///   (`source_size > 0`), must equal it too: copies can share a second.
/// - `Thumb::URI`, when present, must be `uri` (GLib requires this; the
///   filename already hashes the URI, so it only catches a collision).
///
/// Only when the thumbnail has **no** `Thumb::MTime` chunk at all does this
/// fall back to the old ordering rule: the thumbnail file's own mtime
/// (`thumbnail_mtime`) no earlier than the source's.
pub fn is_current(
    meta: &ThumbMeta,
    thumbnail_mtime: i64,
    uri: &str,
    source_mtime: i64,
    source_size: i64,
) -> bool {
    if meta.uri.as_deref().is_some_and(|u| u != uri) {
        return false;
    }
    if let Some(size) = meta.size {
        if source_size > 0 && size != Some(source_size as u64) {
            return false;
        }
    }
    match meta.mtime {
        Some(mtime) => mtime == Some(source_mtime),
        None => thumbnail_mtime >= source_mtime,
    }
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

    const URI: &str = "file:///home/jens/photo/me.png";

    fn meta(mtime: Option<&str>, size: Option<&str>) -> ThumbMeta {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        png.extend_from_slice(&0u32.to_be_bytes());
        let mut entries = vec![("Thumb::URI", URI)];
        if let Some(m) = mtime {
            entries.push(("Thumb::MTime", m));
        }
        if let Some(s) = size {
            entries.push(("Thumb::Size", s));
        }
        ThumbMeta::parse(&crate::png_meta::add_text_chunks(&png, &entries).unwrap())
    }

    #[test]
    fn parses_thumb_chunks() {
        let m = meta(Some("1700000000"), Some("4096"));
        assert_eq!(m.uri.as_deref(), Some(URI));
        assert_eq!(m.mtime, Some(Some(1_700_000_000)));
        assert_eq!(m.size, Some(Some(4096)));
        assert_eq!(
            meta(Some("1700000000.75"), None).mtime,
            Some(Some(1_700_000_000))
        );
        assert_eq!(meta(Some("soon"), None).mtime, Some(None));
        assert_eq!(ThumbMeta::parse(b"not a png"), ThumbMeta::default());
    }

    /// #10: the embedded MTime decides, by equality.
    #[test]
    fn current_when_mtime_chunk_equals_source() {
        let m = meta(Some("1000"), None);
        // Equal: current — even when the thumbnail *file* is older than the
        // source (copied with its times, or never access-touched).
        assert!(is_current(&m, 500, URI, 1000, 0));
        assert!(is_current(&m, 2000, URI, 1000, 0));
    }

    #[test]
    fn stale_when_source_mtime_is_older_than_recorded() {
        // `cp -p` / `rsync -t` / a backup restore put back an *older* mtime:
        // the thumbnail file is newer than the source, so ordering said
        // "current" forever. Equality says stale.
        let m = meta(Some("2000"), None);
        assert!(!is_current(&m, 3000, URI, 1500, 0));
        // Edited afterwards: stale either way.
        assert!(!is_current(&m, 3000, URI, 2500, 0));
    }

    #[test]
    fn missing_mtime_chunk_falls_back_to_ordering() {
        let m = meta(None, None);
        assert!(is_current(&m, 100, URI, 100, 0));
        assert!(is_current(&m, 101, URI, 100, 0));
        assert!(!is_current(&m, 99, URI, 100, 0));
        // No chunks at all (a foreign or pre-spec PNG): same rule.
        assert!(is_current(&ThumbMeta::default(), 101, URI, 100, 0));
        assert!(!is_current(&ThumbMeta::default(), 99, URI, 100, 0));
    }

    #[test]
    fn unreadable_mtime_chunk_is_stale() {
        assert!(!is_current(&meta(Some("soon"), None), 9999, URI, 100, 0));
    }

    #[test]
    fn size_chunk_is_checked_when_present_and_known() {
        let m = meta(Some("1000"), Some("4096"));
        assert!(is_current(&m, 1000, URI, 1000, 4096));
        assert!(!is_current(&m, 1000, URI, 1000, 4097));
        // Size unknown to the caller (0): MTime alone decides.
        assert!(is_current(&m, 1000, URI, 1000, 0));
    }

    #[test]
    fn uri_chunk_must_match_when_present() {
        let m = meta(Some("1000"), None);
        assert!(!is_current(&m, 1000, "file:///elsewhere.png", 1000, 0));
    }
}
