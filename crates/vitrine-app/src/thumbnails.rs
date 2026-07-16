//! Thumbnail loading: shared freedesktop cache first, glycin on a miss.
//!
//! Load order (PLAN task 3), fastest first:
//!  1. **Shared cache** (`~/.cache/thumbnails/…`). Inside Flatpak the
//!     `xdg-cache/thumbnails` grant maps this to the host's cache, so we reuse
//!     the thumbnails Nautilus/GNOME already generated — no decode at all.
//!  2. **App-private cache** (`$XDG_CACHE_HOME/thumbnails/…`) — where our own
//!     decodes are stored.
//!  3. **glycin decode** (concurrency-gated) + GPU downscale, then written to
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
use gtk::graphene;
use gtk::gsk;
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
    renderer_widget: glib::WeakRef<gtk::Widget>,
) -> Option<gdk::Texture> {
    let bucket = ThumbBucket::for_target(target_px);
    let uri = file.uri().to_string();

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

    crate::debug::cache_miss();
    crate::debug::decode_begin();
    let decoded = crate::decode::thumbnail(&file, bucket.pixels()).await;
    crate::debug::decode_end();
    let texture = match decoded {
        Ok(texture) => texture,
        Err(err) => {
            glib::g_warning!("vitrine", "thumbnail {uri}: {err}");
            return None;
        }
    };

    // glycin may return full resolution; shrink for cache + display. Only cache
    // when we actually downscaled — never store a multi-MB "thumbnail". Resolve
    // the renderer now that decoding is done and the cell is realized.
    let renderer = renderer_widget
        .upgrade()
        .and_then(|w| w.native())
        .and_then(|n| n.renderer());
    match renderer {
        Some(renderer) => {
            let thumb = downscale(&texture, bucket.pixels(), &renderer);
            store(&uri, source_mtime, bucket, &thumb, is_shareable(&file));
            Some(thumb)
        }
        None => Some(texture),
    }
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

/// Prune the app-private disk cache to the configured budget (see
/// [`crate::settings`]), evicting the least-recently-used files. Runs off the
/// main thread; best-effort.
pub fn prune_private_cache() {
    // Budget in MB: VITRINE_CACHE_CAP_MB (dev override) wins, else the user's
    // configured cache size from Preferences.
    let cap = std::env::var("VITRINE_CACHE_CAP_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| crate::settings::Settings::load().cache_mb())
        .saturating_mul(1024 * 1024);
    std::thread::spawn(move || {
        let root = private_dir();
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

/// Write `texture` to the thumbnail cache(s), tagged with the freedesktop
/// `Thumb::URI`/`Thumb::MTime` metadata. Fire-and-forget: PNG **encode and disk
/// write happen on a worker thread** (both are pure CPU/IO and were a major
/// main-loop stall while populating). Always writes the app-private cache; also
/// the shared cache when `shareable` (real-path files, contributing to Nautilus).
fn store(
    uri: &str,
    source_mtime: i64,
    bucket: ThumbBucket,
    texture: &gdk::Texture,
    shareable: bool,
) {
    let texture = texture.clone(); // gdk::Texture is Send
    let uri = uri.to_string();
    let mtime = source_mtime.to_string();
    let rel = format!("{}.png", thumbnail_cache::cache_key(&uri));
    let mut roots = vec![private_dir()];
    let shared = shared_dir();
    if shareable && shared != private_dir() {
        roots.push(shared);
    }

    gio::spawn_blocking(move || {
        let png = texture.save_to_png_bytes();
        let png = vitrine_engine::png_meta::add_text_chunks(
            &png,
            &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime)],
        )
        .unwrap_or_else(|| png.to_vec());
        for root in roots {
            let dir = root.join(bucket.dir());
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = std::fs::write(dir.join(&rel), &png);
            }
        }
    });
}

/// Downscale `texture` so its longest edge is at most `max` px, preserving
/// aspect. Returns the input unchanged if it already fits. Uses the GSK renderer
/// to scale, then copies the result into a **`MemoryTexture`**: a GPU texture is
/// backed by a dmabuf file descriptor, and thousands cached across folders leak
/// FDs (→ "too many open files"); memory textures hold no FD.
pub fn downscale(texture: &gdk::Texture, max: u32, renderer: &gsk::Renderer) -> gdk::Texture {
    let w = texture.width();
    let h = texture.height();
    let longest = w.max(h);
    if longest <= max as i32 {
        return texture.clone();
    }

    let scale = max as f32 / longest as f32;
    let nw = (w as f32 * scale).round().max(1.0);
    let nh = (h as f32 * scale).round().max(1.0);
    let bounds = graphene::Rect::new(0.0, 0.0, nw, nh);

    let node = gsk::TextureScaleNode::new(texture, &bounds, gsk::ScalingFilter::Trilinear);
    let gpu = renderer.render_texture(node, Some(&bounds));
    to_memory_texture(&gpu)
}

/// Copy a texture's pixels into a `MemoryTexture` (holds no GPU/dmabuf FD),
/// dropping the source GPU texture.
fn to_memory_texture(texture: &gdk::Texture) -> gdk::Texture {
    let mut downloader = gdk::TextureDownloader::new(texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    gdk::MemoryTextureBuilder::new()
        .set_bytes(Some(&bytes))
        .set_width(texture.width())
        .set_height(texture.height())
        .set_stride(stride)
        .set_format(gdk::MemoryFormat::R8g8b8a8)
        .build()
        .upcast()
}
