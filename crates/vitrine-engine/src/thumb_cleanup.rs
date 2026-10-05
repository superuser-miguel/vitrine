//! Thumbnail cleanup policy (pure logic).
//!
//! The Preferences "Clean Up Thumbnails…" dialog removes thumbnails two ways:
//! **by age** — not used in N days, Vitrine's own tiers only — and **by missing
//! source** — the file a thumbnail was made from is gone, which also applies to
//! GNOME's shared cache. Both decisions live here; the app supplies the
//! filesystem facts and does the deleting, as with [`crate::cache_evict`].
//!
//! The missing-source rule is deliberately narrow. **Offline is not deleted**:
//! a source on an unplugged drive, behind a portal, or outside what the sandbox
//! can see looks exactly like a deleted one from here, so a thumbnail goes only
//! when its source is *provably* gone — a local `file://` path whose parent
//! directory is visible and readable and which no longer holds the file.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Seconds in a day.
const DAY: i64 = 24 * 3600;

/// The dialog's "Remove thumbnails not used in" choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgeRule {
    /// Don't remove anything by age.
    Off,
    /// Remove thumbnails last used more than this many days ago.
    Days(u32),
    /// Remove every thumbnail in the tier.
    All,
}

impl AgeRule {
    /// Whether a thumbnail last used at `last_used` (unix seconds — the file's
    /// mtime, which reads bump) goes under this rule at time `now`.
    pub fn removes(self, last_used: i64, now: i64) -> bool {
        match self {
            AgeRule::Off => false,
            AgeRule::All => true,
            AgeRule::Days(days) => last_used < now - days as i64 * DAY,
        }
    }
}

/// Indices of the `(size_bytes, last_used)` entries `rule` removes at `now`.
pub fn select_unused(entries: &[(u64, i64)], now: i64, rule: AgeRule) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, &(_, last_used))| rule.removes(last_used, now))
        .map(|(i, _)| i)
        .collect()
}

/// The local path a `file://` URI names, percent-decoded. Only an empty host or
/// `localhost` counts — `file://otherhost/…` is not a path on this machine.
/// `None` for any other scheme or a malformed escape.
pub fn local_path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(OsString::from_vec(out)))
}

/// What the app observed about a thumbnail's source path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceFacts {
    /// The path lies where this process sees the real filesystem: not on
    /// removable media, and (in a sandbox) inside a granted location.
    pub in_view: bool,
    /// The parent directory exists and can be listed.
    pub parent_readable: bool,
    /// Looking the file up failed with "not found" (not any other error).
    pub file_absent: bool,
}

/// Whether the thumbnail made from `uri` may be removed because its source is
/// provably gone. `facts` is asked only for local, non-portal paths.
pub fn source_gone(uri: &str, facts: impl FnOnce(&Path) -> SourceFacts) -> bool {
    let Some(path) = local_path_from_uri(uri) else {
        return false; // not file://, or unparseable: keep
    };
    if path
        .to_str()
        .is_some_and(crate::files::is_portal_document_path)
    {
        return false; // portal ids churn; their absence proves nothing
    }
    let f = facts(&path);
    f.in_view && f.parent_readable && f.file_absent
}

/// Whether `path` is at or under any of `roots`.
pub fn is_within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn age_rules() {
        let old = NOW - 31 * DAY;
        let fresh = NOW - 29 * DAY;
        assert!(AgeRule::Days(30).removes(old, NOW));
        assert!(!AgeRule::Days(30).removes(fresh, NOW));
        assert!(!AgeRule::Days(30).removes(NOW - 30 * DAY, NOW)); // boundary kept
        assert!(AgeRule::All.removes(NOW, NOW));
        assert!(!AgeRule::Off.removes(0, NOW));
    }

    #[test]
    fn select_unused_by_last_use() {
        let entries = [
            (10, NOW - 200 * DAY),
            (20, NOW - 61 * DAY),
            (30, NOW - DAY),
            (40, NOW - 100 * DAY),
        ];
        assert_eq!(select_unused(&entries, NOW, AgeRule::Days(90)), vec![0, 3]);
        assert_eq!(
            select_unused(&entries, NOW, AgeRule::Days(60)),
            vec![0, 1, 3]
        );
        assert_eq!(select_unused(&entries, NOW, AgeRule::Days(180)), vec![0]);
        assert_eq!(select_unused(&entries, NOW, AgeRule::All), vec![0, 1, 2, 3]);
        assert!(select_unused(&entries, NOW, AgeRule::Off).is_empty());
        assert!(select_unused(&[], NOW, AgeRule::All).is_empty());
    }

    #[test]
    fn uri_to_path() {
        assert_eq!(
            local_path_from_uri("file:///home/me/a%20b/caf%C3%A9.jpg"),
            Some(PathBuf::from("/home/me/a b/café.jpg"))
        );
        assert_eq!(
            local_path_from_uri("file://localhost/x.jpg"),
            Some(PathBuf::from("/x.jpg"))
        );
        // Non-UTF-8 bytes survive as raw bytes.
        assert_eq!(
            local_path_from_uri("file:///x%FF.jpg")
                .unwrap()
                .into_os_string()
                .into_vec(),
            b"/x\xFF.jpg".to_vec()
        );
        assert_eq!(local_path_from_uri("file://nas/x.jpg"), None);
        assert_eq!(local_path_from_uri("smb://nas/x.jpg"), None);
        assert_eq!(local_path_from_uri("file:///bad%2"), None);
        assert_eq!(local_path_from_uri("file:///bad%zz"), None);
    }

    fn facts(in_view: bool, parent_readable: bool, file_absent: bool) -> SourceFacts {
        SourceFacts {
            in_view,
            parent_readable,
            file_absent,
        }
    }

    #[test]
    fn gone_only_when_provably_gone() {
        let uri = "file:///home/me/Pictures/a.jpg";
        assert!(source_gone(uri, |_| facts(true, true, true)));
        // Any doubt keeps it.
        assert!(!source_gone(uri, |_| facts(false, true, true))); // outside view
        assert!(!source_gone(uri, |_| facts(true, false, true))); // parent gone/unreadable
        assert!(!source_gone(uri, |_| facts(true, true, false))); // file there / unknown
    }

    #[test]
    fn non_local_and_portal_sources_are_kept_without_looking() {
        let never = |_: &Path| -> SourceFacts { panic!("must not probe") };
        assert!(!source_gone("smb://nas/a.jpg", never));
        assert!(!source_gone("sftp://h/a.jpg", never));
        assert!(!source_gone("file://otherhost/a.jpg", never));
        assert!(!source_gone("file:///run/user/1000/doc/abcd/a.jpg", never));
        assert!(!source_gone("not a uri", never));
    }

    #[test]
    fn facts_see_the_decoded_path() {
        let mut seen = None;
        source_gone("file:///p/a%20b.jpg", |p| {
            seen = Some(p.to_path_buf());
            SourceFacts::default()
        });
        assert_eq!(seen, Some(PathBuf::from("/p/a b.jpg")));
    }

    #[test]
    fn within_roots() {
        let roots = [PathBuf::from("/home/me/Pictures")];
        assert!(is_within(Path::new("/home/me/Pictures/x/a.jpg"), &roots));
        assert!(!is_within(Path::new("/home/me/Pictures2/a.jpg"), &roots));
        assert!(!is_within(Path::new("/home/me/a.jpg"), &roots));
    }
}
