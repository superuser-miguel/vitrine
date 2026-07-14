//! Pure EXIF extraction — no decode, no GTK.
//!
//! The app hands us the raw EXIF blob glycin pulls out of an image
//! (`ImageDetails::metadata_exif`); here we parse out only the few fields the
//! index sorts and filters on: capture time, camera, and orientation. Keeping
//! this in the engine means it is testable headless and the app stays
//! decode-only — the boundary (house rule 2) holds.

use exif::{In, Reader, Tag, Value};

/// The indexable EXIF fields. All optional — most images carry only some.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExifData {
    /// Capture time as unix seconds (parsed as UTC-naive: EXIF has no zone, so
    /// this is a stable *ordering* key, not an instant to display literally).
    pub date_taken: Option<i64>,
    /// "Make Model", trimmed (e.g. "SONY ILCE-7M3").
    pub camera: Option<String>,
    /// EXIF orientation, 1..=8.
    pub orientation: Option<i64>,
}

/// Parse a raw EXIF/TIFF blob. Never fails — a malformed or empty blob yields an
/// all-`None` [`ExifData`]. Tolerates an optional leading `Exif\0\0` marker.
pub fn parse_exif(blob: &[u8]) -> ExifData {
    let raw = blob.strip_prefix(b"Exif\0\0").unwrap_or(blob);
    let exif = match Reader::new().read_raw(raw.to_vec()) {
        Ok(exif) => exif,
        Err(_) => return ExifData::default(),
    };

    let date_taken = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY))
        .and_then(|f| ascii(&f.value))
        .and_then(|s| parse_exif_datetime(&s));

    let make = exif
        .get_field(Tag::Make, In::PRIMARY)
        .and_then(|f| ascii(&f.value));
    let model = exif
        .get_field(Tag::Model, In::PRIMARY)
        .and_then(|f| ascii(&f.value));
    let camera = join_camera(make, model);

    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|n| n as i64)
        .filter(|&n| (1..=8).contains(&n));

    ExifData {
        date_taken,
        camera,
        orientation,
    }
}

/// The first ASCII string of a value, trimmed of NULs/whitespace; `None` if empty.
fn ascii(value: &Value) -> Option<String> {
    if let Value::Ascii(items) = value {
        let bytes = items.first()?;
        let s = String::from_utf8_lossy(bytes);
        let trimmed = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else {
        None
    }
}

/// Combine Make and Model into one camera label, dropping a redundant Make that
/// the Model already starts with (e.g. Make "NIKON", Model "NIKON D750").
fn join_camera(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) => {
            if model.to_lowercase().starts_with(&make.to_lowercase()) {
                Some(model)
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (Some(make), None) => Some(make),
        (None, Some(model)) => Some(model),
        (None, None) => None,
    }
}

/// Parse an EXIF `DateTime` string ("YYYY:MM:DD HH:MM:SS") to unix seconds,
/// treating it as UTC (a stable sort key). Returns `None` on any malformation,
/// including the all-zero placeholder some cameras write.
fn parse_exif_datetime(s: &str) -> Option<i64> {
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split(':');
    let year: i64 = d.next()?.trim().parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next()?.trim().parse().ok()?;

    if year == 0 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Days since the unix epoch for a proleptic-Gregorian date (Howard Hinnant's
/// algorithm — pure integer math, no chrono dependency).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_reference_points() {
        // 1970-01-01 00:00:00 == 0; a known later instant checks the arithmetic.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(parse_exif_datetime("1970:01:01 00:00:00"), Some(0));
        // 2001-09-09 01:46:40 UTC is unix 1_000_000_000.
        assert_eq!(
            parse_exif_datetime("2001:09:09 01:46:40"),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn rejects_placeholder_and_garbage() {
        assert_eq!(parse_exif_datetime("0000:00:00 00:00:00"), None);
        assert_eq!(parse_exif_datetime("not a date"), None);
        assert_eq!(parse_exif_datetime(""), None);
    }

    #[test]
    fn camera_joins_and_dedupes_make() {
        assert_eq!(
            join_camera(Some("SONY".into()), Some("ILCE-7M3".into())),
            Some("SONY ILCE-7M3".into())
        );
        // Model already carries the make → don't double it.
        assert_eq!(
            join_camera(Some("NIKON".into()), Some("NIKON D750".into())),
            Some("NIKON D750".into())
        );
        assert_eq!(join_camera(None, None), None);
    }

    #[test]
    fn empty_blob_is_all_none() {
        assert_eq!(parse_exif(&[]), ExifData::default());
        assert_eq!(parse_exif(b"garbage"), ExifData::default());
    }
}
