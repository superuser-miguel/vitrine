//! Thumbnail loading: our caches and GNOME's, glycin on a miss.
//!
//! The disk tiers, in load order (fastest first; the RAM `SizedLru` sits above
//! all of them):
//!  1. **Content tier** (`$XDG_CACHE_HOME/content-thumbnails/…`), keyed by the
//!     BLAKE3 content hash — read by the grid before [`load`] (PLAN §16.5,
//!     Flavor 2).
//!  2. **Removable tier** (`$XDG_CACHE_HOME/removable-thumbnails/…`) — for
//!     USB/network sources only, separately budgeted (Flavor 1).
//!  3. **GNOME's shared cache** (`~/.cache/thumbnails/…`, the host's, via the
//!     `xdg-cache/thumbnails` grant) — reuses what Nautilus already generated.
//!     **Not ours:** we read it and never touch it, add only the spec's standard
//!     `normal`/`large` sizes to it, and never prune it.
//!  4. **Vitrine's private cache** (`$XDG_CACHE_HOME/vitrine-thumbnails/…`) —
//!     our own decodes that don't belong in the shared cache (`x-large`,
//!     `xx-large`, portal paths), LRU-pruned to the user's budget.
//!  5. **glycin decode** (concurrency-gated) + CPU downscale on a worker thread,
//!     then written to *one* of the URI tiers (see [`roots_for`]).
//!
//! **V-30:** the private tier used to be `$XDG_CACHE_HOME/thumbnails`. A
//! Flatpak `xdg-cache/<subdir>` grant also bind-mounts the host dir over the
//! app's own `$XDG_CACHE_HOME/<subdir>`, so that path *was* GNOME's cache:
//! there was no private tier, every thumbnail was written twice to one file,
//! and the budget prune ran on GNOME's cache. The private dir is now a name no
//! grant covers.
//!
//! **§4 RISK, resolved:** cache keys are the MD5 of the file *URI*. Real paths
//! (e.g. under `xdg-pictures`) present the same URI inside the sandbox as on the
//! host, so shared-cache hits work. Document-portal paths do not match the host
//! URI, so we only ever *read* the shared cache (a harmless miss for those) and
//! *write* them to the private cache — never polluting the shared cache with
//! keys the host can't reproduce.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use vitrine_engine::thumbnail_cache::{self, ThumbBucket};
use vitrine_engine::SizedLru;

/// Budget for the in-RAM thumbnail cache (~1500 × 256px textures). This is the
/// bound that keeps memory flat regardless of folder size: items no longer hold
/// their textures, so browsing a 27k-image folder can't accumulate GBs (→ OOM).
const RAM_CACHE_BYTES: u64 = 384 * 1024 * 1024;

/// On a cache hit, only bump the file's mtime (to record access) if it is older
/// than this many seconds — so scrolling doesn't cause a write per read.
const ACCESS_TOUCH_AFTER: i64 = 6 * 3600;

/// Size-bounded, LRU RAM cache of decoded thumbnails, keyed by file URI. Shared
/// (single-threaded, `Rc`) between the grid and the viewer's filmstrip.
pub type ThumbCache = Rc<RefCell<SizedLru<String, gdk::Texture>>>;

/// Create the shared RAM thumbnail cache.
pub fn new_ram_cache() -> ThumbCache {
    Rc::new(RefCell::new(SizedLru::new(RAM_CACHE_BYTES)))
}

/// A texture's approximate cost in bytes (RGBA).
pub fn texture_cost(texture: &gdk::Texture) -> u64 {
    texture.width() as u64 * texture.height() as u64 * 4
}

/// RAM-cache key: URI plus the resolution bucket, so the same image cached at
/// different icon sizes (e.g. 256 vs 512) doesn't collide.
pub fn ram_key(uri: &str, target_px: u32) -> String {
    format!("{uri}#{}", ThumbBucket::for_target(target_px).pixels())
}

/// Admission gate for *all* thumbnail loads (cache reads included, not just
/// glycin decodes). Fast-scrolling a big folder binds thousands of cells, each
/// spawning a load; without a bound they flood the main loop (async I/O + PNG
/// decode) and it stalls. Gating admission — plus each caller re-checking that
/// its cell still wants the image after the wait — means cells scrolled past
/// before their turn bail instead of doing work. Override VITRINE_LOAD_LIMIT.
pub fn load_gate() -> &'static async_lock::Semaphore {
    static GATE: OnceLock<async_lock::Semaphore> = OnceLock::new();
    GATE.get_or_init(|| {
        let limit = std::env::var("VITRINE_LOAD_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(24);
        async_lock::Semaphore::new(limit)
    })
}

/// The shared freedesktop thumbnail cache (host cache; shared with Nautilus).
fn shared_dir() -> PathBuf {
    glib::home_dir().join(".cache/thumbnails")
}

/// Vitrine's private thumbnail cache. Deliberately *not* `…/thumbnails`: under
/// Flatpak the `xdg-cache/thumbnails` grant bind-mounts the host's shared cache
/// over `$XDG_CACHE_HOME/thumbnails` (V-30), and on the host that name is the
/// shared cache outright. No `--filesystem` grant covers this name.
fn private_dir() -> PathBuf {
    glib::user_cache_dir().join("vitrine-thumbnails")
}

/// Whether `dir` is GNOME's shared cache under another name — the same
/// directory by device + inode, which is what sees through a bind mount (a
/// path comparison can't). Our pruning and cleanup refuse to run on it.
fn is_shared_cache(dir: &std::path::Path) -> bool {
    same_dir(dir, &shared_dir())
}

