//! Thumbnail helpers.
//!
//! glycin's `FrameRequest::scale` is a best-effort hint that older loaders
//! ignore — a decode can hand back a full-resolution texture (e.g. 6000×4000 ≈
//! 96 MB). Holding that per grid cell is a memory bomb, so we defensively
//! **GPU-downscale** every decoded thumbnail to at most `max` px on its longest
//! edge and cache the small result, letting the big texture drop. The scale runs
//! on the widget's GSK renderer (GL/Vulkan) — off the CPU, on the main context.
//!
//! (The shared freedesktop thumbnail cache, which sidesteps decoding entirely on
//! a warm cache, layers on top of this in the next increment.)

use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::prelude::*;

use crate::image_object::ImageObject;

/// Ensure `item` has a thumbnail, decoding once if needed and caching it on the
/// item's `texture` property (so property bindings update every view showing it).
/// `widget` supplies the GSK renderer for the defensive downscale. A no-op if a
/// thumbnail already exists, a previous decode failed, or one is already running.
pub fn ensure_thumbnail(widget: &impl IsA<gtk::Widget>, item: &ImageObject, size: u32) {
    if item.texture().is_some() || item.has_failed() || !item.begin_load() {
        return;
    }
    let file = item.file();
    let weak_widget = widget.as_ref().downgrade();
    let item = item.clone();
    glib::spawn_future_local(async move {
        match crate::decode::thumbnail(&file, size).await {
            Ok(texture) => {
                let thumb = weak_widget
                    .upgrade()
                    .and_then(|w| w.native())
                    .and_then(|n| n.renderer())
                    .map(|r| downscale(&texture, size, &r))
                    .unwrap_or(texture);
                item.set_texture(Some(thumb));
            }
            Err(_) => item.mark_failed(),
        }
    });
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
