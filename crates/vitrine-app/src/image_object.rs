//! `ImageObject` — the grid/filmstrip model item.
//!
//! A lightweight GObject wrapping one `gio::File`. Construction is cheap (no
//! decode); the thumbnail `texture` property is filled asynchronously when a
//! cell for this item is bound and scrolled into view. Because the same
//! `ImageObject` instance is reused for the item's lifetime, a thumbnail decoded
//! once survives cell recycling and re-scrolling.

use std::cell::RefCell;

use gtk::gdk;
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
        pub texture: RefCell<Option<gdk::Texture>>,
        /// True once a thumbnail load has started, so binds don't re-issue it.
        pub load_started: RefCell<bool>,
        /// True if decoding failed — the cell shows a broken-image placeholder.
        pub failed: RefCell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageObject {
        const NAME: &'static str = "VitrineImageObject";
        type Type = super::ImageObject;
    }

    impl ObjectImpl for ImageObject {
        fn properties() -> &'static [glib::ParamSpec] {
            use std::sync::OnceLock;
            static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("display-name")
                        .read_only()
                        .build(),
                    glib::ParamSpecObject::builder::<gdk::Texture>("texture").build(),
                ]
            })
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "display-name" => self.display_name.borrow().to_value(),
                "texture" => self.texture.borrow().to_value(),
                _ => unimplemented!(),
            }
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "texture" => {
                    *self.texture.borrow_mut() = value.get().ok().flatten();
                }
                _ => unimplemented!(),
            }
        }
    }
}

glib::wrapper! {
    pub struct ImageObject(ObjectSubclass<imp::ImageObject>);
}

impl ImageObject {
    pub fn new(file: gio::File, display_name: &str) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        *imp.file.borrow_mut() = Some(file);
        *imp.display_name.borrow_mut() = display_name.to_string();
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

    pub fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }

    pub fn set_texture(&self, texture: Option<gdk::Texture>) {
        self.set_property("texture", texture);
    }

    /// Returns true and marks the load started if it had not started yet — used
    /// by the cell bind to issue the thumbnail decode exactly once per item.
    pub fn begin_load(&self) -> bool {
        let mut started = self.imp().load_started.borrow_mut();
        if *started {
            false
        } else {
            *started = true;
            true
        }
    }

    pub fn mark_failed(&self) {
        *self.imp().failed.borrow_mut() = true;
    }

    pub fn has_failed(&self) -> bool {
        *self.imp().failed.borrow()
    }
}