/// Whether two paths name the same existing directory (device + inode).
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(a), Ok(b)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Persistent cache for removable/remote (USB, network) thumbnails — a *separate*
/// dir from `private_dir` so local browsing's LRU prune never evicts them
/// (PLAN §16.5, Flavor 1). Budgeted independently via `removable_cache_mb`.
fn removable_dir() -> PathBuf {
    glib::user_cache_dir().join("removable-thumbnails")
}

/// The removable-tier budget in MB (`VITRINE_REMOVABLE_MB` overrides Preferences).
fn removable_budget_mb() -> u64 {
    std::env::var("VITRINE_REMOVABLE_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| crate::settings::Settings::load().removable_cache_mb())
}

/// Whether the removable/remote tier is enabled (budget > 0). Read once — a
/// budget change takes effect on restart, which keeps this off the hot path.
fn removable_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| removable_budget_mb() > 0)
}

/// Typical auto-mount roots for removable media.
fn is_removable_path(path: &str) -> bool {
    path.starts_with("/run/media/") || path.starts_with("/media/") || path.starts_with("/mnt/")
}

/// Cheap, synchronous heuristic on the raw URI: network (GVFS) schemes, or a
/// real `file://` path under a removable mount root.
fn is_removable_uri(uri: &str) -> bool {
    const NET: &[&str] = &[
        "smb://",
        "sftp://",
        "ftp://",
        "dav://",
        "davs://",
        "nfs://",
        "mtp://",
        "gphoto2://",
        "afp://",
        "google-drive://",
    ];
    if NET.iter().any(|s| uri.starts_with(s)) {
        return true;
    }
    matches!(uri.strip_prefix("file://"), Some(path) if is_removable_path(path))
}

/// A document-portal URI: `file:///run/user/<UID>/doc/<doc-id>/…`. Inside the
/// Flatpak sandbox, a removable drive granted through the file chooser appears
/// here — the real `/run/media/…` location hidden behind the doc id.
fn is_doc_portal_uri(uri: &str) -> bool {
    matches!(
        uri.strip_prefix("file:///run/user/")
            .and_then(|rest| rest.split_once('/')),
        Some((_uid, rest)) if rest.starts_with("doc/")
    )
}

/// Resolve a document-portal file to its real host path via the FUSE mount's
/// `user.document-portal.host-path` xattr (gio drops the `user.` prefix). This is
/// what lets us see that a portal-granted folder actually lives on a USB.
fn doc_portal_host_path(file: &gio::File) -> Option<String> {
    let info = file
        .query_info(
            "xattr::document-portal.host-path",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok()?;
    info.attribute_string("xattr::document-portal.host-path")
        .map(|s| s.to_string())
}

/// Whether this source lives on removable/remote media, resolving through the
/// document portal when needed (PLAN §16.5, Flavor 1). The xattr query runs only
/// for doc-portal paths, so ordinary local/direct files pay nothing.
fn is_removable_source(file: &gio::File, uri: &str) -> bool {
    if is_removable_uri(uri) {
        return true;
    }
    if is_doc_portal_uri(uri) {
        if let Some(host) = doc_portal_host_path(file) {
            let removable = is_removable_path(&host);
            // One-shot signal that portal→host-path resolution fired inside the
            // sandbox (the uncertain part). VITRINE_DEBUG only.
            if removable && crate::debug::enabled() {
                use std::sync::atomic::{AtomicBool, Ordering};
                static LOGGED: AtomicBool = AtomicBool::new(false);
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    eprintln!("VDBG-REMOVABLE portal folder → removable host path: {host}");
                }
            }
            return removable;
        }
    }
    false
}

// ---- Content-hash tier (PLAN §16.5, Flavor 2) --------------------------------
// A thumbnail cache keyed by the file's BLAKE3 content hash instead of its path,
// so the same bytes reuse one thumbnail regardless of path, drive, or mtime —
// duplicates, renames, and backups all hit. **The hash is the validation**: same
// content ⇒ same key ⇒ correct thumbnail, so there is no mtime check. The hash is
// already stamped on each grid item (`ImageObject::content_hash`), so the lookup
// costs a field read, not a file read. Only Vitrine's own caches join this scheme
// — the shared freedesktop tier stays URI-keyed (spec).

/// Content cache dir, keyed by BLAKE3. Deduplicated by content, so it holds one
/// entry per unique image however many paths point at it.
fn content_dir() -> PathBuf {
    glib::user_cache_dir().join("content-thumbnails")
}

/// The content tier's budget in MB (`VITRINE_CONTENT_MB` overrides Preferences).
fn content_budget_mb() -> u64 {
    std::env::var("VITRINE_CONTENT_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| crate::settings::Settings::load().content_cache_mb())
}

/// Whether the content tier is enabled (budget > 0). Read once (restart to change).
fn content_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| content_budget_mb() > 0)
}

fn content_path(hash: &str, bucket: ThumbBucket) -> PathBuf {
    content_dir().join(bucket.dir()).join(format!("{hash}.png"))
}

