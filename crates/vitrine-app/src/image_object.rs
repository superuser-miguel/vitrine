//! `ImageObject` — the grid/filmstrip model item.
//!
//! A lightweight GObject wrapping one `gio::File` plus its mtime. It deliberately
//! does **not** hold the decoded thumbnail: with tens of thousands of items in a
//! folder, caching a texture per item is an unbounded leak (→ OOM on large
//! libraries). Thumbnails live in a size-bounded RAM cache
//! ([`crate::thumbnails::ThumbCache`]) keyed by URI instead; items only remember
//! whether a decode failed, so a broken file isn't retried forever.
//!
//! `rating` is a GObject **property** (not a plain field) so a grid cell can
//! observe `notify::rating` and repaint its star overlay the instant a rating
//! changes, without the window hunting down the cell by position.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ImageObject {
        pub file: RefCell<Option<gio::File>>,
        pub display_name: RefCell<String>,
        /// Source mtime (unix seconds), for validating cached thumbnails and the
        /// "Modified" sort.
        pub mtime: Cell<i64>,
        /// File size in bytes (from the folder enumerate), for the "Size" sort.
        pub size: Cell<i64>,
        /// Content type (MIME), for the "Type" sort.
        pub content_type: RefCell<String>,
        /// The file's content hash (annotation key), stamped from the index.
        pub content_hash: RefCell<String>,
        /// Star rating 0–5 (the `rating` property; notifies on change).
        pub rating: Cell<i32>,
        /// Non-destructive user orientation (EXIF 1-8; 1 = as-decoded).
        pub orientation: Cell<i32>,
        /// Non-destructive crop rect, normalized display space; None = full.
        pub crop: Cell<Option<(f64, f64, f64, f64)>>,
        /// EXIF capture time (unix seconds) for the "Date Taken" sort, stamped
        /// from the index. `None` until background enrichment has decoded the
        /// file — the sort has to cope with that, not assume it.
        pub date_taken: Cell<Option<i64>>,
        /// True if decoding failed — the cell shows a broken-image placeholder.
        pub failed: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageObject {
        const NAME: &'static str = "VitrineImageObject";
        type Type = super::ImageObject;
    }

    impl ObjectImpl for ImageObject {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPS.get_or_init(|| {
                vec![
                    glib::ParamSpecInt::builder("rating")
                        .minimum(0)
                        .maximum(5)
                        .build(),
                    glib::ParamSpecInt::builder("orientation")
                        .minimum(1)
                        .maximum(8)
                        .default_value(1)
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "rating" => self.rating.set(value.get().unwrap_or(0)),
                "orientation" => self.orientation.set(value.get().unwrap_or(1)),
                other => unimplemented!("set unknown property {other}"),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "rating" => self.rating.get().to_value(),
                "orientation" => self.orientation.get().to_value(),
                other => unimplemented!("get unknown property {other}"),
            }
        }
    }
}

glib::wrapper! {
    pub struct ImageObject(ObjectSubclass<imp::ImageObject>);
}

impl ImageObject {
    pub fn new(
        file: gio::File,
        display_name: &str,
        mtime: i64,
        size: i64,
        content_type: &str,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        *imp.file.borrow_mut() = Some(file);
        *imp.display_name.borrow_mut() = display_name.to_string();
        imp.mtime.set(mtime);
        imp.size.set(size);
        *imp.content_type.borrow_mut() = content_type.to_string();
        obj
    }

    pub fn file(&self) -> gio::File {
        self.imp()
            .file
            .borrow()
            .clone()
            .expect("ImageObject always constructed with a file")
    }

    pub fn display_name(&self) -> String {
        self.imp().display_name.borrow().clone()
    }

    pub fn mtime(&self) -> i64 {
        self.imp().mtime.get()
    }

    pub fn size(&self) -> i64 {
        self.imp().size.get()
    }

    pub fn content_type(&self) -> String {
        self.imp().content_type.borrow().clone()
    }

    /// The content hash (annotation key), or empty if the file isn't indexed yet.
    pub fn content_hash(&self) -> String {
        self.imp().content_hash.borrow().clone()
    }

    pub fn set_content_hash(&self, hash: &str) {
        *self.imp().content_hash.borrow_mut() = hash.to_string();
    }

    /// Current star rating (0–5).
    pub fn rating(&self) -> i32 {
        self.property("rating")
    }

    /// Set the rating (clamped 0–5). Emits `notify::rating`, so bound cells repaint.
    pub fn set_rating(&self, rating: i32) {
        self.set_property("rating", rating.clamp(0, 5));
    }

    /// Non-destructive user orientation (EXIF 1–8; 1 = identity). The `Cell`
    /// default is 0, so unstamped items read as identity via the `max(1)`.
    pub fn orientation(&self) -> i32 {
        self.property::<i32>("orientation").max(1)
    }

    /// Set the user orientation (clamped 1–8).
    pub fn set_orientation(&self, orientation: i32) {
        self.set_property("orientation", orientation.clamp(1, 8));
    }

    /// Non-destructive crop instruction (display-space normalized), if any.
    /// EXIF capture time, or `None` if not yet enriched.
    pub fn date_taken(&self) -> Option<i64> {
        self.imp().date_taken.get()
    }

    pub fn set_date_taken(&self, date_taken: Option<i64>) {
        self.imp().date_taken.set(date_taken);
    }

    pub fn crop(&self) -> Option<(f64, f64, f64, f64)> {
        self.imp().crop.get()
    }

    pub fn set_crop(&self, crop: Option<(f64, f64, f64, f64)>) {
        self.imp().crop.set(crop);
    }

    pub fn mark_failed(&self) {
        self.imp().failed.set(true);
    }

    pub fn has_failed(&self) -> bool {
        self.imp().failed.get()
    }
}
