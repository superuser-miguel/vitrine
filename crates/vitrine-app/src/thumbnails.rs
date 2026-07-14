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

/// Byte budget for the app-private disk cache (the higher-res buckets GNOME
/// doesn't generate). Pruned to this on startup, evicting least-recently-used.
const PRIVATE_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
        return Some(texture);
    }
    // Private cache is ours — mark access so eviction is LRU, not FIFO.
    if let Some(texture) = read_cache(private_dir(), &uri, bucket, source_mtime, true).await {
        return Some(texture);
    }

    let texture = match crate::decode::thumbnail(&file, bucket.pixels()).await {
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
            store(&uri, source_mtime, bucket, &thumb, is_shareable(&file)).await;
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
    let file = gio::File::for_path(path);

    let info = file
        .query_info_future(
            "time::modified",
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await
        .ok()?;
    let cache_mtime = info.attribute_uint64("time::modified") as i64;
    if !thumbnail_cache::is_current(cache_mtime, source_mtime) {
        return None;
    }

    let (bytes, _etag) = file.load_bytes_future().await.ok()?;
    let texture = gdk::Texture::from_bytes(&bytes).ok()?;

    // Record access for LRU eviction, throttled so scrolling isn't write-heavy.
    if touch {
        let now = now_secs();
        if cache_mtime < now - ACCESS_TOUCH_AFTER {
            file.set_attribute_uint64(
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

/// Prune the app-private disk cache to [`PRIVATE_CACHE_BYTES`], evicting the
/// least-recently-used files. Runs off the main thread; best-effort.
pub fn prune_private_cache() {
    // Cap override (MB) — a hook for future user-facing cache controls.
    let cap = std::env::var("VITRINE_CACHE_CAP_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(PRIVATE_CACHE_BYTES);
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
/// `Thumb::URI`/`Thumb::MTime` metadata so any thumbnailer can validate it.
/// Always writes the app-private cache; also the shared cache when `shareable`
/// (real-path files), contributing thumbnails back to Nautilus/GNOME.
/// Best-effort — failures are ignored.
async fn store(
    uri: &str,
    source_mtime: i64,
    bucket: ThumbBucket,
    texture: &gdk::Texture,
    shareable: bool,
) {
    let png = texture.save_to_png_bytes();
    let mtime = source_mtime.to_string();
    let png = vitrine_engine::png_meta::add_text_chunks(
        &png,
        &[("Thumb::URI", uri), ("Thumb::MTime", &mtime)],
    )
    .unwrap_or_else(|| png.to_vec());

    let mut roots = vec![private_dir()];
    let shared = shared_dir();
    if shareable && shared != private_dir() {
        roots.push(shared);
    }

    let rel = format!("{}.png", thumbnail_cache::cache_key(uri));
    for root in roots {
        let dir = root.join(bucket.dir());
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let file = gio::File::for_path(dir.join(&rel));
        let _ = file
            .replace_contents_future(
                png.clone(),
                None,
                false,
                gio::FileCreateFlags::REPLACE_DESTINATION,
            )
            .await;
    }
}

/// Downscale `texture` so its longest edge is at most `max` px, preserving
/// aspect. Returns the input unchanged if it already fits. Rendered on
/// `renderer`, so the result is a compact GPU texture.
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
    renderer.render_texture(node, Some(&bounds))
}