/// Read the content-keyed thumbnail for `hash`, if present. **No mtime check** —
/// the hash guarantees the bytes. Off the main thread; returns the as-decoded
/// thumbnail (the caller applies any rotate/crop edit).
///
/// A hit records access the way the private tier does — the mtime is bumped
/// when older than [`ACCESS_TOUCH_AFTER`] — so the cleanup's "not used in N
/// days" means *read*, not *created*. Same open, one `fstat` (which `fs::read`
/// did anyway), and at most one timestamp write per file per 6 h.
pub async fn read_content(hash: &str, target_px: u32) -> Option<gdk::Texture> {
    if !content_enabled() || hash.is_empty() {
        return None;
    }
    let path = content_path(hash, ThumbBucket::for_target(target_px));
    let tex = gio::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(&path).ok()?;
        let meta = file.metadata().ok()?;
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        file.read_to_end(&mut bytes).ok()?;
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()?;
        let stale = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| (d.as_secs() as i64) < now_secs() - ACCESS_TOUCH_AFTER);
        if stale {
            // futimens on the read fd: we own the file, no write access needed.
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        Some(texture)
    })
    .await
    .ok()
    .flatten();
    if tex.is_some() {
        crate::debug::cache_hit();
        if crate::debug::enabled() {
            use std::sync::atomic::{AtomicBool, Ordering};
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!("VDBG-CONTENT hit — content-keyed reuse (path-independent)");
            }
        }
    }
    tex
}

/// Store a decoded thumbnail under its content hash (fire-and-forget, worker).
/// Skips if a file is already there — the hash is stable, so present ⇒ current.
pub fn store_content(hash: &str, target_px: u32, texture: &gdk::Texture) {
    if !content_enabled() || hash.is_empty() {
        return;
    }
    let path = content_path(hash, ThumbBucket::for_target(target_px));
    let texture = texture.clone(); // gdk::Texture is Send
    gio::spawn_blocking(move || {
        if path.exists() {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let wrote = texture.save_to_png(&path).is_ok();
        if wrote && crate::debug::enabled() {
            use std::sync::atomic::{AtomicBool, Ordering};
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!("VDBG-CONTENT wrote — content tier active");
            }
        }
    });
}

/// Prune the content tier to its budget; 0 = disabled, nothing to prune.
pub fn prune_content_cache() {
    prune_dir(
        content_dir(),
        content_budget_mb().saturating_mul(1024 * 1024),
    );
}

/// A weak reference used only to obtain a GSK renderer after decoding.
pub fn renderer_source(widget: &impl IsA<gtk::Widget>) -> glib::WeakRef<gtk::Widget> {
    widget.clone().upcast::<gtk::Widget>().downgrade()
}

/// Load a thumbnail for `file` at roughly `target_px`, from cache or by decoding.
///
/// `source_mtime` (unix seconds) validates cached entries. `renderer_widget` is
/// resolved to a GSK renderer *after* decoding (so a not-yet-realized cell still
/// works) to GPU-downscale a full-resolution decode; the shrunk result is cached.
/// Returns `None` if the image cannot be decoded.
pub async fn load(
    file: gio::File,
    source_mtime: i64,
    target_px: u32,
    byte_size: i64,
    _renderer_widget: glib::WeakRef<gtk::Widget>,
) -> Option<gdk::Texture> {
    let bucket = ThumbBucket::for_target(target_px);
    let uri = file.uri().to_string();
    // Removable/remote sources get a persistent, separately-budgeted tier so
    // their thumbnails survive local browsing's LRU eviction (PLAN §16.5).
    let removable = removable_enabled() && is_removable_source(&file, &uri);

    // VITRINE_NOCACHE forces the cold path (skip cache reads → always decode).
    if !crate::debug::force_decode() {
        // Our persistent removable/remote copy first, when applicable.
        if removable {
            if let Some(texture) =
                read_cache(removable_dir(), &uri, bucket, source_mtime, true).await
            {
                crate::debug::cache_hit();
                return Some(texture);
            }
        }
        // Shared cache is GNOME's — read but never re-touch it.
        if let Some(texture) = read_cache(shared_dir(), &uri, bucket, source_mtime, false).await {
            crate::debug::cache_hit();
            return Some(texture);
        }
        // Private cache is ours — mark access so eviction is LRU, not FIFO.
        if let Some(texture) = read_cache(private_dir(), &uri, bucket, source_mtime, true).await {
            crate::debug::cache_hit();
            return Some(texture);
        }
    }

    crate::debug::cache_miss();
    crate::debug::decode_begin();
    let decoded = crate::decode::thumbnail(&file, bucket.pixels(), byte_size).await;
    crate::debug::decode_end();
    let texture = match decoded {
        Ok(texture) => texture,
        Err(err) => {
            glib::g_warning!("vitrine", "thumbnail {uri}: {err}");
            return None;
        }
    };

    // glycin may return full resolution; shrink for cache + display on a worker
    // thread (CPU resize) so a large image never blocks the main loop — and with
    // no GSK render_texture there's no ~1.7s shader compile. Only cache when we
    // actually shrank (never a multi-MB "thumbnail").
    match downscale_cpu(texture, bucket.pixels()).await {
        Some(thumb) => {
            // Fill metric (§13.3): when each *decoded* thumbnail completed, and
            // how big its source was — aggregate-only, for cold pop-in analysis.
            if crate::debug::enabled() {
                eprintln!(
                    "VDBG-FILL ms={} bytes={byte_size}",
                    crate::debug::since_start_ms()
                );
            }
            store(
                &uri,
                source_mtime,
                bucket,
                &thumb,
                is_shareable(&file),
                removable,
            );
            Some(thumb)
        }
        None => None,
    }
}

