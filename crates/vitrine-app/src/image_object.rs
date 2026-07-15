//! `ImageObject` — the grid/filmstrip model item.
//!
//! A lightweight GObject wrapping one `gio::File` plus its mtime. It deliberately
//! does **not** hold the decoded thumbnail: with tens of thousands of items in a
//! folder, caching a texture per item is an unbounded leak (→ OOM on large
//! libraries). Thumbnails live in a size-bounded RAM cache
//! ([`crate::thumbnails::ThumbCache`]) keyed by URI instead; items only remember
//! whether a decode failed, so a broken file isn't retried forever.

use std::cell::{Cell, RefCell};

use gtk::gio;
use gtk::glib;
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
        /// True if decoding failed — the cell shows a broken-image placeholder.
        pub failed: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageObject {
        const NAME: &'static str = "VitrineImageObject";
        type Type = super::ImageObject;
    }

    impl ObjectImpl for ImageObject {}
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

    pub fn mark_failed(&self) {
        self.imp().failed.set(true);
    }

    pub fn has_failed(&self) -> bool {
        self.imp().failed.get()
    }
}
