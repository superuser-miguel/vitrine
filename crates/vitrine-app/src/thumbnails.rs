//! Thumbnail loading: shared freedesktop cache first, glycin on a miss.
//!
//! Load order (PLAN task 3), fastest first:
//!  1. **Shared cache** (`~/.cache/thumbnails/…`). Inside Flatpak the
//!     `xdg-cache/thumbnails` grant maps this to the host's cache, so we reuse
//!     the thumbnails Nautilus/GNOME already generated — no decode at all.
//!  2. **App-private cache** (`$XDG_CACHE_HOME/thumbnails/…`) — where our own
//!     decodes are stored.
//!  3. **glycin decode** (concurrency-gated) + CPU downscale on a worker thread, then written to
//!     the app-private cache.
//!
//! **§4 RISK, resolved:** cache keys are the MD5 of the file *URI*. Real paths
//! (e.g. under `xdg-pictures`) present the same URI inside the sandbox as on the
//! host, so shared-cache hits work. Document-portal paths do not match the host
//! URI, so we only ever *read* the shared cache (a harmless miss for those) and
//! *write* to the app-private cache — never polluting the shared cache with keys
//! the host can't reproduce.

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

/// Our private thumbnail cache (app-scoped under Flatpak; = shared on the host).
fn private_dir() -> PathBuf {
    glib::user_cache_dir().join("thumbnails")
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
        "smb://", "sftp://", "ftp://", "dav://", "davs://", "nfs://", "mtp://",
        "gphoto2://", "afp://", "google-drive://",
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
pub async fn read_content(hash: &str, target_px: u32) -> Option<gdk::Texture> {
    if !content_enabled() || hash.is_empty() {
        return None;
    }
    let path = content_path(hash, ThumbBucket::for_target(target_px));
    let tex = gio::spawn_blocking(move || {
        let bytes = std::fs::read(&path).ok()?;
        gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()
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
    prune_dir(content_dir(), content_budget_mb().saturating_mul(1024 * 1024));
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
            store(&uri, source_mtime, bucket, &thumb, is_shareable(&file), removable);
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
    let roots = roots_for(is_shareable(file), removable);
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
        let mut files: Vec<PathBuf> = Vec::new();
        let mut facts: Vec<(u64, i64)> = Vec::new();
        for bucket in ["normal", "large", "x-large", "xx-large"] {
            let Ok(entries) = std::fs::read_dir(root.join(bucket)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                files.push(entry.path());
                facts.push((meta.len(), mtime));
            }
        }
        for i in vitrine_engine::cache_evict::evict_lru(&facts, cap) {
            let _ = std::fs::remove_file(&files[i]);
        }
    });
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
    prune_dir(removable_dir(), removable_budget_mb().saturating_mul(1024 * 1024));
}

/// Write `texture` to the thumbnail cache(s), tagged with the freedesktop
/// `Thumb::URI`/`Thumb::MTime` metadata. Fire-and-forget: PNG **encode and disk
/// write happen on a worker thread** (both are pure CPU/IO and were a major
/// main-loop stall while populating). Always writes the app-private cache; also
/// the shared cache when `shareable` (real-path files, contributing to Nautilus).
/// The cache roots a thumbnail is written to: always the app-private cache, plus
/// the shared freedesktop cache when the file is shareable (a real host path).
fn roots_for(shareable: bool, removable: bool) -> Vec<PathBuf> {
    // Removable/remote decodes go to the persistent removable tier; everything
    // else to the LRU-pruned private cache. Either may also write the shared
    // freedesktop cache (Nautilus interop) when the path is a real host path.
    let primary = if removable { removable_dir() } else { private_dir() };
    let shared = shared_dir();
    let mut roots = vec![primary.clone()];
    if shareable && shared != primary {
        roots.push(shared);
    }
    roots
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
    let roots = roots_for(shareable, removable);

    gio::spawn_blocking(move || {
        write_thumb(&texture, &uri, source_mtime, bucket, &roots);
        PENDING.fetch_sub(1, Ordering::Relaxed);
    });
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
        assert!(is_doc_portal_uri("file:///run/user/1000/doc/16ecde37/photo.jpg"));
        assert!(is_doc_portal_uri("file:///run/user/1000/doc/abc/sub/photo.jpg"));
        assert!(!is_doc_portal_uri("file:///run/media/me/USB/photo.jpg"));
        assert!(!is_doc_portal_uri("file:///home/me/x.jpg"));
        assert!(!is_doc_portal_uri("file:///run/user/1000/other/x.jpg"));
    }

    /// Removable decodes route to their own persistent dir (so local browsing's
    /// LRU prune can't evict them); local decodes stay in the private cache.
    #[test]
    fn roots_route_removable_to_its_own_dir() {
        let rem = roots_for(false, true);
        assert_eq!(rem[0], removable_dir());
        assert!(!rem.contains(&private_dir()));

        let local = roots_for(false, false);
        assert_eq!(local[0], private_dir());
        assert!(!local.contains(&removable_dir()));

        // Shareable (real host path) also writes the shared freedesktop cache.
        assert!(roots_for(true, true).contains(&shared_dir()));
    }
}