/// Shrink a decoded texture on a worker thread: download its RGBA, resize in the
/// engine (pure pixel math), and rebuild a small `MemoryTexture` (which holds no
/// dmabuf FD). Returns the input unchanged if it already fits the bucket, or
/// `None` on failure. This is what moves the downscale off the main thread.
/// Also used by the viewer to enforce its VIEW_MAX cap (glycin's scale request
/// is best-effort) before an oversized frame reaches the GPU upload.
pub(crate) async fn downscale_cpu(texture: gdk::Texture, max: u32) -> Option<gdk::Texture> {
    let w = texture.width() as u32;
    let h = texture.height() as u32;
    if w == 0 || h == 0 {
        return None;
    }
    if w.max(h) <= max {
        return Some(texture); // already thumbnail-sized (glycin honored the scale)
    }
    gio::spawn_blocking(move || {
        let (bytes, stride) = {
            let mut downloader = gdk::TextureDownloader::new(&texture);
            downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
            downloader.download_bytes()
        };
        // Free the full-resolution texture (tens of MB) before resizing, so many
        // concurrent downscales don't pile up full-res images in memory.
        drop(texture);
        let resized = vitrine_engine::resize_rgba(&bytes, w, h, stride as u32, max);
        drop(bytes);
        let (out, nw, nh) = resized?;
        Some(
            gdk::MemoryTextureBuilder::new()
                .set_bytes(Some(&glib::Bytes::from_owned(out)))
                .set_width(nw as i32)
                .set_height(nh as i32)
                .set_stride((nw as usize) * 4)
                .set_format(gdk::MemoryFormat::R8g8b8a8)
                .build()
                .upcast::<gdk::Texture>(),
        )
    })
    .await
    .ok()
    .flatten()
}

/// Apply the non-destructive edit instructions — orientation (EXIF 1–8) then
/// crop (normalized display-space rect) — to a texture on a worker thread.
/// Identity instructions return the input untouched. Applied *after* cache
/// reads / decode+downscale, so the disk caches always hold the as-decoded
/// pixels (the shared cache is Nautilus's view of the file — never edit it).
pub(crate) async fn transform_cpu(
    texture: gdk::Texture,
    orientation: i32,
    crop: Option<(f64, f64, f64, f64)>,
) -> Option<gdk::Texture> {
    if orientation <= 1 && crop.is_none() {
        return Some(texture);
    }
    let w = texture.width() as u32;
    let h = texture.height() as u32;
    gio::spawn_blocking(move || {
        let (bytes, stride) = {
            let mut downloader = gdk::TextureDownloader::new(&texture);
            downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
            downloader.download_bytes()
        };
        drop(texture);
        // Orient first (crop rects are display-space), each step passing
        // through untouched when it's the identity.
        let (bytes, w, h, stride) =
            match vitrine_engine::orient_rgba(&bytes, w, h, stride as u32, orientation as i64) {
                Some((o, ow, oh)) => (glib::Bytes::from_owned(o), ow, oh, None),
                None => (bytes, w, h, Some(stride as u32)),
            };
        let stride = stride.unwrap_or(w * 4);
        let (out, nw, nh) = match crop {
            Some(rect) => vitrine_engine::crop_rgba(&bytes, w, h, stride, rect)?,
            None => {
                if orientation <= 1 {
                    return None; // nothing was applied (shouldn't happen)
                }
                (bytes.to_vec(), w, h)
            }
        };
        Some(
            gdk::MemoryTextureBuilder::new()
                .set_bytes(Some(&glib::Bytes::from_owned(out)))
                .set_width(nw as i32)
                .set_height(nh as i32)
                .set_stride((nw as usize) * 4)
                .set_format(gdk::MemoryFormat::R8g8b8a8)
                .build()
                .upcast::<gdk::Texture>(),
        )
    })
    .await
    .ok()
    .flatten()
}

/// RAM-cache key suffix for the edit instructions — identity adds nothing, so
/// unedited items keep their existing keys.
pub fn edit_key(orientation: i32, crop: Option<(f64, f64, f64, f64)>) -> String {
    let mut key = String::new();
    if orientation > 1 {
        key.push_str(&format!("#o{orientation}"));
    }
    if let Some((x, y, w, h)) = crop {
        key.push_str(&format!("#c{x:.3},{y:.3},{w:.3},{h:.3}"));
    }
    key
}

/// Thumbnail size the background enrichment pass warms into the disk cache — the
/// grid's default load size — so an indexed folder opens with no on-demand decode.
pub const WARM_PX: u32 = 256;

/// Warm the on-disk thumbnail cache for `file` from a texture the enrichment pass
/// already decoded (it decodes every image for the pHash anyway). The grid reads
/// this same URI-keyed cache, so a folder that has been indexed shows thumbnails
/// instantly instead of decoding on scroll. Disk only — enrichment isn't display,
/// so it doesn't touch the RAM cache.
pub async fn warm_cache(file: &gio::File, source_mtime: i64, frame: &gdk::Texture) {
    let bucket = ThumbBucket::for_target(WARM_PX);
    let Some(thumb) = downscale_cpu(frame.clone(), bucket.pixels()).await else {
        return;
    };
    let uri = file.uri().to_string();
    let removable = removable_enabled() && is_removable_source(file, &uri);
    let roots = roots_for(is_shareable(file), removable, bucket);
    // Awaited (not fire-and-forget) and unbounded, unlike `store`: warm thumbnails
    // are already small (no full-res RSS risk), and enrichment's own bounded
    // concurrency paces these — so every indexed image actually gets warmed rather
    // than being dropped by the write backlog cap.
    let _ =
        gio::spawn_blocking(move || write_thumb(&thumb, &uri, source_mtime, bucket, &roots)).await;
}

/// Whether a decode for `file` may be written to the *shared* cache: only for
/// real paths, whose URI matches the host's. Document-portal paths
/// (`/run/user/<uid>/doc/…`) present a sandbox-only URI, so their key wouldn't
/// match the host — we keep those app-private and never pollute the shared cache.
fn is_shareable(file: &gio::File) -> bool {
    file.path()
        .map(|p| !p.starts_with("/run/user"))
        .unwrap_or(false)
}

