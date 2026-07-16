//! XMP metadata generation for interop with other photo tools.
//!
//! Vitrine keeps annotations in its own content-hash-keyed index, but the rest
//! of the ecosystem (digiKam, darktable, Lightroom, XnView, …) speaks XMP. This
//! module renders a Vitrine record into a standard XMP packet so those tools can
//! pick up its ratings, comments, and tags.
//!
//! The packet is written as an `.xmp` **sidecar** next to the image
//! (`photo.jpg` → `photo.jpg.xmp`, the digiKam/darktable convention). That is a
//! non-destructive interop path: it never rewrites the original file's bytes, so
//! nothing can corrupt an irreplaceable original. Embedding the packet directly
//! into JPEG/PNG containers is a later refinement on top of this same generator.
//!
//! Mapping: rating → `xmp:Rating`, comment → `dc:description`, tags →
//! `dc:subject`.

use crate::Db;
use std::path::{Path, PathBuf};

/// The annotations for one image, ready to render as an XMP packet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpMetadata {
    /// 0–5 star rating, if set.
    pub rating: Option<i64>,
    /// Free-text comment, if set.
    pub comment: Option<String>,
    /// Tags applied to the image.
    pub tags: Vec<String>,
}

impl XmpMetadata {
    /// True when there is nothing worth writing (no rating, comment, or tags).
    pub fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.comment.as_deref().unwrap_or("").is_empty()
            && self.tags.is_empty()
    }

    /// Render a complete, standards-compliant XMP sidecar packet.
    pub fn to_packet(&self) -> String {
        let mut props = String::new();

        if let Some(rating) = self.rating {
            props.push_str(&format!(
                "   <xmp:Rating>{}</xmp:Rating>\n",
                rating.clamp(0, 5)
            ));
        }

        if let Some(comment) = self.comment.as_deref().filter(|c| !c.is_empty()) {
            props.push_str(&format!(
                "   <dc:description>\n    <rdf:Alt>\n     \
                 <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    \
                 </rdf:Alt>\n   </dc:description>\n",
                escape(comment)
            ));
        }

        if !self.tags.is_empty() {
            props.push_str("   <dc:subject>\n    <rdf:Bag>\n");
            for tag in &self.tags {
                props.push_str(&format!("     <rdf:li>{}</rdf:li>\n", escape(tag)));
            }
            props.push_str("    </rdf:Bag>\n   </dc:subject>\n");
        }

        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Vitrine\">\n \
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
             <rdf:Description rdf:about=\"\"\n    \
             xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n    \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
             {props}  \
             </rdf:Description>\n \
             </rdf:RDF>\n\
             </x:xmpmeta>\n\
             <?xpacket end=\"w\"?>\n"
        )
    }
}

impl Db {
    /// Gather the annotations for `content_hash` into an [`XmpMetadata`].
    pub fn xmp_for_hash(&self, content_hash: &str) -> rusqlite::Result<XmpMetadata> {
        Ok(XmpMetadata {
            rating: self.rating(content_hash)?,
            comment: self.comment(content_hash)?,
            tags: self.tags_for_hash(content_hash)?,
        })
    }
}

/// The XMP sidecar path for an image: append `.xmp` to the full filename
/// (`photo.jpg` → `photo.jpg.xmp`), the digiKam/darktable convention.
pub fn sidecar_path(image: &Path) -> PathBuf {
    let mut name = image
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".xmp");
    image.with_file_name(name)
}

/// Escape XML text content (`&`, `<`, `>`).
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metadata_is_empty() {
        assert!(XmpMetadata::default().is_empty());
        assert!(XmpMetadata {
            comment: Some(String::new()),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn rating_is_clamped_and_rendered() {
        let packet = XmpMetadata {
            rating: Some(9),
            ..Default::default()
        }
        .to_packet();
        assert!(packet.contains("<xmp:Rating>5</xmp:Rating>"), "{packet}");
        assert!(packet.contains("W5M0MpCehiHzreSzNTczkc9d"));
        assert!(packet.contains("<?xpacket end=\"w\"?>"));
    }

    #[test]
    fn comment_is_xml_escaped() {
        let packet = XmpMetadata {
            comment: Some("a < b & c".into()),
            ..Default::default()
        }
        .to_packet();
        assert!(packet.contains("a &lt; b &amp; c"), "{packet}");
        assert!(!packet.contains("a < b & c"));
        assert!(packet.contains("xml:lang=\"x-default\""));
    }

    #[test]
    fn tags_become_a_bag() {
        let packet = XmpMetadata {
            tags: vec!["sunset".into(), "beach".into()],
            ..Default::default()
        }
        .to_packet();
        assert!(packet.contains("<dc:subject>"));
        assert!(packet.contains("<rdf:li>sunset</rdf:li>"));
        assert!(packet.contains("<rdf:li>beach</rdf:li>"));
    }

    #[test]
    fn sidecar_appends_extension() {
        assert_eq!(
            sidecar_path(Path::new("/a/b/photo.jpg")),
            PathBuf::from("/a/b/photo.jpg.xmp")
        );
        assert_eq!(
            sidecar_path(Path::new("/a/b/image.tar.avif")),
            PathBuf::from("/a/b/image.tar.avif.xmp")
        );
    }
}
