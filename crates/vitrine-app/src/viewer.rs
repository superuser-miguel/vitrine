//! `VitrineViewer` — the single-image viewer page.
//!
//! Pushed onto the browser's `AdwNavigationView` when a grid item is activated.
//! Displays one image at viewer resolution, with fit / zoom / pan, arrow-key
//! prev-next, and a filmstrip along the bottom kept in sync with the shown
//! image. Decoded viewer textures go through a size-bounded LRU (the engine's
//! `SizedLru`, ~256 MB) keyed by file URI, and navigation prefetches ±2
//! neighbours so next/prev shows no decode flash on a warm cache (PLAN Phase 1).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use vitrine_engine::SizedLru;

use crate::grid_cell::THUMB_SIZE;
use crate::image_object::ImageObject;

/// Cap the longest edge of a decoded viewer texture. Bounds per-image memory
/// (≤ ~4096²·4 ≈ 64 MB) so the LRU holds a useful working set.
const VIEW_MAX: u32 = 4096;
/// Viewer texture cache budget.
const CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// Zoom multiplier per step; zoom range clamps around 1.0 (100%).
const ZOOM_STEP: f64 = 1.25;
const ZOOM_MIN: f64 = 0.05;
const ZOOM_MAX: f64 = 20.0;

type TextureCache = Rc<RefCell<SizedLru<String, gdk::Texture>>>;

mod imp {
    use super::*;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/superuser_miguel/Vitrine/viewer.ui")]
    pub struct VitrineViewer {
        #[template_child]
        pub title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub picture: TemplateChild<gtk::Picture>,
        #[template_child]
        pub picture_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub filmstrip_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub zoom_in_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub zoom_out_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub zoom_fit_button: TemplateChild<gtk::Button>,

        /// Shared, ordered image model (same store the grid shows).
        pub store: RefCell<Option<gio::ListStore>>,
        /// Filmstrip selection = the current image; single source of "current".
        pub filmstrip: RefCell<Option<gtk::SingleSelection>>,
        pub filmstrip_view: RefCell<Option<gtk::ListView>>,
        /// None = fit-to-window; Some(f) = zoom factor over the texture's pixels.
        pub zoom: Cell<Option<f64>>,
        /// Natural pixel size of the currently displayed texture.
        pub natural: Cell<(i32, i32)>,
        pub cache: TextureCache,
        /// Guards the filmstrip-selection ↔ show-position feedback loop.
        pub syncing: Cell<bool>,
    }

    impl Default for VitrineViewer {
        fn default() -> Self {
            Self {
                title: Default::default(),
                picture: Default::default(),
                picture_scroller: Default::default(),
                filmstrip_scroller: Default::default(),
                zoom_in_button: Default::default(),
                zoom_out_button: Default::default(),
                zoom_fit_button: Default::default(),
                store: RefCell::new(None),
                filmstrip: RefCell::new(None),
                filmstrip_view: RefCell::new(None),
                zoom: Cell::new(None),
                natural: Cell::new((0, 0)),
                cache: Rc::new(RefCell::new(SizedLru::new(CACHE_BYTES))),
                syncing: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VitrineViewer {
        const NAME: &'static str = "VitrineViewer";
        type Type = super::VitrineViewer;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for VitrineViewer {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_filmstrip();
            obj.setup_controls();
        }
    }

    impl WidgetImpl for VitrineViewer {}
    impl NavigationPageImpl for VitrineViewer {}
}

glib::wrapper! {
    pub struct VitrineViewer(ObjectSubclass<imp::VitrineViewer>)
        @extends adw::NavigationPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for VitrineViewer {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl VitrineViewer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Show `store`'s image at `position`, (re)pointing the filmstrip at it.
    pub fn open(&self, store: gio::ListStore, position: u32) {
        let imp = self.imp();
        *imp.store.borrow_mut() = Some(store.clone());
        if let Some(filmstrip) = imp.filmstrip.borrow().as_ref() {
            filmstrip.set_model(Some(&store));
        }
        self.show_position(position);
    }

    // --- setup ---------------------------------------------------------------

    fn setup_filmstrip(&self) {
        let imp = self.imp();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let pic = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Contain)
                .width_request(72)
                .height_request(72)
                .build();
            list_item.set_child(Some(&pic));
        });
        factory.connect_bind(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let pic = list_item.child().and_downcast::<gtk::Picture>().unwrap();
            let item = list_item.item().and_downcast::<ImageObject>().unwrap();
            // Reactive: any view that decodes this item updates the filmstrip.
            let binding = item
                .bind_property("texture", &pic, "paintable")
                .sync_create()
                .build();
            unsafe { list_item.set_data("tex-binding", binding) };
            crate::thumbnails::ensure_thumbnail(&pic, &item, THUMB_SIZE);
        });
        factory.connect_unbind(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            unsafe {
                if let Some(binding) = list_item.steal_data::<glib::Binding>("tex-binding") {
                    binding.unbind();
                }
            }
        });

