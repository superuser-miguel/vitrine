//! Image histogram — a live luminance + RGB readout over the texture the viewer
//! already has in hand. Core, not an extension: it reads pixels Vitrine has
//! already decoded, so it costs a bounded off-thread tally and a cheap Cairo
//! draw, with no ImageMagick round-trip (PLAN §16.5, "stays in core").
//!
//! Performance shape (the project North Star): the tally runs on a worker via
//! `gio::spawn_blocking` — never the main loop — over the viewer's *downscaled*
//! texture, and is capped by subsampling so a 24-megapixel source and a
//! preview-sized one cost the same. Bins are cached per texture-cache key by
//! the caller, so re-viewing an image is a map lookup, not a re-tally.

use gtk::{gdk, gio, prelude::*};

/// One bin per 8-bit level.
pub const BINS: usize = 256;

/// Sub-sampling budget: tally at most ~this many pixels regardless of texture
/// size. A histogram is statistical, so a stride-sampled subset is visually
/// identical to a full walk at a fraction of the cost.
const SAMPLE_BUDGET: usize = 400_000;

/// Rec. 709 luma weights, matching how the eye reads brightness.
const LUMA_R: f32 = 0.2126;
const LUMA_G: f32 = 0.7152;
const LUMA_B: f32 = 0.0722;

/// Per-channel 256-bin counts plus a luminance curve.
#[derive(Clone)]
pub struct Histogram {
    pub r: [u32; BINS],
    pub g: [u32; BINS],
    pub b: [u32; BINS],
    pub luma: [u32; BINS],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            r: [0; BINS],
            g: [0; BINS],
            b: [0; BINS],
            luma: [0; BINS],
        }
    }
}

/// Tally `texture` into a histogram on a worker thread. Mirrors the
/// `thumbnails::downscale_cpu` idiom: move the (Send) texture into
/// `spawn_blocking`, download R8G8B8A8 bytes there, and hand back a plain value.
pub async fn compute(texture: gdk::Texture) -> Histogram {
    gio::spawn_blocking(move || {
        let w = texture.width() as usize;
        let h = texture.height() as usize;
        if w == 0 || h == 0 {
            return Histogram::default();
        }
        let (bytes, stride) = {
            let mut d = gdk::TextureDownloader::new(&texture);
            d.set_format(gdk::MemoryFormat::R8g8b8a8);
            d.download_bytes()
        };
        drop(texture);

        // Stride large images down to the sample budget (√ so both axes scale).
        let total = w * h;
        let step = ((total as f64 / SAMPLE_BUDGET as f64).sqrt().ceil() as usize).max(1);

        let mut hist = Histogram::default();
        let mut y = 0;
        while y < h {
            let row = y * stride;
            let mut x = 0;
            while x < w {
                let p = row + x * 4;
                let r = bytes[p] as usize;
                let g = bytes[p + 1] as usize;
                let b = bytes[p + 2] as usize;
                hist.r[r] += 1;
                hist.g[g] += 1;
                hist.b[b] += 1;
                let l = (LUMA_R * r as f32 + LUMA_G * g as f32 + LUMA_B * b as f32) as usize;
                hist.luma[l.min(BINS - 1)] += 1;
                x += step;
            }
            y += step;
        }
        hist
    })
    .await
    .unwrap_or_default()
}

/// Paint the histogram onto a Cairo context sized `w`×`h`. Log-scaled counts
/// (a few dominant tones otherwise flatten everything else); translucent
/// additive R/G/B mountains with a crisp luma outline on top — the
/// darktable/gThumb-standard read.
pub fn draw(hist: &Histogram, cr: &gtk::cairo::Context, w: f64, h: f64) {
    if w <= 1.0 || h <= 1.0 {
        return;
    }

    // Scale to the tallest bin across every channel, in log space.
    let peak = hist
        .r
        .iter()
        .chain(hist.g.iter())
        .chain(hist.b.iter())
        .chain(hist.luma.iter())
        .copied()
        .max()
        .unwrap_or(0);
    let max_log = ((peak as f64) + 1.0).ln();
    if max_log <= 0.0 {
        return;
    }

    // Faint baseline so an empty panel still reads as "a histogram".
    cr.set_source_rgba(0.5, 0.5, 0.55, 0.25);
    cr.set_line_width(1.0);
    cr.move_to(0.0, h - 0.5);
    cr.line_to(w, h - 0.5);
    let _ = cr.stroke();

    // Translucent additive channels: overlaps brighten toward white/grey the way
    // a real RGB histogram does, rather than the last-drawn channel winning.
    cr.set_operator(gtk::cairo::Operator::Screen);
    for (bins, (cr_, cg, cb)) in [
        (&hist.r, (0.92, 0.24, 0.30)),
        (&hist.g, (0.30, 0.80, 0.38)),
        (&hist.b, (0.28, 0.55, 0.98)),
    ] {
        fill_path(cr, w, h, bins, max_log);
        cr.set_source_rgba(cr_, cg, cb, 0.55);
        let _ = cr.fill();
    }
    cr.set_operator(gtk::cairo::Operator::Over);

    // Luminance as a crisp outline over the colour, not a fourth mountain.
    stroke_path(cr, w, h, &hist.luma, max_log);
    cr.set_source_rgba(0.88, 0.88, 0.92, 0.9);
    cr.set_line_width(1.2);
    let _ = cr.stroke();
}

/// A closed area path under the log-scaled curve for one channel.
fn fill_path(cr: &gtk::cairo::Context, w: f64, h: f64, bins: &[u32; BINS], max_log: f64) {
    let bw = w / BINS as f64;
    cr.move_to(0.0, h);
    for (i, &c) in bins.iter().enumerate() {
        let v = ((c as f64) + 1.0).ln() / max_log;
        cr.line_to(i as f64 * bw + bw * 0.5, h - v * h);
    }
    cr.line_to(w, h);
    cr.close_path();
}

/// The open curve (no baseline) for a stroked channel.
fn stroke_path(cr: &gtk::cairo::Context, w: f64, h: f64, bins: &[u32; BINS], max_log: f64) {
    let bw = w / BINS as f64;
    for (i, &c) in bins.iter().enumerate() {
        let v = ((c as f64) + 1.0).ln() / max_log;
        let x = i as f64 * bw + bw * 0.5;
        let y = h - v * h;
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
}
