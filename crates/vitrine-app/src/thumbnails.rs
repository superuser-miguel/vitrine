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
use gtk::graphene;
use gtk::gsk;
use gtk::prelude::*;

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