        let selection = gtk::SingleSelection::builder()
            .autoselect(false)
            .can_unselect(true)
            .build();
        let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list_view.set_orientation(gtk::Orientation::Horizontal);
        list_view.add_css_class("filmstrip");
        list_view.set_single_click_activate(true);

        // Filmstrip click/keys change the current image.
        selection.connect_selected_notify(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |sel| {
                if viewer.imp().syncing.get() {
                    return;
                }
                let pos = sel.selected();
                if pos != gtk::INVALID_LIST_POSITION {
                    viewer.show_position(pos);
                }
            }
        ));

        imp.filmstrip_scroller.set_child(Some(&list_view));
        *imp.filmstrip.borrow_mut() = Some(selection);
        *imp.filmstrip_view.borrow_mut() = Some(list_view);
    }

    fn setup_controls(&self) {
        let imp = self.imp();

        imp.zoom_in_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.zoom_by(ZOOM_STEP)
        ));
        imp.zoom_out_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.zoom_by(1.0 / ZOOM_STEP)
        ));
        imp.zoom_fit_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.zoom_fit()
        ));

        // Keyboard: arrows navigate, +/-/0 zoom.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = v)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                match key {
                    gdk::Key::Left | gdk::Key::Up => v.step(-1),
                    gdk::Key::Right | gdk::Key::Down => v.step(1),
                    gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => v.zoom_by(ZOOM_STEP),
                    gdk::Key::minus | gdk::Key::KP_Subtract => v.zoom_by(1.0 / ZOOM_STEP),
                    gdk::Key::_0 | gdk::Key::KP_0 => v.zoom_fit(),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        ));
        self.add_controller(keys);

        // Ctrl+scroll zooms; plain scroll pans (ScrolledWindow default).
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = v)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |ctrl, _, dy| {
                if !ctrl
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    return glib::Propagation::Proceed;
                }
                if dy < 0.0 {
                    v.zoom_by(ZOOM_STEP);
                } else {
                    v.zoom_by(1.0 / ZOOM_STEP);
                }
                glib::Propagation::Stop
            }
        ));
        imp.picture_scroller.add_controller(scroll);
    }

    // --- navigation ----------------------------------------------------------

    fn item_at(&self, pos: u32) -> Option<ImageObject> {
        self.imp()
            .store
            .borrow()
            .as_ref()?
            .item(pos)
            .and_downcast::<ImageObject>()
    }

    fn n_items(&self) -> u32 {
        self.imp()
            .store
            .borrow()
            .as_ref()
            .map_or(0, |s| s.n_items())
    }

    fn step(&self, delta: i32) {
        let n = self.n_items();
        if n == 0 {
            return;
        }
        let cur = self.imp().filmstrip.borrow().as_ref().map_or(0, |f| {
            let s = f.selected();
            if s == gtk::INVALID_LIST_POSITION {
                0
            } else {
                s
            }
        });
        let next = (cur as i64 + delta as i64).clamp(0, n as i64 - 1) as u32;
        if next != cur {
            self.show_position(next);
        }
    }

    fn show_position(&self, pos: u32) {
        let Some(item) = self.item_at(pos) else {
            return;
        };
        let imp = self.imp();

        imp.title.set_title(&item.display_name());

        // Keep the filmstrip selection + scroll in step without re-entering.
        imp.syncing.set(true);
        if let Some(filmstrip) = imp.filmstrip.borrow().as_ref() {
            filmstrip.set_selected(pos);
        }
        imp.syncing.set(false);
        if let Some(view) = imp.filmstrip_view.borrow().as_ref() {
            view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
        }

        // Display from cache, or decode; either way reset zoom to fit.
        let uri = item.file().uri().to_string();
        if let Some(texture) = imp.cache.borrow_mut().get(&uri).cloned() {
            self.set_texture(&texture);
        } else {
            self.load_and_show(item.clone(), pos);
        }
        self.prefetch(pos);
    }

    /// Decode `item` at viewer resolution, cache it, and display it if the user
    /// is still on `pos` when it arrives.
    fn load_and_show(&self, item: ImageObject, pos: u32) {
        let file = item.file();
        let uri = file.uri().to_string();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            async move {
                if let Ok(texture) = crate::decode::full(&file, VIEW_MAX).await {
                    viewer.cache_texture(&uri, &texture);
                    if viewer.current_position() == pos {
                        viewer.set_texture(&texture);
                    }
                }
            }
        ));
    }

    /// Warm the cache for ±2 neighbours so next/prev is flash-free.
    fn prefetch(&self, pos: u32) {
        let n = self.n_items();
        for delta in [-2i64, -1, 1, 2] {
            let p = pos as i64 + delta;
            if p < 0 || p >= n as i64 {
                continue;
            }
            let p = p as u32;
            let Some(item) = self.item_at(p) else {
                continue;
            };
            let uri = item.file().uri().to_string();
            if self.imp().cache.borrow().contains(&uri) {
                continue;
            }
            let file = item.file();
            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = viewer)]
                self,
                async move {
                    if let Ok(texture) = crate::decode::full(&file, VIEW_MAX).await {
                        viewer.cache_texture(&uri, &texture);
                    }
                }
            ));
        }
    }

    fn cache_texture(&self, uri: &str, texture: &gdk::Texture) {
        let cost = texture.width() as u64 * texture.height() as u64 * 4;
        self.imp()
            .cache
            .borrow_mut()
            .put(uri.to_string(), texture.clone(), cost);
    }

    fn current_position(&self) -> u32 {
        self.imp()
            .filmstrip
            .borrow()
            .as_ref()
            .map_or(gtk::INVALID_LIST_POSITION, |f| f.selected())
    }

    // --- zoom ----------------------------------------------------------------

    fn set_texture(&self, texture: &gdk::Texture) {
        let imp = self.imp();
        imp.natural.set((texture.width(), texture.height()));
        imp.picture.set_paintable(Some(texture));
        self.zoom_fit();
    }

    fn zoom_fit(&self) {
        let imp = self.imp();
        imp.zoom.set(None);
        imp.picture.set_content_fit(gtk::ContentFit::Contain);
        imp.picture.set_halign(gtk::Align::Fill);
        imp.picture.set_valign(gtk::Align::Fill);
        imp.picture.set_size_request(-1, -1);
    }

    fn zoom_by(&self, factor: f64) {
        let imp = self.imp();
        let (nw, nh) = imp.natural.get();
        if nw == 0 || nh == 0 {
            return;
        }
        // Starting from fit means "fit scale" ≈ 1.0 baseline for the first step.
        let current = imp.zoom.get().unwrap_or(1.0);
        let zoom = (current * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        imp.zoom.set(Some(zoom));
        imp.picture.set_content_fit(gtk::ContentFit::Fill);
        imp.picture.set_halign(gtk::Align::Center);
        imp.picture.set_valign(gtk::Align::Center);
        imp.picture.set_size_request(
            (nw as f64 * zoom).round() as i32,
            (nh as f64 * zoom).round() as i32,
        );
    }
}
