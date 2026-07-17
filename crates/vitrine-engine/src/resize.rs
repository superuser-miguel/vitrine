//! CPU thumbnail resize — pure pixel math (no GTK), so it runs on a worker
//! thread instead of the GSK GPU downscale, which had to run on the main thread
//! (and triggered a ~1.7 s GL shader compile on first use). RGBA8 in, tightly-
//! packed RGBA8 out.

/// Shrink an RGBA8 image so its longest edge is at most `max_edge`, preserving
/// aspect. `stride` is the source row length in bytes (may exceed `width * 4`
/// when the decoder pads rows). Returns `(tight RGBA8 bytes, out_width,
/// out_height)`. If the image already fits, it's returned repacked to tight rows,
/// unscaled.
pub fn resize_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    max_edge: u32,
) -> (Vec<u8>, u32, u32) {
    let tight = to_tight_rgba(src, width, height, stride);
    let longest = width.max(height);
    let expected = (width as usize) * (height as usize) * 4;
    if longest == 0 || longest <= max_edge || tight.len() != expected {
        return (tight, width, height);
    }
    let scale = max_edge as f64 / longest as f64;
    let nw = ((width as f64 * scale).round() as u32).max(1);
    let nh = ((height as f64 * scale).round() as u32).max(1);
    let img = image::RgbaImage::from_raw(width, height, tight).expect("validated length");
    // `thumbnail` box-averages — good quality for large downscale ratios, and
    // fast (unlike `resize` with a wide filter, which aliases on big shrinks).
    let thumb = image::imageops::thumbnail(&img, nw, nh);
    (thumb.into_raw(), nw, nh)
}

/// Copy `src` into tightly-packed RGBA rows, dropping any per-row stride padding.
fn to_tight_rgba(src: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let row = (width as usize) * 4;
    let stride = stride as usize;
    if stride == row {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        if start + row <= src.len() {
            out.extend_from_slice(&src[start..start + row]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_to_max_edge_preserving_aspect() {
        let src = [255u8, 0, 0, 255].repeat(40 * 20); // 40x20 solid red
        let (out, w, h) = resize_rgba(&src, 40, 20, 40 * 4, 10);
        assert_eq!((w, h), (10, 5));
        assert_eq!(out.len(), 10 * 5 * 4);
        assert!(out[0] > 200 && out[1] < 60, "still red-ish"); // averaged red
    }

    #[test]
    fn passthrough_when_already_small() {
        let src = [1u8, 2, 3, 4].repeat(8 * 8);
        let (out, w, h) = resize_rgba(&src, 8, 8, 8 * 4, 256);
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
        let (out, w, h) = resize_rgba(&src, 2, 2, 12, 256);
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            out,
            vec![10, 20, 30, 40, 50, 60, 70, 80, 11, 21, 31, 41, 51, 61, 71, 81]
        );
    }
}
