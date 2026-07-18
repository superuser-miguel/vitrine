//! Loupe-level lossless image transforms via glycin's `Editor`.
//!
//! Rotate/flip are applied *in place*. For JPEG the change is **sparse** — glycin
//! rewrites only a few EXIF-orientation bytes, so the pixel data is never touched
//! and there's no re-encode quality loss. Formats that can't express the change in
//! metadata (e.g. PNG) come back as a full, still-lossless rewrite. glycin auto-
//! applies EXIF orientation on decode, so once the file is edited a fresh decode
//! shows the new orientation with no other bookkeeping.
//!
//! The runtime (`org.gnome.Platform//50`) ships editors for jpeg/png/gif/avif/
//! heif/jxl; unsupported formats surface as an `Err` the caller can toast.

use glycin::{EditOutcome, Editor, Operation, Operations, SparseEdit};
use gtk::gio;
use gtk::prelude::FileExt;
use gufo_common::orientation::Rotation;

/// A lossless transform the viewer can apply to the current image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    RotateLeft,
    RotateRight,
    FlipHorizontal,
    FlipVertical,
}

impl Transform {
    fn operation(self) -> Operation {
        match self {
            // glycin's `Rotate` is *counter-clockwise*: 90° CCW = rotate-left,
            // 270° CCW = rotate-right.
            Transform::RotateLeft => Operation::Rotate(Rotation::_90),
            Transform::RotateRight => Operation::Rotate(Rotation::_270),
            Transform::FlipHorizontal => Operation::MirrorHorizontally,
            Transform::FlipVertical => Operation::MirrorVertically,
        }
    }

    /// Short past-tense verb for the confirmation toast.
    pub fn past_tense(self) -> &'static str {
        match self {
            Transform::RotateLeft | Transform::RotateRight => "Rotated",
            Transform::FlipHorizontal | Transform::FlipVertical => "Flipped",
        }
    }
}

/// Apply `transform` to `file` in place, losslessly. `Ok(true)` if the file was
/// changed, `Ok(false)` if the editor reported no change; `Err` carries a message
/// suitable for a toast (e.g. an unsupported format).
pub async fn apply(file: gio::File, transform: Transform) -> Result<bool, String> {
    let ops = Operations::new(vec![transform.operation()]);
    let editable = Editor::new(file.clone())
        .edit()
        .await
        .map_err(|e| e.to_string())?;
    let edit = editable
        .apply_sparse(&ops)
        .await
        .map_err(|e| e.to_string())?;
    match &edit {
        // Sparse: glycin patches a handful of bytes in the existing file.
        SparseEdit::Sparse(_) => {
            let outcome = edit.apply_to(file).await.map_err(|e| e.to_string())?;
            Ok(outcome == EditOutcome::Changed)
        }
        // Complete: the format needs a full (still lossless) rewrite.
        SparseEdit::Complete(data) => {
            let bytes = data.get_full().map_err(|e| e.to_string())?;
            let path = file.path().ok_or("image has no local path to write")?;
            gio::spawn_blocking(move || std::fs::write(&path, &bytes))
                .await
                .map_err(|_| "write task failed".to_string())?
                .map_err(|e| e.to_string())?;
            Ok(true)
        }
    }
}
