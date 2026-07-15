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

use adw::prelude::ActionRowExt;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use vitrine_engine::{Db, FileRecord, SizedLru};

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
        #[template_child]
        pub info_split: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub meta_name_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_folder_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_dimensions_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_size_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_format_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_date_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_camera_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_orientation_row: TemplateChild<adw::ActionRow>,

        /// Read-only index connection, opened lazily to look up metadata for the
        /// shown image (the writer lives on the indexer thread).
        pub read_db: RefCell<Option<Db>>,

        /// Shared, ordered image model (the same sorted model the grid shows).
        pub store: RefCell<Option<gio::ListModel>>,
        /// Filmstrip selection = the current image; single source of "current".
        pub filmstrip: RefCell<Option<gtk::SingleSelection>>,
        pub filmstrip_view: RefCell<Option<gtk::ListView>>,
        /// None = fit-to-window; Some(f) = zoom factor over the texture's pixels.
        pub zoom: Cell<Option<f64>>,
        /// Natural pixel size of the currently displayed texture.
        pub natural: Cell<(i32, i32)>,
        /// Viewer-resolution texture cache (large images).
        pub cache: TextureCache,
        /// Shared RAM thumbnail cache (for the filmstrip); set on `open`.
        pub thumb_cache: RefCell<Option<crate::thumbnails::ThumbCache>>,
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
                info_split: Default::default(),
                meta_name_row: Default::default(),
                meta_folder_row: Default::default(),
                meta_dimensions_row: Default::default(),
                meta_size_row: Default::default(),
                meta_format_row: Default::default(),
                meta_date_row: Default::default(),
                meta_camera_row: Default::default(),
                meta_orientation_row: Default::default(),
                read_db: RefCell::new(None),
                store: RefCell::new(None),
                filmstrip: RefCell::new(None),
                filmstrip_view: RefCell::new(None),
                zoom: Cell::new(None),
                natural: Cell::new((0, 0)),
                cache: Rc::new(RefCell::new(SizedLru::new(CACHE_BYTES))),
                thumb_cache: RefCell::new(None),
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
    /// `thumb_cache` is the window's shared RAM thumbnail cache.
    pub fn open(
        &self,
        store: gio::ListModel,
        position: u32,
        thumb_cache: crate::thumbnails::ThumbCache,
    ) {
        let imp = self.imp();
        *imp.store.borrow_mut() = Some(store.clone());
        *imp.thumb_cache.borrow_mut() = Some(thumb_cache);
        if let Some(filmstrip) = imp.filmstrip.borrow().as_ref() {
            filmstrip.set_model(Some(&store));
        }
        self.show_position(position);
        // Dev aid: VITRINE_INFO reveals the properties sidebar (for screenshots).
        if std::env::var_os("VITRINE_INFO").is_some() {
            imp.info_split.set_show_sidebar(true);
        }
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
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let pic = list_item.child().and_downcast::<gtk::Picture>().unwrap();
                let item = list_item.item().and_downcast::<ImageObject>().unwrap();
                viewer.bind_filmstrip_cell(list_item, &pic, &item);
            }
        ));
        factory.connect_unbind(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(pic) = list_item.child().and_downcast::<gtk::Picture>() {
                pic.set_paintable(gtk::gdk::Paintable::NONE);
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

    /// Fill one filmstrip cell from the shared RAM cache, or load it, guarding
    /// against the list item being recycled to a different image mid-load.
    fn bind_filmstrip_cell(
        &self,
        list_item: &gtk::ListItem,
        pic: &gtk::Picture,
        item: &ImageObject,
    ) {
        let Some(cache) = self.imp().thumb_cache.borrow().clone() else {
            return;
        };
        let key = crate::thumbnails::ram_key(&item.file().uri(), THUMB_SIZE);
        if let Some(texture) = cache.borrow_mut().get(&key).cloned() {
            pic.set_paintable(Some(&texture));
            return;
        }
        pic.set_paintable(gtk::gdk::Paintable::NONE);
        if item.has_failed() {
            return;
        }

        let renderer_widget = crate::thumbnails::renderer_source(pic);
        let file = item.file();
        let mtime = item.mtime();
        let item = item.clone();
        let list_item = list_item.downgrade();
        glib::spawn_future_local(async move {
            let Some(texture) =
                crate::thumbnails::load(file, mtime, THUMB_SIZE, renderer_widget).await
            else {
                item.mark_failed();
                return;
            };
            cache.borrow_mut().put(
                key,
                texture.clone(),
                crate::thumbnails::texture_cost(&texture),
            );
            if let Some(list_item) = list_item.upgrade() {
                let still = list_item.item().and_downcast::<ImageObject>();
                if still.as_ref() == Some(&item) {
                    if let Some(pic) = list_item.child().and_downcast::<gtk::Picture>() {
                        pic.set_paintable(Some(&texture));
                    }
                }
            }
        });
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
                    gdk::Key::_1 | gdk::Key::KP_1 => v.zoom_actual(),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        ));
        self.add_controller(keys);

        // Double-click the image toggles fit ↔ 100%.
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_PRIMARY);
        click.connect_pressed(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_, n_press, _, _| {
                if n_press == 2 {
                    v.zoom_toggle();
                }
            }
        ));
        imp.picture.add_controller(click);

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
        self.update_metadata(&item);

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

    // --- metadata sidebar ----------------------------------------------------

    /// Fill the properties sidebar for `item` from the index. Fields the index
    /// hasn't backfilled yet (enrichment still pending, or an un-indexed folder)
    /// show an em dash and fill in once the image is revisited.
    fn update_metadata(&self, item: &ImageObject) {
        let imp = self.imp();
        const DASH: &str = "—";

        imp.meta_name_row.set_subtitle(&item.display_name());
        let folder = item
            .file()
            .parent()
            .and_then(|p| p.path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| DASH.to_string());
        imp.meta_folder_row.set_subtitle(&folder);

        let record = self.lookup_record(item);
        let text = |value: Option<String>| value.unwrap_or_else(|| DASH.to_string());

        let dimensions = record
            .as_ref()
            .and_then(|r| r.width.zip(r.height))
            .map(|(w, h)| format!("{w} × {h}"));
        imp.meta_dimensions_row.set_subtitle(&text(dimensions));

        let size = record.as_ref().map(|r| human_size(r.size));
        imp.meta_size_row.set_subtitle(&text(size));

        let format = record.as_ref().and_then(|r| r.format.clone());
        imp.meta_format_row.set_subtitle(&text(format));

        let date = record.as_ref().and_then(|r| r.date_taken).map(format_date);
        imp.meta_date_row.set_subtitle(&text(date));

        let camera = record.as_ref().and_then(|r| r.camera.clone());
        imp.meta_camera_row.set_subtitle(&text(camera));

        let orientation = record
            .as_ref()
            .and_then(|r| r.orientation)
            .map(orientation_label);
        imp.meta_orientation_row.set_subtitle(&text(orientation));
    }

    /// Look up the index row for `item`, opening the read connection on first use.
    fn lookup_record(&self, item: &ImageObject) -> Option<FileRecord> {
        let path = item.file().path()?;
        let imp = self.imp();
        if imp.read_db.borrow().is_none() {
            match Db::open(crate::index::index_db_path()) {
                Ok(db) => *imp.read_db.borrow_mut() = Some(db),
                Err(e) => {
                    glib::g_warning!("vitrine", "viewer read db: {e}");
                    return None;
                }
            }
        }
        let db = imp.read_db.borrow();
        db.as_ref()?
            .file_by_path(&path.to_string_lossy())
            .ok()
            .flatten()
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
        // Starting from fit means "fit scale" ≈ 1.0 baseline for the first step.
        let current = self.imp().zoom.get().unwrap_or(1.0);
        self.apply_zoom(current * factor);
    }

    /// 100% — one image pixel per screen pixel (of the decoded texture).
    fn zoom_actual(&self) {
        self.apply_zoom(1.0);
    }

    /// Double-click / dedicated key toggles between fit and 100%.
    fn zoom_toggle(&self) {
        if self.imp().zoom.get().is_none() {
            self.zoom_actual();
        } else {
            self.zoom_fit();
        }
    }

    /// Set an absolute zoom factor over the texture's pixels and pan via the
    /// scroller.
    fn apply_zoom(&self, factor: f64) {
        let imp = self.imp();
        let (nw, nh) = imp.natural.get();
        if nw == 0 || nh == 0 {
            return;
        }
        let zoom = factor.clamp(ZOOM_MIN, ZOOM_MAX);
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

/// Human-readable file size (e.g. "1.2 MB"), via GLib's localized formatter.
fn human_size(bytes: i64) -> String {
    glib::format_size(bytes.max(0) as u64).to_string()
}

/// Format a unix-seconds capture time (stored UTC) as "YYYY-MM-DD HH:MM".
fn format_date(secs: i64) -> String {
    glib::DateTime::from_unix_utc(secs)
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A readable label for an EXIF orientation value (1..=8).
fn orientation_label(o: i64) -> String {
    match o {
        1 => "Normal",
        2 => "Mirrored horizontally",
        3 => "Rotated 180°",
        4 => "Mirrored vertically",
        5 => "Mirrored, rotated 90° CCW",
        6 => "Rotated 90° CW",
        7 => "Mirrored, rotated 90° CW",
        8 => "Rotated 90° CCW",
        _ => "Normal",
    }
    .to_string()
}