/// Read and validate a cached thumbnail from `dir`. A cache entry is used only
/// when its mtime is at least the source's (i.e. it isn't stale).
async fn read_cache(
    dir: PathBuf,
    uri: &str,
    bucket: ThumbBucket,
    source_mtime: i64,
    touch: bool,
) -> Option<gdk::Texture> {
    let path = dir.join(thumbnail_cache::relative_path(uri, bucket));

    // Stat + read + PNG-decode entirely off the main thread — gdk::Texture is
    // Send, so only the finished texture comes back. This is what keeps the main
    // loop responsive while scrolling thousands of cached thumbnails.
    let read_path = path.clone();
    let (texture, cache_mtime) = gio::spawn_blocking(move || {
        let meta = std::fs::metadata(&read_path).ok()?;
        let cache_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)?;
        if !thumbnail_cache::is_current(cache_mtime, source_mtime) {
            return None;
        }
        let bytes = std::fs::read(&read_path).ok()?;
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()?;
        Some((texture, cache_mtime))
    })
    .await
    .ok()
    .flatten()?;

    // Record access for LRU eviction, throttled so scrolling isn't write-heavy.
    if touch {
        let now = now_secs();
        if cache_mtime < now - ACCESS_TOUCH_AFTER {
            gio::File::for_path(&path)
                .set_attribute_uint64(
                    "time::modified",
                    now as u64,
                    gio::FileQueryInfoFlags::NONE,
                    gio::Cancellable::NONE,
                )
                .ok();
        }
    }
    Some(texture)
}

/// Current unix time in seconds.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Prune a disk thumbnail cache dir to `cap` bytes (LRU eviction). Runs off the
/// main thread; best-effort. `cap == 0` prunes nothing (a disabled tier).
fn prune_dir(root: PathBuf, cap: u64) {
    if cap == 0 {
        return;
    }
    std::thread::spawn(move || {
        // Never GNOME's cache, whatever path reached us (V-30).
        if is_shared_cache(&root) {
            glib::g_warning!(
                "vitrine",
                "not pruning {}: it is the shared thumbnail cache",
                root.display()
            );
            return;
        }
        let (files, facts): (Vec<PathBuf>, Vec<(u64, i64)>) = list_thumbs(&root)
            .into_iter()
            .map(|(path, size, mtime)| (path, (size, mtime)))
            .unzip();
        for i in vitrine_engine::cache_evict::evict_lru(&facts, cap) {
            let _ = std::fs::remove_file(&files[i]);
        }
    });
}

/// Every thumbnail file in a cache dir's size buckets, as `(path, size,
/// mtime)`. Blocking (call it off the main thread).
fn list_thumbs(root: &std::path::Path) -> Vec<(PathBuf, u64, i64)> {
    let mut out = Vec::new();
    for bucket in ["normal", "large", "x-large", "xx-large"] {
        let Ok(entries) = std::fs::read_dir(root.join(bucket)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            out.push((entry.path(), meta.len(), mtime_secs(&meta)));
        }
    }
    out
}

