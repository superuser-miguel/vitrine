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

use std::sync::OnceLock;

use gtk::gdk;
use gtk::gio;

use glycin::{FrameRequest, Loader};

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
    pub exif: Option<Vec<u8>>,
    pub frame: gdk::Texture,
}

/// Decode `file` once for indexing: read its metadata and a `phash_px`-scaled
/// frame (perceptual hashing only needs a small image). Shares the decode gate
/// with thumbnailing so background enrichment can't outrun the UI's decodes.
pub async fn probe(file: &gio::File, phash_px: u32) -> Option<Probe> {
    let _permit = decode_gate().acquire().await;
    let image = Loader::new(file.clone()).load().await.ok()?;
    let details = image.details();
    let width = details.width();
    let height = details.height();
    let exif = details.metadata_exif().and_then(|b| b.get_full().ok());
    let frame = image
        .specific_frame(FrameRequest::new().scale(phash_px, phash_px))
        .await
        .ok()?;
    Some(Probe {
        width,
        height,
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
