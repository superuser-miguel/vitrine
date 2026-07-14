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
/// which on the single-threaded GLib main loop starves/stalls and no decode ever
/// finishes (observed: hundreds of images → a blank grid). Gating admission to a
/// small number keeps loaders warm and the pipeline flowing. Override with
/// `VITRINE_DECODE_LIMIT` for experiments.
fn decode_gate() -> &'static async_lock::Semaphore {
    static GATE: OnceLock<async_lock::Semaphore> = OnceLock::new();
    GATE.get_or_init(|| {
        let limit = std::env::var("VITRINE_DECODE_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(4);
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

/// The MIME types Vitrine treats as browsable images — glycin's advertised set
/// (includes AVIF, JXL, HEIF). Used to filter folder contents into the grid.
pub fn is_supported_image(content_type: &str) -> bool {
    // `content_type` from Gio may be a MIME type already, or a fallback like
    // "application/octet-stream"; match against glycin's default MIME list.
    Loader::DEFAULT_MIME_TYPES.contains(&content_type)
}
