//! glycin orchestration: sandboxed subprocess-per-image decode.
//!
//! glycin already runs decoders as sandboxed subprocesses, in parallel, sharing
//! a warm global `Pool` (`Loader`'s default), and applies EXIF orientation by
//! default (`apply_transformations: true`). So the whole decode workload — CPU
//! and untrusted parsing — happens off our main thread and out of our process;
//! here we only `await` the result on the GLib main context. That is what keeps
//! the grid's main loop free (PLAN Phase 1 acceptance: no decode on the main
//! thread).
//!
//! AVIF / JXL / HEIF are first-class: they are in glycin's advertised MIME set
//! and decode through the same path as JPEG.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gtk::gdk;
use gtk::gio;
use gtk::glib;

use glycin::{FrameRequest, Loader};

/// Interactive (thumbnail / viewer) decodes currently in flight — including
/// those still waiting for a `decode_gate` permit. Background enrichment yields
/// while this is non-zero (or recently was), so the foreground never queues
/// behind the decode-everything pass. See [`yield_to_foreground`].
static INTERACTIVE_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Millis (since process start) of the last interactive decode start/finish.
static LAST_INTERACTIVE_MS: AtomicU64 = AtomicU64::new(0);

/// Millis since an arbitrary fixed start — a cheap monotonic clock for the
/// foreground-activity timestamp (no wall-clock, no allocation).
fn now_millis() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// RAII marker for an in-flight interactive decode: bumps the counter and stamps
/// the activity clock on both creation and drop, so enrichment yields for the
/// whole decode *and* for a grace period after it finishes.
struct InteractiveGuard;

impl InteractiveGuard {
    fn new() -> InteractiveGuard {
        INTERACTIVE_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        LAST_INTERACTIVE_MS.store(now_millis(), Ordering::Relaxed);
        InteractiveGuard
    }
}

impl Drop for InteractiveGuard {
    fn drop(&mut self) {
        INTERACTIVE_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
        LAST_INTERACTIVE_MS.store(now_millis(), Ordering::Relaxed);
    }
}

/// Park until the interactive foreground is idle: no interactive decodes in
/// flight and none for a short grace period (so enrichment doesn't thrash the
/// gate in the gaps between scroll settles). This is what makes background
/// enrichment a good citizen — it runs full-tilt when you're not browsing and
/// steps aside the moment you are.
pub async fn yield_to_foreground() {
    /// Quiet window required before enrichment resumes grabbing the gate.
    const GRACE_MS: u64 = 150;
    loop {
        let inflight = INTERACTIVE_INFLIGHT.load(Ordering::Relaxed);
        let quiet_for = now_millis().saturating_sub(LAST_INTERACTIVE_MS.load(Ordering::Relaxed));
        if inflight == 0 && quiet_for >= GRACE_MS {
            return;
        }
        glib::timeout_future(Duration::from_millis(50)).await;
    }
}

/// Cap on concurrent glycin decodes. glycin's pool is unbounded
/// (`max_parallel_operations = usize::MAX`), so a grid that fans out one decode
/// per visible cell spawns a burst of sandboxed loader subprocesses at once —
/// which on the single-threaded GLib main loop starves and *no* decode ever
/// finishes (measured: 0 completions for an ~800-image folder; a blank grid).
///
/// The heavy work is already parallel — each decode runs in its own subprocess
/// across all cores — so this only gates the *coordinator*. Measured throughput
/// on a 12-core box plateaus by ~8 concurrent (1→200, 4→482, 8→513, 20→513,
/// unbounded→0 completions/12s): past core-count, more admissions add coordinator
/// churn without decoding faster. So the default tracks core count, capped at 8,
/// floored at 4. Override with `VITRINE_DECODE_LIMIT`.
fn decode_gate() -> &'static async_lock::Semaphore {
    static GATE: OnceLock<async_lock::Semaphore> = OnceLock::new();
    GATE.get_or_init(|| {
        let limit = std::env::var("VITRINE_DECODE_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .clamp(4, 8)
            });
        async_lock::Semaphore::new(limit)
    })
}

/// Decode `file` at thumbnail resolution: a frame scaled to fit within
/// `size`×`size` (aspect preserved by the loader). EXIF orientation is applied.
pub async fn thumbnail(file: &gio::File, size: u32) -> Result<gdk::Texture, glycin::ErrorCtx> {
    let _foreground = InteractiveGuard::new();
    let _permit = decode_gate().acquire().await;
    let image = Loader::new(file.clone()).load().await?;
    let frame = image
        .specific_frame(FrameRequest::new().scale(size, size))
        .await?;
    Ok(frame.texture())
}

/// Decode `file` for the viewer, capping the longest edge at `max_dim` so a
/// single displayed image stays a bounded texture (the LRU cache and zoom work
/// against this). Requests a scaled frame when the source is larger; the caller
/// still defensively downscales, since the scale hint is best-effort.
pub async fn full(file: &gio::File, max_dim: u32) -> Result<gdk::Texture, glycin::ErrorCtx> {
    let _foreground = InteractiveGuard::new();
    let _permit = decode_gate().acquire().await;
    let image = Loader::new(file.clone()).load().await?;
    let details = image.details();
    let frame = if details.width().max(details.height()) > max_dim {
        image
            .specific_frame(FrameRequest::new().scale(max_dim, max_dim))
            .await?
    } else {
        image.next_frame().await?
    };
    Ok(frame.texture())
}

/// Everything the background enrichment pass needs from one gated decode:
/// original dimensions, the raw EXIF blob (parsed by the engine, pure), and a
/// small frame whose pixels feed the perceptual hash.
pub struct Probe {
    pub width: u32,
    pub height: u32,
    pub format: Option<String>,
    pub exif: Option<Vec<u8>>,
    pub frame: gdk::Texture,
}

/// Decode `file` once for indexing: read its metadata and a `phash_px`-scaled
/// frame (perceptual hashing only needs a small image). The enrichment driver
/// calls [`yield_to_foreground`] once per batch so the background pass never
/// queues the UI behind it.
pub async fn probe(file: &gio::File, phash_px: u32) -> Option<Probe> {
    let _permit = decode_gate().acquire().await;
    let image = Loader::new(file.clone()).load().await.ok()?;
    let details = image.details();
    let width = details.width();
    let height = details.height();
    let format = details.info_format_name().map(str::to_string);
    let exif = details.metadata_exif().and_then(|b| b.get_full().ok());
    let frame = image
        .specific_frame(FrameRequest::new().scale(phash_px, phash_px))
        .await
        .ok()?;
    Some(Probe {
        width,
        height,
        format,
        exif,
        frame: frame.texture(),
    })
}

/// The MIME types Vitrine treats as browsable images — glycin's advertised set
/// (includes AVIF, JXL, HEIF). Used to filter folder contents into the grid.
pub fn is_supported_image(content_type: &str) -> bool {
    // `content_type` from Gio may be a MIME type already, or a fallback like
    // "application/octet-stream"; match against glycin's default MIME list.
    Loader::DEFAULT_MIME_TYPES.contains(&content_type)
}
