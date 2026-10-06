//! Two hashes, two jobs (PLAN §4, §8).
//!
//! - **BLAKE3** hashes the file **bytes** → identity. Fast, SIMD, streaming.
//!   Keys tags/ratings (survives renames) and finds byte-identical duplicates.
//! - **Perceptual hash** hashes the **pixels** → similarity (resize/re-compress
//!   tolerant), for near-duplicate clustering and "find similar".
//!
//! Both are computed in the app's one ingestion pass from different inputs: raw
//! bytes → BLAKE3, the downscaled thumbnail pixels (EXIF-oriented) → pHash.

use std::io::Read;
use std::path::Path;

use image::RgbImage;
use image_hasher::{HashAlg, HasherConfig};

/// BLAKE3 hex digest of a byte slice.
pub fn blake3_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Whether `s` has the shape of a content hash as this crate writes one: a
/// BLAKE3 digest as 64 lowercase hex digits. A drop or other untyped input that
/// claims to be a hash is checked against this before it can key anything.
pub fn is_content_hash(s: &str) -> bool {
    s.len() == blake3::OUT_LEN * 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// BLAKE3 hex digest of a reader, streamed (constant memory for huge files).
pub fn blake3_reader<R: Read>(mut reader: R) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// BLAKE3 hex digest of a file on disk.
pub fn blake3_file(path: &Path) -> std::io::Result<String> {
    blake3_reader(std::io::BufReader::new(std::fs::File::open(path)?))
}

/// 64-bit perceptual hash (dHash/gradient, 8×8) from tightly-packed RGB8 pixels.
///
/// Returns `None` if the buffer isn't `width * height * 3` bytes. The result is
/// stored as `i64` (the `files.phash` column); near-duplicate distance is the
/// popcount of `a ^ b` over the raw bits. **Feed EXIF-oriented pixels** so a
/// rotated copy still near-matches (PLAN §8).
pub fn phash_rgb8(width: u32, height: u32, rgb: &[u8]) -> Option<i64> {
    if rgb.len() != (width as usize) * (height as usize) * 3 {
        return None;
    }
    let img = RgbImage::from_raw(width, height, rgb.to_vec())?;
    let hasher = HasherConfig::new()
        .hash_alg(HashAlg::Gradient)
        .hash_size(8, 8)
        .to_hasher();
    let bytes = hasher.hash_image(&img).into_inner();
    let mut arr = [0u8; 8];
    for (slot, b) in arr.iter_mut().zip(bytes.iter()) {
        *slot = *b;
    }
    Some(i64::from_le_bytes(arr))
}

/// Hamming distance between two perceptual hashes (bits that differ).
pub fn phash_distance(a: i64, b: i64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_shape_matches_what_blake3_writes() {
        assert!(is_content_hash(&blake3_bytes(b"")));
        assert!(is_content_hash(&blake3_bytes(b"any bytes at all")));
        for not_a_hash in [
            "",
            "hello",
            "https://example.com/cat.jpg",
            // Right length, wrong alphabet / case.
            "AF1349B9F5F9A1A6A0404DEA36DCC9499BCB25C9ADC112B7CC9A93CAE41F3262",
            "zf1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            // One short, one long, surrounding whitespace.
            "f1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f32620",
            " af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            // 64 bytes but not 64 ASCII hex digits.
            "éf1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f326",
        ] {
            assert!(!is_content_hash(not_a_hash), "{not_a_hash:?}");
        }
    }

    #[test]
    fn blake3_is_stable_and_content_addressed() {
        // Known BLAKE3 of the empty input.
        assert_eq!(
            blake3_bytes(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        // Same bytes → same hash; different bytes → different hash.
        assert_eq!(blake3_bytes(b"hello"), blake3_bytes(b"hello"));
        assert_ne!(blake3_bytes(b"hello"), blake3_bytes(b"world"));
        // Reader path agrees with the slice path.
        assert_eq!(
            blake3_reader(&b"hello"[..]).unwrap(),
            blake3_bytes(b"hello")
        );
    }

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        (0..w * h).flat_map(|_| [r, g, b]).collect()
    }

    #[test]
    fn phash_rejects_wrong_buffer_size() {
        assert!(phash_rgb8(4, 4, &[0u8; 10]).is_none());
        assert!(phash_rgb8(2, 2, &solid(2, 2, 0, 0, 0)).is_some());
    }

    #[test]
    fn phash_similar_images_are_close() {
        // A gradient vs the same gradient with a small brightness shift should
        // be near (small Hamming distance); an inverted image should be far.
        let grad: Vec<u8> = (0..32u32 * 32)
            .flat_map(|i| {
                let v = ((i % 32) * 8) as u8;
                [v, v, v]
            })
            .collect();
        let grad_shift: Vec<u8> = grad.iter().map(|p| p.saturating_add(10)).collect();
        let inverted: Vec<u8> = grad.iter().map(|p| 255 - p).collect();

        let a = phash_rgb8(32, 32, &grad).unwrap();
        let b = phash_rgb8(32, 32, &grad_shift).unwrap();
        let c = phash_rgb8(32, 32, &inverted).unwrap();

        assert!(phash_distance(a, b) <= 4, "shifted copy should be near");
        assert!(
            phash_distance(a, c) > phash_distance(a, b),
            "inverted should be farther than a brightness shift"
        );
        assert_eq!(phash_distance(a, a), 0);
    }
}
