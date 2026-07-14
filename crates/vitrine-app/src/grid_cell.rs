//! `VitrineGridCell` — the widget bound to each `GtkGridView` item.
//!
//! Layout is in `data/ui/grid_cell.blp`. This module does two things only:
//! binds the item's filename + thumbnail, and issues the async thumbnail decode
//! with a **recycling guard** — GridView reuses cell widgets as you scroll, so
//! a decode started for item A must never paint into a cell that has since been
//! rebound to item B (PLAN §8). We guard by comparing the cell's current item
//! against the item the decode was started for before touching the picture.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::CompositeTemplate;

use crate::image_object::ImageObject;

/// Thumbnail decode resolution (fits within THUMB_SIZE×THUMB_SIZE).
pub const THUMB_SIZE: u32 = 256;

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/github/superuser_miguel/Vitrine/grid_cell.ui")]
    pub struct VitrineGridCell {
        #[template_child]
        pub picture: TemplateChild<gtk::Picture>,
        #[template_child]
        pub label: TemplateChild<gtk::Label>,
        #[template_child]
        pub broken_icon: TemplateChild<gtk::Image>,
        /// The item this cell currently displays; the recycling-guard token.
        pub item: RefCell<Option<ImageObject>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VitrineGridCell {
        const NAME: &'static str = "VitrineGridCell";
        type Type = super::VitrineGridCell;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for VitrineGridCell {}
    impl WidgetImpl for VitrineGridCell {}
    impl BoxImpl for VitrineGridCell {}
}

glib::wrapper! {
    pub struct VitrineGridCell(ObjectSubclass<imp::VitrineGridCell>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for VitrineGridCell {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl VitrineGridCell {
    /// Bind this cell to `item`: show its name, and its thumbnail from cache or
    /// via an async decode.
    pub fn bind(&self, item: &ImageObject) {
        let imp = self.imp();
        imp.label.set_text(&item.display_name());
        *imp.item.borrow_mut() = Some(item.clone());

        if let Some(texture) = item.texture() {
            self.show_texture(&texture);
            return;
        }
        if item.has_failed() {
            self.show_broken();
            return;
        }
        // Placeholder while decoding; keep whatever the recycled cell showed
        // from being mistaken for this item.
        self.show_pending();

        if item.begin_load() {
            self.spawn_thumbnail(item.clone());
        }
    }

    /// Unbind on recycle: forget the item so late decodes don't paint here.
    pub fn unbind(&self) {
        let imp = self.imp();
        *imp.item.borrow_mut() = None;
        imp.picture.set_paintable(gtk::gdk::Paintable::NONE);
        imp.broken_icon.set_visible(false);
    }

    fn spawn_thumbnail(&self, item: ImageObject) {
        let file = item.file();
        let mtime = item.mtime();
        let renderer_widget = crate::thumbnails::renderer_source(self);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = cell)]
            self,
            async move {
                match crate::thumbnails::load(file, mtime, THUMB_SIZE, renderer_widget).await {
                    Some(thumb) => {
                        // Cache on the item so re-scroll is instant.
                        item.set_texture(Some(thumb.clone()));
                        if cell.is_showing(&item) {
                            cell.show_texture(&thumb);
                        }
                    }
                    None => {
                        item.mark_failed();
                        if cell.is_showing(&item) {
                            cell.show_broken();
                        }
                    }
                }
            }
        ));
    }

    /// Recycling guard: is this cell still bound to `item`?
    fn is_showing(&self, item: &ImageObject) -> bool {
        self.imp()
            .item
            .borrow()
            .as_ref()
            .is_some_and(|current| current == item)
    }

    fn show_texture(&self, texture: &gtk::gdk::Texture) {
        let imp = self.imp();
        imp.broken_icon.set_visible(false);
        imp.picture.set_paintable(Some(texture));
    }

    fn show_pending(&self) {
        let imp = self.imp();
        imp.broken_icon.set_visible(false);
        imp.picture.set_paintable(gtk::gdk::Paintable::NONE);
    }

    fn show_broken(&self) {
        let imp = self.imp();
        imp.picture.set_paintable(gtk::gdk::Paintable::NONE);
        imp.broken_icon.set_visible(true);
    }
}