/// A file's mtime in unix seconds (0 if unavailable).
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Prune the app-private disk cache to the configured budget (see
/// [`crate::settings`]), evicting the least-recently-used files.
pub fn prune_private_cache() {
    // VITRINE_CACHE_CAP_MB (dev override) wins, else the user's configured size.
    let cap = std::env::var("VITRINE_CACHE_CAP_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| crate::settings::Settings::load().cache_mb())
        .saturating_mul(1024 * 1024);
    prune_dir(private_dir(), cap);
}

/// Prune the persistent removable/remote cache to its own (larger) budget. A 0
/// budget means the tier is disabled — nothing to prune.
pub fn prune_removable_cache() {
    prune_dir(
        removable_dir(),
        removable_budget_mb().saturating_mul(1024 * 1024),
    );
}

/// Whether a thumbnail at `bucket` may go into GNOME's shared cache: only the
/// sizes GNOME itself generates. `x-large`/`xx-large` are Vitrine's grid sizes
/// and stay private — GNOME's cache is not ours to grow.
fn is_standard_bucket(bucket: ThumbBucket) -> bool {
    matches!(bucket, ThumbBucket::Normal | ThumbBucket::Large)
}

/// The cache roots a thumbnail is written to — each one a tier the load order
/// will actually read back, so nothing is written twice for nothing (V-30):
/// - **removable/remote** → the persistent removable tier, plus the shared cache
///   at a standard size when the path is a real host path (Nautilus interop);
/// - **local, standard size, real host path** → the shared cache alone (it is
///   read before the private tier, so a private copy would never be used);
/// - **everything else** (`x-large`/`xx-large`, portal paths) → the private
///   tier.
fn roots_for(shareable: bool, removable: bool, bucket: ThumbBucket) -> Vec<PathBuf> {
    let to_shared = shareable && is_standard_bucket(bucket);
    if removable {
        let mut roots = vec![removable_dir()];
        if to_shared {
            roots.push(shared_dir());
        }
        roots
    } else if to_shared {
        vec![shared_dir()]
    } else {
        vec![private_dir()]
    }
}

/// PNG-encode `texture` (with the freedesktop `Thumb::URI`/`Thumb::MTime` chunks)
/// and write it into each cache root. Runs on a worker thread (callers wrap it in
/// `spawn_blocking`).
fn write_thumb(
    texture: &gdk::Texture,
    uri: &str,
    source_mtime: i64,
    bucket: ThumbBucket,
    roots: &[PathBuf],
) {
    let png = texture.save_to_png_bytes();
    let png = vitrine_engine::png_meta::add_text_chunks(
        &png,
        &[
            ("Thumb::URI", uri),
            ("Thumb::MTime", &source_mtime.to_string()),
        ],
    )
    .unwrap_or_else(|| png.to_vec());
    let rel = format!("{}.png", thumbnail_cache::cache_key(uri));
    for root in roots {
        let dir = root.join(bucket.dir());
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(dir.join(&rel), &png);
        }
    }
}

/// Write `texture` to the thumbnail cache(s), tagged with the freedesktop
/// `Thumb::URI`/`Thumb::MTime` metadata. Fire-and-forget: PNG **encode and disk
/// write happen on a worker thread** (both are pure CPU/IO and were a major
/// main-loop stall while populating). Where it lands is [`roots_for`].
fn store(
    uri: &str,
    source_mtime: i64,
    bucket: ThumbBucket,
    texture: &gdk::Texture,
    shareable: bool,
    removable: bool,
) {
    // Bound in-flight disk writes: a fast cold scroll produces thumbnails faster
    // than they can be PNG-encoded + written, and unbounded fire-and-forget encode
    // tasks each hold thumbnail data → RSS balloons. If we're backed up, skip the
    // disk cache for this one (it stays in the RAM cache; a later visit re-decodes
    // or hits the shared cache) instead of piling up.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static PENDING: AtomicUsize = AtomicUsize::new(0);
    const MAX_PENDING: usize = 16;
    if PENDING.load(Ordering::Relaxed) >= MAX_PENDING {
        return;
    }
    PENDING.fetch_add(1, Ordering::Relaxed);

    let texture = texture.clone(); // gdk::Texture is Send
    let uri = uri.to_string();
    let roots = roots_for(shareable, removable, bucket);

    gio::spawn_blocking(move || {
        write_thumb(&texture, &uri, source_mtime, bucket, &roots);
        PENDING.fetch_sub(1, Ordering::Relaxed);
    });
}

// ---- Cleanup (Preferences → Clean Up Thumbnails…) ---------------------------
// The dialog lists every tier once (`survey`), optionally checks the URI-keyed
// tiers for thumbnails whose source is gone (`find_gone`), derives what the
// chosen options would remove (`plan_cleanup`), and only then deletes
// (`run_cleanup`). All of it is blocking file I/O, run off the main thread. The
// decisions are `vitrine_engine::thumb_cleanup`'s; this side gathers the facts.

/// A disk tier, as the cleanup dialog shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Private,
    Removable,
    Content,
    /// GNOME's shared cache — never cleaned by age, only of missing sources.
    Shared,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Private, Tier::Removable, Tier::Content, Tier::Shared];

    fn dir(self) -> PathBuf {
        match self {
            Tier::Private => private_dir(),
            Tier::Removable => removable_dir(),
            Tier::Content => content_dir(),
            Tier::Shared => shared_dir(),
        }
    }

    /// Vitrine's own tiers — the only ones the age rule touches.
    fn is_ours(self) -> bool {
        self != Tier::Shared
    }

    /// Keyed by source URI (so the missing-source rule applies). The content
    /// tier is keyed by hash and has no source to check.
    fn is_uri_keyed(self) -> bool {
        self != Tier::Content
    }
}

/// One thumbnail file on disk.
#[derive(Debug, Clone)]
pub struct CachedThumb {
    pub tier: Tier,
    pub path: PathBuf,
    pub size: u64,
    /// Last use: reads of our tiers bump it (see `ACCESS_TOUCH_AFTER`).
    pub mtime: i64,
}

/// Every thumbnail across the tiers, listed once when the dialog opens.
#[derive(Debug, Default)]
pub struct Survey {
    pub thumbs: Vec<CachedThumb>,
}

impl Survey {
    /// `(files, bytes)` held by `tier`.
    pub fn usage(&self, tier: Tier) -> (usize, u64) {
        self.thumbs
            .iter()
            .filter(|t| t.tier == tier)
            .fold((0, 0), |(n, b), t| (n + 1, b + t.size))
    }

    /// How many thumbnails `find_gone` would have to read.
    pub fn uri_keyed_count(&self) -> usize {
        self.thumbs.iter().filter(|t| t.tier.is_uri_keyed()).count()
    }
}

/// List every tier. Blocking. One of *our* tiers that turns out to be the
/// shared cache under another name (V-30) is left out, so nothing below can
/// apply our rules to GNOME's files.
pub fn survey() -> Survey {
    let mut thumbs = Vec::new();
    for tier in Tier::ALL {
        let dir = tier.dir();
        if tier.is_ours() && is_shared_cache(&dir) {
            continue;
        }
        for (path, size, mtime) in list_thumbs(&dir) {
            thumbs.push(CachedThumb {
                tier,
                path,
                size,
                mtime,
            });
        }
    }
    Survey { thumbs }
}

/// Where this process sees the real filesystem: `None` = everywhere (not
/// sandboxed). Under Flatpak only the granted locations are faithful — the
/// rest of `$HOME` and `/tmp` exist but are sandbox stand-ins, where a missing
/// file proves nothing.
fn source_view() -> Option<Vec<PathBuf>> {
    if !std::path::Path::new("/.flatpak-info").exists() {
        return None;
    }
    let mut roots = vec![glib::home_dir()
        .join(".var/app")
        .join(crate::config::APP_ID)];
    if let Some(pictures) = glib::user_special_dir(glib::UserDirectory::Pictures) {
        roots.push(pictures);
    }
    Some(roots)
}

/// How much of a thumbnail to read for its `Thumb::URI` chunk — thumbnailers
/// write their text chunks right after the header, well inside this.
const URI_PREFIX_BYTES: u64 = 8 * 1024;

