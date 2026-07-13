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

use gtk::gdk;
use gtk::gio;

use glycin::{FrameRequest, Loader};

/// Decode `file` at thumbnail resolution: a frame scaled to fit within
/// `size`×`size` (aspect preserved by the loader). EXIF orientation is applied.
pub async fn thumbnail(file: &gio::File, size: u32) -> Result<gdk::Texture, glycin::ErrorCtx> {
    let image = Loader::new(file.clone()).load().await?;
    let frame = image
        .specific_frame(FrameRequest::new().scale(size, size))
        .await?;
    Ok(frame.texture())
}

/// The MIME types Vitrine treats as browsable images — glycin's advertised set
/// (includes AVIF, JXL, HEIF). Used to filter folder contents into the grid.
pub fn is_supported_image(content_type: &str) -> bool {
    // `content_type` from Gio may be a MIME type already, or a fallback like
    // "application/octet-stream"; match against glycin's default MIME list.
    Loader::DEFAULT_MIME_TYPES.contains(&content_type)
}
