//! CPU thumbnail resize — pure pixel math (no GTK), so it runs on a worker
//! thread instead of the GSK GPU downscale, which had to run on the main thread
//! (and triggered a ~1.7 s GL shader compile on first use).
//!
//! Memory-frugal: it resizes from a **borrowed view** over the decoded RGBA
//! (handling row-stride padding directly), so it never copies the full-resolution
//! image — only the small thumbnail is allocated. That matters because many large
//! decodes can be resizing at once.

use image::flat::{FlatSamples, SampleLayout};
use image::{ColorType, Rgba};

/// Shrink an RGBA8 image so its longest edge is at most `max_edge`, preserving
/// aspect. `stride` is the source row length in bytes (may exceed `width * 4`
/// when the decoder pads rows). Returns `(tight RGBA8 bytes, out_width,
/// out_height)`, or `None` if the input is malformed. If it already fits, returns
/// the source repacked to tight rows, unscaled.
pub fn resize_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    max_edge: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let row = (width as usize) * 4;
    let stride = (stride as usize).max(row);
    if src.len() < stride * height as usize {
        return None;
    }

    let longest = width.max(height);
    if longest <= max_edge {
        return Some((to_tight_rgba(src, width, height, stride), width, height));
    }

    let scale = max_edge as f64 / longest as f64;
    let nw = ((width as f64 * scale).round() as u32).max(1);
    let nh = ((height as f64 * scale).round() as u32).max(1);

    // A borrowed view over the (possibly padded) source — no full-res copy.
    let layout = SampleLayout {
        channels: 4,
        channel_stride: 1,
        width,
        width_stride: 4,
        height,
        height_stride: stride,
    };
    let flat = FlatSamples {
        samples: src,
        layout,
        color_hint: Some(ColorType::Rgba8),
    };
    let view = flat.as_view::<Rgba<u8>>().ok()?;
    // `thumbnail` box-averages — good quality for large downscale ratios and fast.
    let thumb = image::imageops::thumbnail(&view, nw, nh);
    Some((thumb.into_raw(), nw, nh))
}