/// The thumbnails in `paths` whose source is provably gone (see
/// `vitrine_engine::thumb_cleanup::source_gone`). Blocking: reads the head of
/// each file. Anything unreadable, non-`file://`, off-view, on removable
/// media, or under a missing/unreadable parent is kept.
pub fn find_gone(paths: Vec<PathBuf>) -> std::collections::HashSet<PathBuf> {
    use std::io::Read;
    use vitrine_engine::thumb_cleanup::{is_within, source_gone, SourceFacts};

    let view = source_view();
    let mut parents: std::collections::HashMap<PathBuf, bool> = Default::default();
    let mut gone = std::collections::HashSet::new();
    for path in paths {
        let mut head = Vec::new();
        let read = std::fs::File::open(&path)
            .and_then(|f| f.take(URI_PREFIX_BYTES).read_to_end(&mut head));
        if read.is_err() {
            continue;
        }
        let Some(uri) = vitrine_engine::png_meta::read_text_chunk(&head, "Thumb::URI") else {
            continue;
        };
        let is_gone = source_gone(&uri, |source| {
            let in_view = !source.to_str().is_some_and(is_removable_path)
                && view.as_ref().is_none_or(|roots| is_within(source, roots));
            let parent_readable = in_view
                && source.parent().is_some_and(|parent| {
                    *parents
                        .entry(parent.to_path_buf())
                        .or_insert_with(|| std::fs::read_dir(parent).is_ok())
                });
            let file_absent = parent_readable
                && matches!(
                    std::fs::symlink_metadata(source),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                );
            SourceFacts {
                in_view,
                parent_readable,
                file_absent,
            }
        });
        if is_gone {
            gone.insert(path);
        }
    }
    gone
}

/// What a cleanup would remove: each file with the mtime it had when planned.
#[derive(Debug, Default, Clone)]
pub struct CleanupPlan {
    pub items: Vec<(PathBuf, i64)>,
    pub bytes: u64,
}

/// Apply the dialog's options to a survey. `gone` is `find_gone`'s result when
/// the missing-files option is on. Never selects a shared-cache file by age.
pub fn plan_cleanup(
    survey: &Survey,
    rule: vitrine_engine::thumb_cleanup::AgeRule,
    gone: Option<&std::collections::HashSet<PathBuf>>,
) -> CleanupPlan {
    let ours: Vec<usize> = (0..survey.thumbs.len())
        .filter(|&i| survey.thumbs[i].tier.is_ours())
        .collect();
    let facts: Vec<(u64, i64)> = ours
        .iter()
        .map(|&i| (survey.thumbs[i].size, survey.thumbs[i].mtime))
        .collect();
    let mut chosen = vec![false; survey.thumbs.len()];
    for k in vitrine_engine::thumb_cleanup::select_unused(&facts, now_secs(), rule) {
        chosen[ours[k]] = true;
    }
    if let Some(gone) = gone {
        for (i, thumb) in survey.thumbs.iter().enumerate() {
            if thumb.tier.is_uri_keyed() && gone.contains(&thumb.path) {
                chosen[i] = true;
            }
        }
    }
    let mut plan = CleanupPlan::default();
    for (thumb, _) in survey.thumbs.iter().zip(&chosen).filter(|(_, &c)| c) {
        plan.items.push((thumb.path.clone(), thumb.mtime));
        plan.bytes += thumb.size;
    }
    plan
}

