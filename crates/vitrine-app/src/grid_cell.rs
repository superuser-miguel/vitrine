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
use crate::thumbnails::ThumbCache;

/// Default thumbnail resolution (also the filmstrip's load size).
pub const THUMB_SIZE: u32 = 256;

/// Load a thumbnail at least this large even for small icons, so small/medium
/// icons reuse the warm shared `large` (256) cache instead of a cold `normal`.
const MIN_LOAD_PX: u32 = 256;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/github/superuser_miguel/Vitrine/grid_cell.ui")]
    pub struct VitrineGridCell {
        #[template_child]
        pub picture: TemplateChild<gtk::Picture>,
        #[template_child]
        pub label: TemplateChild<gtk::Label>,
        #[template_child]
        pub broken_icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub rating_overlay: TemplateChild<gtk::Label>,
        /// The item this cell currently displays; the recycling-guard token.
        pub item: RefCell<Option<ImageObject>>,
        /// `notify::rating` subscription on the bound item (disconnected on rebind
        /// so a recycled cell doesn't react to its previous item).
        pub rating_handler: RefCell<Option<glib::SignalHandlerId>>,
        /// Display size of the thumbnail (px), from the grid's icon-size level.
        pub icon_size: Cell<u32>,
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
    /// Set the thumbnail display size (px). Sizes the picture and scales the
    /// filename width to roughly match.
    pub fn set_icon_size(&self, px: u32) {
        let imp = self.imp();
        imp.icon_size.set(px);
        imp.picture.set_size_request(px as i32, px as i32);
        // Roughly one char per 8 px, clamped, so the label tracks the icon width.
        let chars = (px / 8).clamp(8, 28) as i32;
        imp.label.set_width_chars(chars);
        imp.label.set_max_width_chars(chars);
    }

    /// Resolution to load: at least [`MIN_LOAD_PX`] so small icons reuse the warm
    /// shared cache, and higher for big icons so they stay sharp.
    fn load_size(&self) -> u32 {
        self.imp().icon_size.get().max(MIN_LOAD_PX)
    }
}

impl VitrineGridCell {
    /// Bind this cell to `item` and show what we have *synchronously* (no async
    /// spawn — that's the expensive part during fast scroll). Returns `true` if
    /// the thumbnail still needs loading, so the caller can queue it and load
    /// only when scrolling settles.
    pub fn bind(&self, item: &ImageObject, cache: &ThumbCache) -> bool {
        let imp = self.imp();
        self.disconnect_rating(); // drop the recycled item's subscription first
        imp.label.set_text(&item.display_name());
        *imp.item.borrow_mut() = Some(item.clone());

        // Rating overlay: reflect it now and repaint reactively on notify::rating.
        self.update_rating_overlay(item.rating());
        let handler = item.connect_notify_local(
            Some("rating"),
            glib::clone!(
                #[weak(rename_to = cell)]
                self,
                move |item, _| cell.update_rating_overlay(item.rating())
            ),
        );
        *imp.rating_handler.borrow_mut() = Some(handler);

        let key = crate::thumbnails::ram_key(&item.file().uri(), self.load_size());
        if let Some(texture) = cache.borrow_mut().get(&key).cloned() {
            self.show_texture(&texture);
            return false;
        }
        if item.has_failed() {
            self.show_broken();
            return false;
        }
        // Placeholder while loading; keep whatever the recycled cell showed from
        // being mistaken for this item.
        self.show_pending();
        true
    }

    /// The item this cell currently displays, if any.
    pub fn item(&self) -> Option<ImageObject> {
        self.imp().item.borrow().clone()
    }

    /// Add a drag source so this image can be dragged onto a catalog (the drag
    /// carries the current item's content hash). Called once per cell at setup.
    pub fn add_drag_source(&self) {
        let source = gtk::DragSource::new();
        source.set_actions(gtk::gdk::DragAction::COPY);
        source.connect_prepare(glib::clone!(
            #[weak(rename_to = cell)]
            self,
            #[upgrade_or]
            None,
            move |_, _, _| {
                let hash = cell.item()?.content_hash();
                if hash.is_empty() {
                    return None;
                }
                Some(gtk::gdk::ContentProvider::for_value(&hash.to_value()))
            }
        ));
        self.add_controller(source);
    }

    /// Unbind on recycle: forget the item so late loads don't paint here.
    pub fn unbind(&self) {
        self.disconnect_rating();
        let imp = self.imp();
        *imp.item.borrow_mut() = None;
        imp.picture.set_paintable(gtk::gdk::Paintable::NONE);
        imp.broken_icon.set_visible(false);
        imp.rating_overlay.set_visible(false);
    }

    /// Show `rating` stars (or hide the overlay when unrated).
    fn update_rating_overlay(&self, rating: i32) {
        let overlay = &self.imp().rating_overlay;
        if rating > 0 {
            overlay.set_text(&"★".repeat(rating.clamp(0, 5) as usize));
            overlay.set_visible(true);
        } else {
            overlay.set_visible(false);
        }
    }

    /// Drop the `notify::rating` subscription on the currently-bound item.
    fn disconnect_rating(&self) {
        let imp = self.imp();
        if let Some(id) = imp.rating_handler.borrow_mut().take() {
            if let Some(old) = imp.item.borrow().as_ref() {
                old.disconnect(id);
            }
        }
    }

    /// Start the async thumbnail load for `item` (called by the window when
    /// scrolling has settled). No-op if the cell has since moved to another item.
    /// Apply a load result to this cell, but only if it is still bound to `item`
    /// (the recycling guard). Called by the window's bounded load scheduler once
    /// a decode completes — the window owns loading now, so a fast fling can't
    /// spawn an unbounded pile of per-cell decode futures.
    pub fn apply(&self, item: &ImageObject, texture: Option<&gtk::gdk::Texture>) {
        if !self.is_showing(item) {
            return;
        }
        match texture {
            Some(t) => self.show_texture(t),
            None => self.show_broken(),
        }
    }

    /// Recycling guard: is this cell still bound to `item`?
    pub fn is_showing(&self, item: &ImageObject) -> bool {
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