/// Copy `src` into tightly-packed RGBA rows, dropping any per-row stride padding.
fn to_tight_rgba(src: &[u8], width: u32, height: u32, stride: usize) -> Vec<u8> {
    let row = (width as usize) * 4;
    if stride == row {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        out.extend_from_slice(&src[start..start + row]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_to_max_edge_preserving_aspect() {
        let src = [255u8, 0, 0, 255].repeat(40 * 20); // 40x20 solid red
        let (out, w, h) = resize_rgba(&src, 40, 20, 40 * 4, 10).unwrap();
        assert_eq!((w, h), (10, 5));
        assert_eq!(out.len(), 10 * 5 * 4);
        assert!(out[0] > 200 && out[1] < 60, "still red-ish"); // averaged red
    }

    #[test]
    fn passthrough_when_already_small() {
        let src = [1u8, 2, 3, 4].repeat(8 * 8);
        let (out, w, h) = resize_rgba(&src, 8, 8, 8 * 4, 256).unwrap();
        assert_eq!((w, h), (8, 8));
        assert_eq!(out, src);
    }

    #[test]
    fn handles_row_stride_padding() {
        // 2x2, stride 12 (row = 8 bytes + 4 pad) → tight 2x2, padding dropped.
        let src = vec![
            10, 20, 30, 40, 50, 60, 70, 80, 0, 0, 0, 0, // row 0 + pad
            11, 21, 31, 41, 51, 61, 71, 81, 0, 0, 0, 0, // row 1 + pad
        ];
        let (out, w, h) = resize_rgba(&src, 2, 2, 12, 256).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            out,
            vec![10, 20, 30, 40, 50, 60, 70, 80, 11, 21, 31, 41, 51, 61, 71, 81]
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(resize_rgba(&[0u8; 4], 0, 0, 0, 10).is_none());
        assert!(resize_rgba(&[0u8; 4], 100, 100, 400, 10).is_none()); // too small
    }
}

/// Apply an EXIF orientation code (1–8) to tightly-or-padded RGBA8 pixels,
/// returning `(tight RGBA8 bytes, out_width, out_height)` — dims swap for
/// codes 5–8. Pure per-pixel remap (inverse mapping, no interpolation), so it
/// belongs on a worker thread next to `resize_rgba`. `None` on malformed input
/// or the identity code (callers skip work).
pub fn orient_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    orientation: i64,
) -> Option<(Vec<u8>, u32, u32)> {
    if !(2..=8).contains(&orientation) || width == 0 || height == 0 {
        return None;
    }
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    if stride < w * 4 || src.len() < stride * h {
        return None;
    }
    let swap = orientation >= 5;
    let (ow, oh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        for ox in 0..ow {
            let (sx, sy) = match orientation {
                2 => (w - 1 - ox, oy),
                3 => (w - 1 - ox, h - 1 - oy),
                4 => (ox, h - 1 - oy),
                5 => (oy, ox),
                6 => (oy, h - 1 - ox),
                7 => (w - 1 - oy, h - 1 - ox),
                _ => (w - 1 - oy, ox), // 8
            };
            let s = sy * stride + sx * 4;
            let d = (oy * ow + ox) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    Some((out, ow as u32, oh as u32))
}

#[cfg(test)]
mod orient_tests {
    use super::orient_rgba;

    // 2x1 image: pixel A then B (RGBA singles for brevity).
    const AB: [u8; 8] = [1, 1, 1, 1, 2, 2, 2, 2];

    fn px(bytes: &[u8], i: usize) -> u8 {
        bytes[i * 4]
    }

    #[test]
    fn rotations_and_flips_move_pixels_correctly() {
        // flipH: B A
        let (o, w, h) = orient_rgba(&AB, 2, 1, 8, 2).unwrap();
        assert_eq!((w, h, px(&o, 0), px(&o, 1)), (2, 1, 2, 1));
        // rot180 == flipH for a 2x1
        let (o, ..) = orient_rgba(&AB, 2, 1, 8, 3).unwrap();
        assert_eq!((px(&o, 0), px(&o, 1)), (2, 1));
        // rot90CW: A on top, B below (dims swap)
        let (o, w, h) = orient_rgba(&AB, 2, 1, 8, 6).unwrap();
        assert_eq!((w, h, px(&o, 0), px(&o, 1)), (1, 2, 1, 2));
        // rot270CW: B on top
        let (o, w, h) = orient_rgba(&AB, 2, 1, 8, 8).unwrap();
        assert_eq!((w, h, px(&o, 0), px(&o, 1)), (1, 2, 2, 1));
        // identity and bad codes are None
        assert!(orient_rgba(&AB, 2, 1, 8, 1).is_none());
        assert!(orient_rgba(&AB, 2, 1, 8, 9).is_none());
    }

    #[test]
    fn compose_tables_are_group_consistent() {
        use crate::annotations::{compose_orientation as c, OrientOp::*};
        for s in 1..=8 {
            assert_eq!(c(c(c(c(s, RotateCw), RotateCw), RotateCw), RotateCw), s);
            assert_eq!(c(c(s, FlipH), FlipH), s);
            assert_eq!(c(c(s, FlipV), FlipV), s);
            assert_eq!(c(c(s, RotateCw), RotateCcw), s);
            // flipH then flipV == rot180
            assert_eq!(c(c(s, FlipH), FlipV), c(c(s, RotateCw), RotateCw));
        }
        assert_eq!(c(1, RotateCw), 6);
        assert_eq!(c(1, RotateCcw), 8);
    }
}

/// Extract a normalized-rect crop from tight/padded RGBA8. `rect` is
/// `(x, y, w, h)` in [0,1] of the input image (display space — callers apply
/// orientation first). Returns tight RGBA plus pixel dims; `None` for a
/// malformed input or a degenerate (< 1px) result.
pub fn crop_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    rect: (f64, f64, f64, f64),
) -> Option<(Vec<u8>, u32, u32)> {
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    if stride < w * 4 || src.len() < stride * h {
        return None;
    }
    let (rx, ry, rw, rh) = rect;
    let x0 = ((rx.clamp(0.0, 1.0)) * w as f64).round() as usize;
    let y0 = ((ry.clamp(0.0, 1.0)) * h as f64).round() as usize;
    let cw = ((rw.clamp(0.0, 1.0)) * w as f64).round() as usize;
    let ch = ((rh.clamp(0.0, 1.0)) * h as f64).round() as usize;
    let cw = cw.min(w.saturating_sub(x0));
    let ch = ch.min(h.saturating_sub(y0));
    if cw == 0 || ch == 0 {
        return None;
    }
    let mut out = vec![0u8; cw * ch * 4];
    for row in 0..ch {
        let s = (y0 + row) * stride + x0 * 4;
        let d = row * cw * 4;
        out[d..d + cw * 4].copy_from_slice(&src[s..s + cw * 4]);
    }
    Some((out, cw as u32, ch as u32))
}

/// Encode tight RGBA8 for the bake path (Save / Save As). `format` is matched
/// on the destination file extension: `jpg`/`jpeg` → JPEG q90 (alpha dropped),
/// anything else → PNG. Pure CPU; run on a worker.
pub fn encode_baked(rgba: &[u8], width: u32, height: u32, format: &str) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    match format.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => {
            let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
                .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .ok()?;
        }
        _ => {
            image::codecs::png::PngEncoder::new(&mut out)
                .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
                .ok()?;
        }
    }
    Some(out)
}

#[cfg(test)]
mod crop_tests {
    use super::*;

    #[test]
    fn crop_extracts_the_right_pixels() {
        // 2x2: A B / C D — crop right half → B / D
        let px = |v: u8| [v, v, v, 255];
        let src: Vec<u8> = [px(1), px(2), px(3), px(4)].concat();
        let (out, w, h) = crop_rgba(&src, 2, 2, 8, (0.5, 0.0, 0.5, 1.0)).unwrap();
        assert_eq!((w, h, out[0], out[4]), (1, 2, 2, 4));
        assert!(crop_rgba(&src, 2, 2, 8, (1.0, 0.0, 0.0, 1.0)).is_none());
    }

    #[test]
    fn encode_baked_roundtrips_png() {
        let src = vec![9u8; 4 * 4];
        let png = encode_baked(&src, 2, 2, "png").unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(1, 1).0, [9, 9, 9, 9]);
        assert!(encode_baked(&src, 2, 2, "jpg").is_some());
    }
}
