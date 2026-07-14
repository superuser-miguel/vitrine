//! Inject freedesktop thumbnail metadata into an encoded PNG.
//!
//! The thumbnail spec requires cached thumbnails to carry `Thumb::URI` and
//! `Thumb::MTime` `tEXt` chunks so other apps (Nautilus/GNOME) can validate them
//! against the source. GTK's PNG encoder doesn't add these, so rather than
//! re-encode pixels we splice `tEXt` chunks into the already-encoded stream —
//! pure byte manipulation, hence UI-free and testable here.

/// PNG signature.
const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// CRC-32 (IEEE, as used by PNG chunks).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Return a copy of `png` with `tEXt` chunks for `entries` (keyword, text)
/// inserted right after the IHDR chunk. Returns `None` if `png` is not a PNG
/// with a leading IHDR chunk.
pub fn add_text_chunks(png: &[u8], entries: &[(&str, &str)]) -> Option<Vec<u8>> {
    if png.len() < 8 + 12 || png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let ihdr_len = u32::from_be_bytes([png[8], png[9], png[10], png[11]]) as usize;
    // signature(8) + length(4) + type(4) + data(ihdr_len) + crc(4)
    let ihdr_end = 8 + 4 + 4 + ihdr_len + 4;
    if png.len() < ihdr_end {
        return None;
    }

    let mut out = Vec::with_capacity(png.len() + 64);
    out.extend_from_slice(&png[..ihdr_end]);
    for (keyword, text) in entries {
        out.extend_from_slice(&text_chunk(keyword, text));
    }
    out.extend_from_slice(&png[ihdr_end..]);
    Some(out)
}

/// Build one `tEXt` chunk: `len | "tEXt" | keyword \0 text | crc`.
fn text_chunk(keyword: &str, text: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(keyword.len() + 1 + text.len());
    data.extend_from_slice(keyword.as_bytes());
    data.push(0);
    data.extend_from_slice(text.as_bytes());

    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(b"tEXt");
    crc_input.extend_from_slice(&data);

    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(&data);
    chunk.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    chunk
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// A minimal but structurally-valid PNG: signature + IHDR + IEND.
    fn minimal_png() -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(&SIGNATURE);
        // IHDR: 13 bytes of data (1x1, 8-bit, RGBA) — content irrelevant here.
        let ihdr_data = [0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
        png.extend_from_slice(&(ihdr_data.len() as u32).to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&ihdr_data);
        png.extend_from_slice(&0u32.to_be_bytes()); // fake crc, unchecked here
                                                    // IEND
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&0u32.to_be_bytes());
        png
    }

    #[test]
    fn rejects_non_png() {
        assert!(add_text_chunks(b"not a png at all!!", &[("k", "v")]).is_none());
    }

    #[test]
    fn inserts_text_after_ihdr() {
        let png = minimal_png();
        let out = add_text_chunks(&png, &[("Thumb::URI", "file:///x"), ("Thumb::MTime", "42")])
            .expect("valid png");

        // IHDR (33 bytes) is preserved, then a tEXt chunk must follow.
        let ihdr_end = 8 + 4 + 4 + 13 + 4;
        assert_eq!(&out[..ihdr_end], &png[..ihdr_end]);
        assert_eq!(&out[ihdr_end + 4..ihdr_end + 8], b"tEXt");
        // The payload contains our keyword and value.
        assert!(out
            .windows(b"Thumb::URI\0file:///x".len())
            .any(|w| w == b"Thumb::URI\0file:///x"));
        assert!(out
            .windows(b"Thumb::MTime\x0042".len())
            .any(|w| w == b"Thumb::MTime\x0042"));
        // IEND still at the very end.
        assert_eq!(&out[out.len() - 8..out.len() - 4], b"IEND");
    }
}