/// Delete what `plan` lists. Blocking. A file whose mtime moved since the plan
/// (read — so used — or rewritten in the meantime) is skipped. Returns
/// `(files, bytes)` actually removed.
pub fn run_cleanup(plan: CleanupPlan) -> (usize, u64) {
    let mut removed = (0, 0);
    for (path, planned_mtime) in plan.items {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if mtime_secs(&meta) != planned_mtime {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed.0 += 1;
            removed.1 += meta.len();
        }
    }
    if crate::debug::enabled() {
        eprintln!(
            "VDBG-CLEANUP ms={} removed={} bytes={}",
            crate::debug::since_start_ms(),
            removed.0,
            removed.1
        );
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The removable/remote heuristic (PLAN §16.5, Flavor 1): network schemes
    /// and typical auto-mount roots are "removable"; local + portal paths are not.
    #[test]
    fn removable_uri_classification() {
        for u in [
            "smb://nas/photos/x.jpg",
            "sftp://host/x.jpg",
            "mtp://phone/DCIM/x.jpg",
            "google-drive://acct/x",
            "file:///run/media/me/USB/x.jpg",
            "file:///media/usb/x.jpg",
            "file:///mnt/backup/x.jpg",
        ] {
            assert!(is_removable_uri(u), "{u} should be removable");
        }
        for u in [
            "file:///home/me/Pictures/x.jpg",
            "file:///run/user/1000/doc/abcd/x.jpg", // portal path — resolved separately
            "resource:///icons/x.png",
        ] {
            assert!(!is_removable_uri(u), "{u} should NOT be removable");
        }
    }

    /// Document-portal URIs are recognised so their real host path can be
    /// resolved (via xattr) and classified — vs. plain local/removable paths.
    #[test]
    fn doc_portal_uri_detection() {
        assert!(is_doc_portal_uri(
            "file:///run/user/1000/doc/16ecde37/photo.jpg"
        ));
        assert!(is_doc_portal_uri(
            "file:///run/user/1000/doc/abc/sub/photo.jpg"
        ));
        assert!(!is_doc_portal_uri("file:///run/media/me/USB/photo.jpg"));
        assert!(!is_doc_portal_uri("file:///home/me/x.jpg"));
        assert!(!is_doc_portal_uri("file:///run/user/1000/other/x.jpg"));
    }

    /// Removable decodes route to their own persistent dir (so local browsing's
    /// LRU prune can't evict them); local decodes stay in the private cache.
    #[test]
    fn roots_route_removable_to_its_own_dir() {
        let large = ThumbBucket::Large;
        let rem = roots_for(false, true, large);
        assert_eq!(rem, vec![removable_dir()]);

        let local = roots_for(false, false, large);
        assert_eq!(local, vec![private_dir()]);

        // Shareable (real host path) removable also writes the shared cache.
        assert_eq!(
            roots_for(true, true, large),
            vec![removable_dir(), shared_dir()]
        );
    }

    /// V-30: only GNOME's standard sizes go to its shared cache, and a local
    /// thumbnail is written exactly once.
    #[test]
    fn shared_cache_gets_standard_sizes_only() {
        for bucket in [ThumbBucket::Normal, ThumbBucket::Large] {
            assert_eq!(roots_for(true, false, bucket), vec![shared_dir()]);
        }
        for bucket in [ThumbBucket::XLarge, ThumbBucket::XxLarge] {
            assert_eq!(roots_for(true, false, bucket), vec![private_dir()]);
            assert_eq!(roots_for(true, true, bucket), vec![removable_dir()]);
        }
    }

    /// The private dir is never the name the Flatpak grant shadows.
    #[test]
    fn private_dir_is_not_the_granted_name() {
        assert_ne!(private_dir(), glib::user_cache_dir().join("thumbnails"));
        assert_ne!(private_dir(), shared_dir());
    }

    /// The content-tier access touch uses `set_modified` on the fd it read
    /// from; that must work on a read-only open of a read-only file we own.
    #[test]
    fn touch_works_on_a_read_only_fd() {
        let path = std::env::temp_dir().join(format!("vitrine-touch-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now()).unwrap();
        assert!(mtime_secs(&std::fs::metadata(&path).unwrap()) > 1_000_000);
        std::fs::remove_file(&path).ok();
    }

    fn thumb(tier: Tier, name: &str, size: u64, mtime: i64) -> CachedThumb {
        CachedThumb {
            tier,
            path: PathBuf::from(name),
            size,
            mtime,
        }
    }

    /// The age rule never reaches GNOME's cache; the missing rule never
    /// reaches the content tier.
    #[test]
    fn plan_respects_tier_rules() {
        use vitrine_engine::thumb_cleanup::AgeRule;
        let survey = Survey {
            thumbs: vec![
                thumb(Tier::Private, "p", 1, 0),
                thumb(Tier::Removable, "r", 2, 0),
                thumb(Tier::Content, "c", 4, 0),
                thumb(Tier::Shared, "s", 8, 0),
                thumb(Tier::Private, "fresh", 16, now_secs()),
            ],
        };
        let plan = plan_cleanup(&survey, AgeRule::All, None);
        assert_eq!(plan.items.len(), 4); // p r c fresh — never s
        assert_eq!(plan.bytes, 1 + 2 + 4 + 16);

        let plan = plan_cleanup(&survey, AgeRule::Days(30), None);
        assert_eq!(plan.bytes, 1 + 2 + 4);

        let gone: std::collections::HashSet<PathBuf> =
            ["s", "c", "fresh"].into_iter().map(PathBuf::from).collect();
        let plan = plan_cleanup(&survey, AgeRule::Off, Some(&gone));
        assert_eq!(plan.bytes, 8 + 16); // c is hash-keyed: not by missing
    }

    /// Only provably-gone sources are found: present files, non-file URIs and
    /// files under a missing parent are kept.
    #[test]
    fn find_gone_on_a_temp_tree() {
        let tmp = std::env::temp_dir().join(format!("vitrine-gone-{}", std::process::id()));
        let src = tmp.join("src");
        let cache = tmp.join("cache");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(src.join("here.jpg"), b"x").unwrap();
        let png = |uri: &str, name: &str| {
            let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
            bytes.extend_from_slice(&13u32.to_be_bytes());
            bytes.extend_from_slice(b"IHDR");
            bytes.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
            bytes.extend_from_slice(&0u32.to_be_bytes());
            let bytes =
                vitrine_engine::png_meta::add_text_chunks(&bytes, &[("Thumb::URI", uri)]).unwrap();
            let path = cache.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let uri = |p: &std::path::Path| format!("file://{}", p.display());
        let here = png(&uri(&src.join("here.jpg")), "1.png");
        let gone = png(&uri(&src.join("gone.jpg")), "2.png");
        let offline = png(&uri(&tmp.join("unplugged/x.jpg")), "3.png");
        let remote = png("smb://nas/x.jpg", "4.png");
        let found = find_gone(vec![here, gone.clone(), offline, remote]);
        // Under Flatpak the temp dir is outside the source view: nothing goes.
        if source_view().is_none() {
            assert_eq!(found, [gone].into_iter().collect());
        } else {
            assert!(found.is_empty());
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The shared-cache guard sees through a second name for the same dir
    /// (a symlink here, standing in for the Flatpak bind mount).
    #[test]
    fn shared_cache_guard_is_by_inode() {
        let tmp = std::env::temp_dir().join(format!("vitrine-v30-{}", std::process::id()));
        let real = tmp.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = tmp.join("alias");
        let _ = std::os::unix::fs::symlink(&real, &alias);
        assert!(same_dir(&real, &alias));
        assert!(!same_dir(&real, &tmp));
        assert!(!same_dir(&real, &tmp.join("absent")));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
