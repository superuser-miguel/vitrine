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

use adw::prelude::{ActionRowExt, AlertDialogExt, AlertDialogExtManual, EntryRowExt};
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use vitrine_engine::{Db, FileRecord, SizedLru};

use crate::grid_cell::THUMB_SIZE;
use crate::image_object::ImageObject;
use crate::index::Annotator;

/// Star-rating range.
const MAX_STARS: i64 = 5;

/// Cap the longest edge of a decoded viewer texture. Bounds per-image memory
/// (≤ ~4096²·4 ≈ 64 MB) so the LRU holds a useful working set.
const VIEW_MAX: u32 = 4096;
/// Viewer texture cache budget.
const CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// Zoom multiplier per step; zoom range clamps around 1.0 (100%).
const ZOOM_STEP: f64 = 1.25;
const ZOOM_MIN: f64 = 0.05;
const ZOOM_MAX: f64 = 20.0;

/// Grace period before the wait spinner appears over a pending full decode:
/// slow decodes always get feedback, fast ones never flash it.
const SPINNER_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Max in-flight filmstrip thumbnail loads (bounds a fast filmstrip fling).
const FILM_INFLIGHT: usize = 8;
/// Cap on queued filmstrip loads; oldest (scrolled-past) dropped.
const FILM_QUEUE_CAP: usize = 96;

type TextureCache = Rc<RefCell<SizedLru<String, gdk::Texture>>>;

/// Decode for the viewer with the `VIEW_MAX` cap *enforced*: glycin's scale
/// request is best-effort, so an oversized frame is CPU-downscaled on a worker
/// thread before it ever reaches the main-thread GPU upload (an unbounded
/// full-res upload is a visible transition stall).
async fn decode_view(
    file: &gio::File,
    orientation: i32,
    crop: Option<(f64, f64, f64, f64)>,
    byte_size: i64,
) -> Option<gdk::Texture> {
    let texture = crate::decode::full(file, VIEW_MAX, byte_size).await.ok()?;
    let texture = crate::thumbnails::downscale_cpu(texture, VIEW_MAX).await?;
    crate::thumbnails::transform_cpu(texture, orientation, crop).await
}

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
        pub toolbar_view: TemplateChild<adw::ToolbarView>,
        #[template_child]
        pub fs_close_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub picture_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub loading_spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub filmstrip_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub zoom_in_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub zoom_out_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub zoom_fit_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub fullscreen_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub edit_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub edit_split: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub rotate_left_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub rotate_right_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub flip_h_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub flip_v_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub crop_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub crop_apply_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub crop_reset_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub undo_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub redo_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub save_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub save_as_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub crop_area: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub crop_confirm_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub crop_confirm_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub crop_cancel_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub filmstrip_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub info_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub info_split: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub rating_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub comment_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub tag_add_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub tag_chips: TemplateChild<gtk::FlowBox>,
        #[template_child]
        pub meta_name_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_folder_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub meta_folder_files_button: TemplateChild<gtk::Button>,
        /// The current image's parent folder, for the Folder row's two actions
        /// (browse in Vitrine / show in Files).
        pub meta_folder: RefCell<Option<gio::File>>,
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
        /// Routes annotation writes (rating/comment) to the writer thread.
        pub annotator: RefCell<Option<Annotator>>,
        /// The shown image's content hash (annotation key), if it's indexed.
        pub current_hash: RefCell<Option<String>>,
        /// Tags on the shown image, kept in sync optimistically: annotation
        /// writes are queued on the writer thread, so re-reading the index right
        /// after an edit would show the pre-edit state.
        pub current_tags: RefCell<Vec<String>>,
        /// The five rating star buttons, built in `setup_review`.
        pub stars: RefCell<Vec<gtk::Button>>,
        /// The rating currently shown (0–5).
        pub rating: Cell<i64>,
        /// Guards programmatic comment-row updates from re-triggering a write.
        pub setting_comment: Cell<bool>,

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
        /// Bounded filmstrip loader: queued cells to thumbnail + in-flight count,
        /// so fast filmstrip scrolling can't spawn thousands of decode futures
        /// (the same bug the grid's scheduler fixes).
        pub film_queue: RefCell<Vec<(glib::WeakRef<gtk::ListItem>, ImageObject, u32)>>,
        pub film_inflight: Cell<usize>,
        /// Per-image session edit history for undo/redo: uri → (states, index
        /// into states of the current one). A state is (orientation, crop).
        #[allow(clippy::type_complexity)]
        pub edit_history: RefCell<
            std::collections::HashMap<String, (Vec<(i32, Option<(f64, f64, f64, f64)>)>, usize)>,
        >,
        /// In-progress crop selection in crop_area widget coords (x, y, w, h).
        pub crop_sel: Cell<Option<(f64, f64, f64, f64)>>,
        pub crop_drag_start: Cell<(f64, f64)>,
        /// What this drag is doing: fresh rect, moving, or pulling corner i.
        pub crop_drag_mode: Cell<u8>,
        /// The selection as it was when the drag began (for move/resize).
        pub crop_orig: Cell<(f64, f64, f64, f64)>,
        /// Where the strip is *about* to be: `scroll_to`'s target, used as the
        /// centre until the hadjustment catches up (it updates asynchronously,
        /// and ordering/evicting by the stale value starved the very cells the
        /// user was looking at on viewer open). Cleared on manual scroll.
        pub film_center_hint: Cell<Option<u32>>,
        /// The uri+edit-key the displayed pane is waiting a full decode for
        /// (None once the shown texture is final). Keys both the grace-period
        /// spinner and the arrival-time "apply or just cache?" decision.
        pub loading_uri: RefCell<Option<String>>,
        /// Viewer decodes in flight, by uri+edit-key: without this a fast flip
        /// decodes the same neighbour twice — once as a prefetch, once on
        /// arrival — doubling the transient full-size frames (V-24).
        pub decode_inflight: RefCell<std::collections::HashSet<String>>,
    }

    impl Default for VitrineViewer {
        fn default() -> Self {
            Self {
                title: Default::default(),
                picture: Default::default(),
                toolbar_view: Default::default(),
                fs_close_button: Default::default(),
                picture_scroller: Default::default(),
                filmstrip_scroller: Default::default(),
                zoom_in_button: Default::default(),
                zoom_out_button: Default::default(),
                zoom_fit_button: Default::default(),
                fullscreen_button: Default::default(),
                edit_button: Default::default(),
                edit_split: Default::default(),
                rotate_left_button: Default::default(),
                rotate_right_button: Default::default(),
                flip_h_button: Default::default(),
                flip_v_button: Default::default(),
                crop_button: Default::default(),
                crop_apply_button: Default::default(),
                crop_reset_button: Default::default(),
                undo_button: Default::default(),
                redo_button: Default::default(),
                save_row: Default::default(),
                save_as_row: Default::default(),
                crop_area: Default::default(),
                crop_confirm_box: Default::default(),
                crop_confirm_button: Default::default(),
                crop_cancel_button: Default::default(),
                filmstrip_button: Default::default(),
                info_button: Default::default(),
                info_split: Default::default(),
                rating_box: Default::default(),
                comment_row: Default::default(),
                tag_add_row: Default::default(),
                tag_chips: Default::default(),
                meta_name_row: Default::default(),
                meta_folder_row: Default::default(),
                meta_folder_files_button: Default::default(),
                meta_folder: RefCell::new(None),
                meta_dimensions_row: Default::default(),
                meta_size_row: Default::default(),
                meta_format_row: Default::default(),
                meta_date_row: Default::default(),
                meta_camera_row: Default::default(),
                meta_orientation_row: Default::default(),
                read_db: RefCell::new(None),
                annotator: RefCell::new(None),
                current_hash: RefCell::new(None),
                current_tags: RefCell::new(Vec::new()),
                stars: RefCell::new(Vec::new()),
                rating: Cell::new(0),
                setting_comment: Cell::new(false),
                store: RefCell::new(None),
                filmstrip: RefCell::new(None),
                filmstrip_view: RefCell::new(None),
                zoom: Cell::new(None),
                natural: Cell::new((0, 0)),
                cache: Rc::new(RefCell::new(SizedLru::new(CACHE_BYTES))),
                thumb_cache: RefCell::new(None),
                syncing: Cell::new(false),
                film_queue: RefCell::new(Vec::new()),
                film_inflight: Cell::new(0),
                edit_history: Default::default(),
                crop_sel: Cell::new(None),
                crop_drag_start: Cell::new((0.0, 0.0)),
                crop_drag_mode: Cell::new(0),
                crop_orig: Cell::new((0.0, 0.0, 0.0, 0.0)),
                film_center_hint: Cell::new(None),
                loading_spinner: Default::default(),
                loading_uri: RefCell::new(None),
                decode_inflight: Default::default(),
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
            obj.setup_review();
            obj.setup_tags();
            obj.setup_metadata();
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
        // Drop any filmstrip loads still queued from a previously-shown folder.
        imp.film_queue.borrow_mut().clear();
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

    /// Provide the annotation-write handle (rating/comment go through it).
    pub fn set_annotator(&self, annotator: Annotator) {
        *self.imp().annotator.borrow_mut() = Some(annotator);
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

        // Mouse wheel scrolls the horizontal filmstrip: a vertical notch (or a
        // trackpad's horizontal delta) moves the strip sideways. Without this the
        // strip only moved by keyboard — a vertical wheel does nothing to
        // horizontal content by default.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, dx, dy| {
                let hadj = viewer.imp().filmstrip_scroller.hadjustment();
                // Prefer an explicit horizontal delta (trackpad); else map the
                // vertical wheel to horizontal. ~1.5 thumbnails per notch.
                let delta = if dx != 0.0 { dx } else { dy };
                hadj.set_value(hadj.value() + delta * 108.0);
                // Manual scroll: the hadjustment is authoritative again.
                viewer.imp().film_center_hint.set(None);
                glib::Propagation::Stop
            }
        ));
        imp.filmstrip_scroller.add_controller(scroll);

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
        let key = crate::thumbnails::ram_key(&item.file().uri(), THUMB_SIZE)
            + &crate::thumbnails::edit_key(item.orientation(), item.crop());
        if let Some(texture) = cache.borrow_mut().get(&key).cloned() {
            pic.set_paintable(Some(&texture));
            // Coverage metric: bind-time RAM hits paint without the queue, so a
            // test can only prove "no starved cells" by unioning these with the
            // VDBG-FILM queue completions.
            if crate::debug::enabled() {
                eprintln!(
                    "VDBG-FILMBIND ms={} pos={} hit=true",
                    crate::debug::since_start_ms(),
                    list_item.position()
                );
            }
            return;
        }
        pic.set_paintable(gtk::gdk::Paintable::NONE);
        if crate::debug::enabled() {
            eprintln!(
                "VDBG-FILMBIND ms={} pos={} hit=false",
                crate::debug::since_start_ms(),
                list_item.position()
            );
        }
        if item.has_failed() {
            return;
        }

        // Enqueue for the bounded loader instead of spawning a decode future per
        // bind (a fast filmstrip fling would otherwise pile up thousands).
        {
            let mut q = self.imp().film_queue.borrow_mut();
            q.push((list_item.downgrade(), item.clone(), list_item.position()));
            if q.len() > FILM_QUEUE_CAP {
                // First shed entries whose cell died or recycled to another item
                // (they'd be skipped at pump time anyway, but they occupy cap
                // space and used to push *live* requests out — blank cells).
                q.retain(|(weak, it, _)| {
                    weak.upgrade()
                        .and_then(|li| li.item().and_downcast::<ImageObject>())
                        .as_ref()
                        == Some(it)
                });
            }
            if q.len() > FILM_QUEUE_CAP {
                // Still over: keep the requests nearest the visible strip (same
                // policy as the grid's load queue), never the oldest-vs-newest.
                let center = self.film_center();
                q.sort_by_key(|(_, _, pos)| (*pos as i64 - center).unsigned_abs());
                q.truncate(FILM_QUEUE_CAP);
            }
        }
        self.pump_filmstrip();
    }

    /// The item index at the centre of the filmstrip's visible range, estimated
    /// from the scroller's hadjustment (uniform cell widths). This is the
    /// viewport signal the loader orders by — without it, fill order was pure
    /// bind order (LIFO), which populated the strip backwards from offscreen
    /// overscan cells and let visible cells starve.
    fn film_center(&self) -> i64 {
        let n = self.n_items() as f64;
        let adj = self.imp().filmstrip_scroller.hadjustment();
        let upper = adj.upper();
        if n <= 0.0 || upper <= 0.0 {
            let sel = self.current_position();
            return if sel == gtk::INVALID_LIST_POSITION {
                0
            } else {
                sel as i64
            };
        }
        let derived = (((adj.value() + adj.page_size() / 2.0) / upper) * n) as i64;
        // Until the hadjustment reflects a pending scroll_to, trust the target:
        // ordering by the stale value made the cap evict the on-screen cells.
        if let Some(hint) = self.imp().film_center_hint.get() {
            let page_items = ((adj.page_size() / upper) * n) as i64;
            if (derived - hint as i64).abs() <= page_items.max(4) {
                self.imp().film_center_hint.set(None); // caught up
            } else {
                return hint as i64;
            }
        }
        derived
    }

    /// Spawn filmstrip loads up to the in-flight bound, nearest the visible
    /// centre first (mirrors the grid's `pop_best_load`).
    fn pump_filmstrip(&self) {
        let imp = self.imp();
        while imp.film_inflight.get() < FILM_INFLIGHT {
            let popped = {
                let center = self.film_center();
                let mut q = imp.film_queue.borrow_mut();
                q.iter()
                    .enumerate()
                    .min_by_key(|(_, (_, _, pos))| (*pos as i64 - center).unsigned_abs())
                    .map(|(i, _)| i)
                    .map(|i| q.swap_remove(i))
            };
            let Some((weak_li, item, pos)) = popped else {
                break;
            };
            // Skip cells that recycled to another item before their turn.
            let Some(li) = weak_li.upgrade() else {
                continue;
            };
            if li.item().and_downcast::<ImageObject>().as_ref() != Some(&item) {
                continue;
            }
            imp.film_inflight.set(imp.film_inflight.get() + 1);
            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = viewer)]
                self,
                async move {
                    viewer.run_filmstrip_load(weak_li, item, pos).await;
                    let imp = viewer.imp();
                    imp.film_inflight
                        .set(imp.film_inflight.get().saturating_sub(1));
                    viewer.pump_filmstrip();
                }
            ));
        }
    }

    async fn run_filmstrip_load(
        &self,
        weak_li: glib::WeakRef<gtk::ListItem>,
        item: ImageObject,
        pos: u32,
    ) {
        let Some(cache) = self.imp().thumb_cache.borrow().clone() else {
            return;
        };
        let orientation = item.orientation();
        let crop = item.crop();
        let key = crate::thumbnails::ram_key(&item.file().uri(), THUMB_SIZE)
            + &crate::thumbnails::edit_key(orientation, crop);
        let cached = cache.borrow_mut().get(&key).cloned();
        let texture = if cached.is_some() {
            cached
        } else {
            let renderer = crate::thumbnails::renderer_source(self);
            let loaded = crate::thumbnails::load(
                item.file(),
                item.mtime(),
                THUMB_SIZE,
                item.size(),
                renderer,
            )
            .await;
            let loaded = match loaded {
                Some(t) => crate::thumbnails::transform_cpu(t, orientation, crop).await,
                None => None,
            };
            match &loaded {
                Some(t) => {
                    cache
                        .borrow_mut()
                        .put(key, t.clone(), crate::thumbnails::texture_cost(t))
                }
                None => item.mark_failed(),
            }
            loaded
        };
        if let (Some(t), Some(li)) = (texture, weak_li.upgrade()) {
            if li.item().and_downcast::<ImageObject>().as_ref() == Some(&item) {
                if let Some(pic) = li.child().and_downcast::<gtk::Picture>() {
                    pic.set_paintable(Some(&t));
                    // Fill-order metric (§13.3): where this completion landed
                    // relative to the strip's visible centre at that moment.
                    if crate::debug::enabled() {
                        eprintln!(
                            "VDBG-FILM ms={} pos={pos} center={}",
                            crate::debug::since_start_ms(),
                            self.film_center()
                        );
                    }
                }
            }
        }
    }

    /// Immersive lightbox: fill the screen with just the image — fullscreen the
    /// window, hide the header bar and filmstrip, and show a floating ✕ close
    /// button so the mode is always exitable (Escape and the ✕ both leave it).
    /// Exiting restores the chrome (the filmstrip to whatever its toggle says).
    fn set_fullscreen(&self, on: bool) {
        let imp = self.imp();
        if let Some(win) = self.root().and_downcast::<gtk::Window>() {
            win.set_fullscreened(on);
        }
        imp.toolbar_view.set_reveal_top_bars(!on);
        imp.fs_close_button.set_visible(on);
        if on {
            imp.filmstrip_scroller.set_visible(false);
        } else {
            imp.filmstrip_scroller
                .set_visible(imp.filmstrip_button.is_active());
        }
        imp.fullscreen_button.set_icon_name(if on {
            "view-restore-symbolic"
        } else {
            "view-fullscreen-symbolic"
        });
    }

    /// Whether the immersive lightbox is active.
    fn is_fullscreen(&self) -> bool {
        self.imp().fullscreen_button.is_active()
    }

    /// F11: plain whole-app fullscreen (chrome stays) — distinct from the viewer's
    /// immersive lightbox. If the lightbox is active, F11 just exits it cleanly.
    fn toggle_app_fullscreen(&self) {
        if self.is_fullscreen() {
            self.imp().fullscreen_button.set_active(false);
            return;
        }
        if let Some(win) = self.root().and_downcast::<gtk::Window>() {
            win.set_fullscreened(!win.is_fullscreen());
        }
    }

    /// Show/hide the Properties sidebar (used by the VITRINE_SOAK journey).
    pub fn set_properties_shown(&self, shown: bool) {
        self.imp().info_split.set_show_sidebar(shown);
    }

    /// Scroll the filmstrip to `fraction` of its range (VITRINE_SOAK).
    pub fn soak_scroll_filmstrip_to(&self, fraction: f64) {
        let adj = self.imp().filmstrip_scroller.hadjustment();
        let span = (adj.upper() - adj.page_size() - adj.lower()).max(0.0);
        adj.set_value(adj.lower() + span * fraction.clamp(0.0, 1.0));
        // Emulates a manual scroll — hadjustment is authoritative (see the
        // wheel handler); a pending scroll_to hint would misorder the queue.
        self.imp().film_center_hint.set(None);
    }

    /// Apply one edit-card transform to the current image: compose onto the
    /// stored orientation, persist (content-hash keyed, file untouched), and
    /// re-show — the new cache key misses, so the oriented texture is derived.
    fn apply_orient(&self, op: vitrine_engine::OrientOp) {
        let pos = self.current_position();
        let Some(item) = self.item_at(pos) else {
            return;
        };
        let next = vitrine_engine::compose_orientation(item.orientation() as i64, op) as i32;
        self.commit_edit_state(&item, next, item.crop());
        self.show_position(pos);
    }

    /// Persist one edit state (orientation + crop) for `item`, recording it on
    /// the per-image undo history (any redo tail is discarded).
    fn commit_edit_state(
        &self,
        item: &ImageObject,
        orientation: i32,
        crop: Option<(f64, f64, f64, f64)>,
    ) {
        {
            let mut hist = self.imp().edit_history.borrow_mut();
            let (states, idx) = hist
                .entry(item.file().uri().to_string())
                .or_insert_with(|| (vec![(item.orientation(), item.crop())], 0));
            states.truncate(*idx + 1);
            states.push((orientation, crop));
            *idx = states.len() - 1;
        }
        self.set_edit_state(item, orientation, crop);
        self.sync_history_buttons(item);
    }

    /// Apply + persist a state without touching history (undo/redo path).
    fn set_edit_state(
        &self,
        item: &ImageObject,
        orientation: i32,
        crop: Option<(f64, f64, f64, f64)>,
    ) {
        item.set_orientation(orientation);
        item.set_crop(crop);
        let hash = item.content_hash();
        if !hash.is_empty() {
            if let Some(annotator) = self.imp().annotator.borrow().as_ref() {
                annotator.set_orientation(&hash, orientation as i64);
                annotator.set_crop(&hash, crop);
            }
        }
    }

    fn sync_history_buttons(&self, item: &ImageObject) {
        let hist = self.imp().edit_history.borrow();
        let (can_undo, can_redo) = match hist.get(&item.file().uri().to_string()) {
            Some((states, idx)) => (*idx > 0, *idx + 1 < states.len()),
            None => (false, false),
        };
        self.imp().undo_button.set_sensitive(can_undo);
        self.imp().redo_button.set_sensitive(can_redo);
    }

    fn history_step(&self, delta: i64) {
        let pos = self.current_position();
        let Some(item) = self.item_at(pos) else {
            return;
        };
        let state = {
            let mut hist = self.imp().edit_history.borrow_mut();
            let Some((states, idx)) = hist.get_mut(&item.file().uri().to_string()) else {
                return;
            };
            let next = (*idx as i64 + delta).clamp(0, states.len() as i64 - 1) as usize;
            if next == *idx {
                return;
            }
            *idx = next;
            states[next]
        };
        self.set_edit_state(&item, state.0, state.1);
        self.sync_history_buttons(&item);
        self.show_position(pos);
    }

    /// Wire the edit card's crop mode, undo/redo, and Save rows.
    fn setup_edit_card(&self) {
        let imp = self.imp();

        // Crop mode: overlay visible + fit zoom while selecting.
        imp.crop_button.connect_toggled(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |b| {
                let imp = v.imp();
                imp.crop_sel.set(None);
                imp.crop_apply_button.set_sensitive(false);
                imp.crop_confirm_button.set_sensitive(false);
                imp.crop_area.set_visible(b.is_active());
                imp.crop_confirm_box.set_visible(b.is_active());
                if b.is_active() {
                    v.zoom_fit();
                }
                imp.crop_area.queue_draw();
            }
        ));

        let drag = gtk::GestureDrag::new();
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_, x, y| {
                let imp = v.imp();
                imp.crop_drag_start.set((x, y));
                // Loupe behaviour: every handle stays adjustable until confirm.
                // Decide from where the press lands — a corner handle resizes,
                // inside the rect moves, anywhere else starts a fresh rect.
                let mode = match imp.crop_sel.get() {
                    Some(r @ (rx, ry, rw, rh)) => {
                        const GRAB: f64 = 22.0;
                        let corners = [(rx, ry), (rx + rw, ry), (rx, ry + rh), (rx + rw, ry + rh)];
                        let hit = corners
                            .iter()
                            .position(|(cx, cy)| (x - cx).abs() < GRAB && (y - cy).abs() < GRAB);
                        match hit {
                            Some(i) => {
                                imp.crop_orig.set(r);
                                2 + i as u8
                            }
                            None if x > rx && x < rx + rw && y > ry && y < ry + rh => {
                                imp.crop_orig.set(r);
                                1
                            }
                            None => 0,
                        }
                    }
                    None => 0,
                };
                imp.crop_drag_mode.set(mode);
                if mode == 0 {
                    imp.crop_sel.set(None);
                }
                imp.crop_area.queue_draw();
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_, dx, dy| {
                let imp = v.imp();
                let (sx, sy) = imp.crop_drag_start.get();
                let sel = match imp.crop_drag_mode.get() {
                    0 => {
                        let (x0, x1) = if dx < 0.0 {
                            (sx + dx, sx)
                        } else {
                            (sx, sx + dx)
                        };
                        let (y0, y1) = if dy < 0.0 {
                            (sy + dy, sy)
                        } else {
                            (sy, sy + dy)
                        };
                        (x0, y0, x1 - x0, y1 - y0)
                    }
                    1 => {
                        let (ox, oy, ow, oh) = imp.crop_orig.get();
                        (ox + dx, oy + dy, ow, oh)
                    }
                    m => {
                        // Pull one corner; the opposite corner stays anchored.
                        let (ox, oy, ow, oh) = imp.crop_orig.get();
                        let (ax, ay) = match m - 2 {
                            0 => (ox + ow, oy + oh), // dragging top-left
                            1 => (ox, oy + oh),      // top-right
                            2 => (ox + ow, oy),      // bottom-left
                            _ => (ox, oy),           // bottom-right
                        };
                        let (px, py) = (sx + dx, sy + dy);
                        (px.min(ax), py.min(ay), (px - ax).abs(), (py - ay).abs())
                    }
                };
                imp.crop_sel.set(Some(sel));
                let valid = sel.2 > 8.0 && sel.3 > 8.0;
                imp.crop_apply_button.set_sensitive(valid);
                imp.crop_confirm_button.set_sensitive(valid);
                imp.crop_area.queue_draw();
            }
        ));
        imp.crop_area.add_controller(drag);

        // Live resize cursors over the handles (Loupe's affordance): motion
        // updates the cursor to the matching resize/move shape.
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_, x, y| {
                let imp = v.imp();
                let name = match imp.crop_sel.get() {
                    Some((rx, ry, rw, rh)) => {
                        const GRAB: f64 = 22.0;
                        let corners = [
                            (rx, ry, "nw-resize"),
                            (rx + rw, ry, "ne-resize"),
                            (rx, ry + rh, "sw-resize"),
                            (rx + rw, ry + rh, "se-resize"),
                        ];
                        corners
                            .iter()
                            .find(|(cx, cy, _)| (x - cx).abs() < GRAB && (y - cy).abs() < GRAB)
                            .map(|(_, _, n)| *n)
                            .unwrap_or(if x > rx && x < rx + rw && y > ry && y < ry + rh {
                                "move"
                            } else {
                                "crosshair"
                            })
                    }
                    None => "crosshair",
                };
                imp.crop_area.set_cursor_from_name(Some(name));
            }
        ));
        imp.crop_area.add_controller(motion);

        imp.crop_area.set_draw_func(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_, cr, w, h| {
                // Dim everything outside the selection; white border around it.
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
                if let Some((x, y, sw, sh)) = v.imp().crop_sel.get() {
                    cr.rectangle(0.0, 0.0, w as f64, h as f64);
                    cr.rectangle(x, y + sh, sw, -sh); // negative = punch hole
                    cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
                    let _ = cr.fill();
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                    cr.set_line_width(1.5);
                    cr.rectangle(x, y, sw, sh);
                    let _ = cr.stroke();
                    // Corner handles (Loupe-style): filled squares to grab.
                    const HS: f64 = 5.0;
                    for (cx, cy) in [(x, y), (x + sw, y), (x, y + sh), (x + sw, y + sh)] {
                        cr.rectangle(cx - HS, cy - HS, HS * 2.0, HS * 2.0);
                        let _ = cr.fill();
                    }
                } else {
                    cr.rectangle(0.0, 0.0, w as f64, h as f64);
                    let _ = cr.fill();
                }
            }
        ));

        imp.crop_apply_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.apply_crop_selection()
        ));
        imp.crop_reset_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| {
                let pos = v.current_position();
                if let Some(item) = v.item_at(pos) {
                    if item.crop().is_some() {
                        v.commit_edit_state(&item, item.orientation(), None);
                        v.show_position(pos);
                    }
                }
                v.imp().crop_button.set_active(false);
            }
        ));

        imp.crop_confirm_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.apply_crop_selection()
        ));
        imp.crop_cancel_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.imp().crop_button.set_active(false)
        ));

        imp.undo_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.history_step(-1)
        ));
        imp.redo_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.history_step(1)
        ));

        imp.save_as_row.connect_activated(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.save_as()
        ));
        imp.save_row.connect_activated(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.confirm_save()
        ));
    }

    /// Map the widget-space selection to a normalized display-space rect,
    /// compose it with any existing crop (a crop of a crop), and commit.
    fn apply_crop_selection(&self) {
        let imp = self.imp();
        let Some((sx, sy, sw, sh)) = imp.crop_sel.get() else {
            return;
        };
        let pos = self.current_position();
        let Some(item) = self.item_at(pos) else {
            return;
        };
        // Displayed image rect inside crop_area (contain-fit, centred; crop
        // mode forces fit zoom so this geometry holds).
        let (aw, ah) = (imp.crop_area.width() as f64, imp.crop_area.height() as f64);
        let (nw, nh) = imp.natural.get();
        if nw <= 0 || nh <= 0 || aw <= 0.0 || ah <= 0.0 {
            return;
        }
        let scale = (aw / nw as f64).min(ah / nh as f64);
        let (dw, dh) = (nw as f64 * scale, nh as f64 * scale);
        let (ox, oy) = ((aw - dw) / 2.0, (ah - dh) / 2.0);
        // Intersect selection with the displayed rect, then normalize.
        let x0 = (sx.max(ox) - ox) / dw;
        let y0 = (sy.max(oy) - oy) / dh;
        let x1 = ((sx + sw).min(ox + dw) - ox) / dw;
        let y1 = ((sy + sh).min(oy + dh) - oy) / dh;
        if x1 - x0 <= 0.0 || y1 - y0 <= 0.0 {
            return;
        }
        let sel = (x0, y0, x1 - x0, y1 - y0);
        // Compose onto the existing crop: sel is relative to the *current*
        // (already-cropped) display.
        let global = match item.crop() {
            Some((ex, ey, ew, eh)) => (ex + sel.0 * ew, ey + sel.1 * eh, sel.2 * ew, sel.3 * eh),
            None => sel,
        };
        self.commit_edit_state(&item, item.orientation(), Some(global));
        imp.crop_button.set_active(false);
        self.show_position(pos);
    }

    /// Bake the current instructions into RGBA at full resolution and encode
    /// for `dest`. Returns the encoded bytes off the main thread.
    async fn bake(&self, item: &ImageObject, dest_ext: String) -> Option<Vec<u8>> {
        let file = item.file();
        let orientation = item.orientation();
        let crop = item.crop();
        let texture = crate::decode::full(&file, u32::MAX, item.size())
            .await
            .ok()?;
        gio::spawn_blocking(move || {
            let w = texture.width() as u32;
            let h = texture.height() as u32;
            let (bytes, stride) = {
                let mut d = gdk::TextureDownloader::new(&texture);
                d.set_format(gdk::MemoryFormat::R8g8b8a8);
                d.download_bytes()
            };
            drop(texture);
            let (bytes, w, h) = match vitrine_engine::orient_rgba(
                &bytes,
                w,
                h,
                stride as u32,
                orientation as i64,
            ) {
                Some((o, ow, oh)) => (o, ow, oh),
                None => {
                    let (t, tw, th) =
                        vitrine_engine::resize_rgba(&bytes, w, h, stride as u32, u32::MAX)?;
                    (t, tw, th)
                }
            };
            let (bytes, w, h) = match crop {
                Some(rect) => vitrine_engine::crop_rgba(&bytes, w, h, w * 4, rect)?,
                None => (bytes, w, h),
            };
            vitrine_engine::encode_baked(&bytes, w, h, &dest_ext)
        })
        .await
        .ok()
        .flatten()
    }

    fn save_as(&self) {
        let pos = self.current_position();
        let Some(item) = self.item_at(pos) else {
            return;
        };
        let name = item.display_name();
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), e.to_string()),
            None => (name.clone(), "jpg".to_string()),
        };
        let dialog = gtk::FileDialog::builder()
            .title(gettextrs::gettext("Save Edited Copy"))
            .initial_name(format!("{stem}-edited.{ext}"))
            .build();
        let win = self.root().and_downcast::<gtk::Window>();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = v)]
            self,
            async move {
                let Ok(dest) = dialog.save_future(win.as_ref()).await else {
                    return;
                };
                let Some(path) = dest.path() else { return };
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "jpg".into());
                match v.bake(&item, ext).await {
                    Some(bytes) => {
                        let ok = gio::spawn_blocking(move || std::fs::write(&path, bytes).is_ok())
                            .await
                            .unwrap_or(false);
                        if !ok {
                            glib::g_warning!("vitrine", "save-as write failed");
                        }
                    }
                    None => glib::g_warning!("vitrine", "save-as bake failed"),
                }
            }
        ));
    }

    fn confirm_save(&self) {
        let dialog = adw::AlertDialog::new(
            Some(&gettextrs::gettext("Save Edits?")),
            Some(&gettextrs::gettext(
                "The edits will be baked into the original file. This cannot be undone.",
            )),
        );
        dialog.add_responses(&[
            ("cancel", &gettextrs::gettext("Cancel")),
            ("save", &gettextrs::gettext("Save")),
        ]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Destructive);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = v)]
            self,
            async move {
                if dialog.choose_future(Some(&v)).await == "save" {
                    v.save_in_place().await;
                }
            }
        ));
    }

    /// Bake into the original file: write-temp + rename, re-hash, move the
    /// annotations to the new identity, clear the (now baked-in) instructions.
    async fn save_in_place(&self) {
        let pos = self.current_position();
        let Some(item) = self.item_at(pos) else {
            return;
        };
        let Some(path) = item.file().path() else {
            return;
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| "jpg".into());
        let Some(bytes) = self.bake(&item, ext).await else {
            glib::g_warning!("vitrine", "save bake failed");
            return;
        };
        let old_hash = item.content_hash();
        let write_path = path.clone();
        let new_hash = gio::spawn_blocking(move || {
            let tmp = write_path.with_extension("vitrine-tmp");
            std::fs::write(&tmp, &bytes).ok()?;
            std::fs::rename(&tmp, &write_path).ok()?;
            vitrine_engine::blake3_file(&write_path).ok()
        })
        .await
        .ok()
        .flatten();
        let Some(new_hash) = new_hash else {
            glib::g_warning!("vitrine", "save write/rehash failed");
            return;
        };
        if !old_hash.is_empty() {
            if let Some(annotator) = self.imp().annotator.borrow().as_ref() {
                annotator.rekey(&old_hash, &new_hash);
            }
        }
        item.set_content_hash(&new_hash);
        // Instructions are in the pixels now — identity state, fresh history.
        item.set_orientation(1);
        item.set_crop(None);
        self.imp()
            .edit_history
            .borrow_mut()
            .remove(&item.file().uri().to_string());
        self.sync_history_buttons(&item);
        // Evict RAM entries for this uri (viewer + thumbs at common buckets),
        // then re-show; the disk cache self-invalidates via the mtime check.
        let uri = item.file().uri().to_string();
        self.imp().cache.borrow_mut().remove(&uri);
        if let Some(thumbs) = self.imp().thumb_cache.borrow().as_ref() {
            for px in [128u32, 256, 512, 1024] {
                thumbs
                    .borrow_mut()
                    .remove(&crate::thumbnails::ram_key(&uri, px));
            }
        }
        self.show_position(pos);
    }

    fn setup_controls(&self) {
        let imp = self.imp();

        // The edit card is a mode: opening it closes Properties, and vice
        // versa, so the right edge always shows exactly one card.
        imp.edit_button.connect_toggled(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |b| {
                if b.is_active() {
                    v.imp().info_button.set_active(false);
                }
            }
        ));
        imp.info_button.connect_toggled(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |b| {
                if b.is_active() {
                    v.imp().edit_button.set_active(false);
                }
            }
        ));

        self.setup_edit_card();

        use vitrine_engine::OrientOp;
        for (button, op) in [
            (&imp.rotate_left_button, OrientOp::RotateCcw),
            (&imp.rotate_right_button, OrientOp::RotateCw),
            (&imp.flip_h_button, OrientOp::FlipH),
            (&imp.flip_v_button, OrientOp::FlipV),
        ] {
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = v)]
                self,
                move |_| v.apply_orient(op)
            ));
        }

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

        // Hide/show the filmstrip (more room for the image).
        imp.filmstrip_button.connect_toggled(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |btn| v.imp().filmstrip_scroller.set_visible(btn.is_active())
        ));

        // Immersive lightbox (also toggled by the ✕ overlay and Escape).
        imp.fullscreen_button.connect_toggled(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |btn| v.set_fullscreen(btn.is_active())
        ));
        imp.fs_close_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| v.imp().fullscreen_button.set_active(false)
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
                    // F11 = plain whole-app fullscreen (or exit the lightbox).
                    gdk::Key::F11 => v.toggle_app_fullscreen(),
                    // Escape leaves the immersive lightbox; otherwise it propagates
                    // (so the nav view can pop back to the grid).
                    gdk::Key::Escape if v.is_fullscreen() => {
                        v.imp().fullscreen_button.set_active(false);
                    }
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

        // Grab-hand pan: click-drag anywhere in the viewport to move a zoomed-in
        // image. The gesture is on the SCROLLER, not the picture — the scroller's
        // viewport stays fixed while the image scrolls inside it, so the drag
        // offset has a stable reference frame. (Attaching it to the picture, which
        // itself moves as you scroll, fed the motion back into the gesture and made
        // the image jitter against the edges.) Offsets are captured at drag-begin.
        let pan_start = std::rc::Rc::new(std::cell::Cell::new((0.0_f64, 0.0_f64)));
        let drag = gtk::GestureDrag::new();
        drag.set_button(gtk::gdk::BUTTON_PRIMARY);
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = v)]
            self,
            #[strong]
            pan_start,
            move |_, _, _| {
                if !v.is_pannable() {
                    return;
                }
                let imp = v.imp();
                pan_start.set((
                    imp.picture_scroller.hadjustment().value(),
                    imp.picture_scroller.vadjustment().value(),
                ));
                imp.picture.set_cursor_from_name(Some("grabbing"));
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = v)]
            self,
            #[strong]
            pan_start,
            move |_, offset_x, offset_y| {
                if !v.is_pannable() {
                    return;
                }
                let imp = v.imp();
                let (start_h, start_v) = pan_start.get();
                // set_value clamps to [lower, upper - page_size].
                imp.picture_scroller
                    .hadjustment()
                    .set_value(start_h - offset_x);
                imp.picture_scroller
                    .vadjustment()
                    .set_value(start_v - offset_y);
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_, _, _| v.update_pan_cursor()
        ));
        imp.picture_scroller.add_controller(drag);
    }

    /// True when the zoomed image is larger than the viewport on either axis, so
    /// there is something to pan.
    fn is_pannable(&self) -> bool {
        let imp = self.imp();
        let Some(zoom) = imp.zoom.get() else {
            return false; // fitting — nothing to pan
        };
        let (nw, nh) = imp.natural.get();
        let vw = imp.picture_scroller.width();
        let vh = imp.picture_scroller.height();
        (nw as f64 * zoom).round() as i32 > vw || (nh as f64 * zoom).round() as i32 > vh
    }

    /// Grab cursor over the image when it can be panned; default arrow otherwise.
    fn update_pan_cursor(&self) {
        let name = if self.is_pannable() {
            Some("grab")
        } else {
            None
        };
        self.imp().picture.set_cursor_from_name(name);
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

    /// Dev aid (`VITRINE_FLIPTEST`): mash Right `count` times, one `step(1)`
    /// every `interval_ms`, then wrap back to the start and keep going until
    /// the budget is spent. This is the V-24 reproduction — flipping *faster
    /// than a large image decodes*, which is what drove RSS to 3.2 GB before
    /// the fix. The interval is deliberately shorter than a cold full decode so
    /// the in-flight-dedup / bounded-hold path is the one under test, not a
    /// leisurely one-at-a-time walk.
    ///
    /// Each step logs a `VDBG-FLIP` line so the log is self-describing without
    /// needing to cross-reference the per-second HUD; the `VDBG fps` sampler
    /// still runs alongside and carries the RSS/stall truth.
    pub fn flip_test(&self, count: u32, interval_ms: u32) {
        let remaining = std::rc::Rc::new(std::cell::Cell::new(count));
        glib::timeout_add_local(
            std::time::Duration::from_millis(interval_ms.max(1) as u64),
            glib::clone!(
                #[weak(rename_to = v)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let left = remaining.get();
                    if left == 0 {
                        eprintln!("VDBG-FLIP done");
                        return glib::ControlFlow::Break;
                    }
                    // Wrap to the start at the end so a short folder still gets
                    // a long flip storm.
                    if v.current_position() + 1 >= v.n_items() {
                        v.show_position(0);
                    } else {
                        v.step(1);
                    }
                    eprintln!(
                        "VDBG-FLIP ms={} pos={} left={} rss={}MB",
                        crate::debug::since_start_ms(),
                        v.current_position(),
                        left - 1,
                        crate::debug::rss_mb(),
                    );
                    remaining.set(left - 1);
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    fn show_position(&self, pos: u32) {
        let Some(item) = self.item_at(pos) else {
            return;
        };
        let imp = self.imp();

        imp.title.set_title(&item.display_name());
        let record = self.lookup_record(&item);
        self.update_metadata(&item, record.as_ref());
        self.update_review(record.as_ref());

        // Keep the filmstrip selection + scroll in step without re-entering.
        imp.syncing.set(true);
        if let Some(filmstrip) = imp.filmstrip.borrow().as_ref() {
            filmstrip.set_selected(pos);
        }
        imp.syncing.set(false);
        if let Some(view) = imp.filmstrip_view.borrow().as_ref() {
            view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
            imp.film_center_hint.set(Some(pos));
        }

        // Display from cache, or decode; either way reset zoom to fit.
        let okey = crate::thumbnails::edit_key(item.orientation(), item.crop());
        let uri = item.file().uri().to_string() + &okey;
        if let Some(texture) = imp.cache.borrow_mut().get(&uri).cloned() {
            self.set_wait_state(None);
            self.set_texture(&texture);
        } else {
            // Instant preview: show the grid's RAM-cached thumbnail (upscaled,
            // soft) while the full decode runs, instead of a blank pane. The
            // full texture replaces it in place — same aspect, so the image
            // sharpens without jumping.
            let placeholder = imp.thumb_cache.borrow().clone().and_then(|cache| {
                let key = crate::thumbnails::ram_key(&item.file().uri(), THUMB_SIZE) + &okey;
                cache.borrow_mut().get(&key).cloned()
            });
            if crate::debug::enabled() {
                eprintln!(
                    "VDBG-VIEWER ms={} placeholder={}",
                    crate::debug::since_start_ms(),
                    placeholder.is_some()
                );
            }
            if let Some(thumb) = placeholder {
                self.set_texture(&thumb);
            }
            self.set_wait_state(Some(uri));
            self.ensure_loaded(&item);
        }
        self.prefetch(pos);
    }

    /// Mark what the pane is waiting on. `Some(uri)` arms the wait spinner
    /// after `SPINNER_GRACE` (fast decodes never flash it); `None` clears any
    /// pending wait and hides the spinner.
    fn set_wait_state(&self, waiting_on: Option<String>) {
        let imp = self.imp();
        match waiting_on {
            None => {
                imp.loading_uri.replace(None);
                imp.loading_spinner.stop();
                imp.loading_spinner.set_visible(false);
            }
            Some(uri) => {
                imp.loading_uri.replace(Some(uri.clone()));
                glib::timeout_add_local_once(
                    SPINNER_GRACE,
                    glib::clone!(
                        #[weak(rename_to = viewer)]
                        self,
                        move || {
                            let imp = viewer.imp();
                            // Only if the pane is *still* waiting on this decode.
                            if imp.loading_uri.borrow().as_deref() == Some(uri.as_str()) {
                                imp.loading_spinner.set_visible(true);
                                imp.loading_spinner.start();
                                if crate::debug::enabled() {
                                    eprintln!(
                                        "VDBG-SPINNER shown ms={}",
                                        crate::debug::since_start_ms()
                                    );
                                }
                            }
                        }
                    ),
                );
            }
        }
    }

    /// Decode `item` at viewer resolution and cache it, deduplicating
    /// in-flight decodes (a fast flip otherwise decodes the same neighbour
    /// twice — once as a prefetch, once on arrival; V-24). The texture is
    /// applied on landing only if the pane is still waiting on this exact
    /// uri — which also covers arriving at a still-loading prefetch.
    fn ensure_loaded(&self, item: &ImageObject) {
        let file = item.file();
        let orientation = item.orientation();
        let crop = item.crop();
        let byte_size = item.size();
        let uri = file.uri().to_string() + &crate::thumbnails::edit_key(orientation, crop);
        if !self.imp().decode_inflight.borrow_mut().insert(uri.clone()) {
            return;
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            async move {
                let texture = decode_view(&file, orientation, crop, byte_size).await;
                let imp = viewer.imp();
                imp.decode_inflight.borrow_mut().remove(&uri);
                let waited_on = imp.loading_uri.borrow().as_deref() == Some(uri.as_str());
                let Some(texture) = texture else {
                    // Decode failed: keep the placeholder, stop implying work.
                    if waited_on {
                        viewer.set_wait_state(None);
                    }
                    return;
                };
                viewer.cache_texture(&uri, &texture);
                if waited_on {
                    viewer.set_wait_state(None);
                    viewer.set_texture(&texture);
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
            let uri = item.file().uri().to_string()
                + &crate::thumbnails::edit_key(item.orientation(), item.crop());
            if self.imp().cache.borrow().contains(&uri) {
                continue;
            }
            self.ensure_loaded(&item);
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

    /// Wire the Folder row's two exits: activating the row browses the folder
    /// in Vitrine's grid; the suffix button shows it in the system file
    /// manager (via the OpenURI portal, so document paths work too).
    fn setup_metadata(&self) {
        let imp = self.imp();
        imp.meta_folder_row.connect_activated(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| {
                let Some(path) = v.imp().meta_folder.borrow().as_ref().and_then(|f| f.path())
                else {
                    return;
                };
                let _ = v.activate_action(
                    "win.browse-folder",
                    Some(&path.to_string_lossy().to_variant()),
                );
            }
        ));
        imp.meta_folder_files_button.connect_clicked(glib::clone!(
            #[weak(rename_to = v)]
            self,
            move |_| {
                let Some(folder) = v.imp().meta_folder.borrow().clone() else {
                    return;
                };
                let parent = v.root().and_downcast::<gtk::Window>();
                gtk::FileLauncher::new(Some(&folder)).launch(
                    parent.as_ref(),
                    gio::Cancellable::NONE,
                    |result| {
                        if let Err(err) = result {
                            glib::g_warning!("vitrine", "show folder in Files: {err}");
                        }
                    },
                );
            }
        ));
    }

    /// Fill the properties sidebar for `item` from the index. Fields the index
    /// hasn't backfilled yet (enrichment still pending, or an un-indexed folder)
    /// show an em dash and fill in once the image is revisited.
    fn update_metadata(&self, item: &ImageObject, record: Option<&FileRecord>) {
        let imp = self.imp();
        const DASH: &str = "—";

        imp.meta_name_row.set_subtitle(&item.display_name());
        // Show the folder as the user thinks of it (~-relative, portal doc
        // prefix stripped); the raw path stays available in the tooltip.
        let parent = item.file().parent();
        let raw = parent.as_ref().and_then(|p| p.path());
        let folder = raw
            .as_deref()
            .map(crate::window::scope_display)
            .unwrap_or_else(|| DASH.to_string());
        imp.meta_folder_row.set_subtitle(&folder);
        imp.meta_folder_row
            .set_tooltip_text(raw.as_deref().and_then(|p| p.to_str()));
        *imp.meta_folder.borrow_mut() = parent;

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
        self.ensure_read_db();
        let db = self.imp().read_db.borrow();
        db.as_ref()?
            .file_by_path(&path.to_string_lossy())
            .ok()
            .flatten()
    }

    /// Open the read-only index connection if not already open.
    fn ensure_read_db(&self) {
        let imp = self.imp();
        if imp.read_db.borrow().is_none() {
            match Db::open(crate::index::index_db_path()) {
                Ok(db) => *imp.read_db.borrow_mut() = Some(db),
                Err(e) => glib::g_warning!("vitrine", "viewer read db: {e}"),
            }
        }
    }

    // --- review (rating + comment) -------------------------------------------

    /// Build the five star buttons and wire the comment row (once, at construct).
    fn setup_review(&self) {
        let imp = self.imp();
        let mut stars = Vec::with_capacity(MAX_STARS as usize);
        for star in 1..=MAX_STARS {
            let button = gtk::Button::builder()
                .icon_name("non-starred-symbolic")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .build();
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = viewer)]
                self,
                move |_| viewer.on_star_clicked(star)
            ));
            imp.rating_box.append(&button);
            stars.push(button);
        }
        *imp.stars.borrow_mut() = stars;

        // Save the comment on Enter / apply-button (not per keystroke).
        imp.comment_row.connect_apply(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |row| {
                let imp = viewer.imp();
                if imp.setting_comment.get() {
                    return;
                }
                if let (Some(hash), Some(ann)) = (
                    imp.current_hash.borrow().clone(),
                    imp.annotator.borrow().clone(),
                ) {
                    ann.set_comment(&hash, row.text().as_str());
                }
            }
        ));
    }

    // --- tags ----------------------------------------------------------------

    /// Wire the "add a tag" row (once, at construct).
    fn setup_tags(&self) {
        self.imp().tag_add_row.connect_apply(glib::clone!(
            #[weak(rename_to = viewer)]
            self,
            move |row| {
                let name = row.text().trim().to_string();
                row.set_text("");
                if !name.is_empty() {
                    viewer.add_tag(&name);
                }
            }
        ));
    }

    /// Put `name` on the shown image.
    fn add_tag(&self, name: &str) {
        let imp = self.imp();
        let Some(hash) = imp.current_hash.borrow().clone() else {
            return;
        };
        // Tag names are unique case-insensitively in the index; mirror that here
        // so re-adding an existing tag doesn't produce a duplicate chip.
        if imp
            .current_tags
            .borrow()
            .iter()
            .any(|t| t.eq_ignore_ascii_case(name))
        {
            return;
        }
        let Some(annotator) = imp.annotator.borrow().clone() else {
            return;
        };
        if !annotator.tag(name, &[hash], true) {
            return; // writer thread gone — don't show a tag that won't persist
        }
        crate::debug::tag_action("add", name, 1);
        {
            let mut tags = imp.current_tags.borrow_mut();
            tags.push(name.to_string());
            tags.sort_by_key(|t| t.to_lowercase());
        }
        self.render_tag_chips();
    }

    /// Take `name` off the shown image — the only place a tag can be removed.
    fn remove_tag(&self, name: &str) {
        let imp = self.imp();
        let Some(hash) = imp.current_hash.borrow().clone() else {
            return;
        };
        let Some(annotator) = imp.annotator.borrow().clone() else {
            return;
        };
        if !annotator.tag(name, &[hash], false) {
            return;
        }
        crate::debug::tag_action("remove", name, 1);
        imp.current_tags
            .borrow_mut()
            .retain(|t| !t.eq_ignore_ascii_case(name));
        self.render_tag_chips();
    }

    /// Rebuild the chip cloud from `current_tags`. Each chip removes its own tag.
    fn render_tag_chips(&self) {
        let flowbox = &self.imp().tag_chips;
        while let Some(child) = flowbox.first_child() {
            flowbox.remove(&child);
        }
        for name in self.imp().current_tags.borrow().iter() {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            content.append(&gtk::Label::new(Some(name)));
            content.append(&gtk::Image::from_icon_name("window-close-symbolic"));

            let chip = gtk::Button::builder()
                .child(&content)
                .css_classes(["pill"])
                .tooltip_text(format!("{} “{name}”", gettextrs::gettext("Remove tag")))
                .build();
            chip.connect_clicked(glib::clone!(
                #[weak(rename_to = viewer)]
                self,
                #[strong]
                name,
                move |_| viewer.remove_tag(&name)
            ));
            flowbox.insert(&chip, -1);
        }
    }

    /// Load the shown image's tags from the index.
    fn update_tags(&self, hash: Option<&String>) {
        let imp = self.imp();
        let tags = match hash {
            Some(hash) => {
                self.ensure_read_db();
                let db = imp.read_db.borrow();
                db.as_ref()
                    .and_then(|db| db.tags_for_hash(hash).ok())
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        *imp.current_tags.borrow_mut() = tags;
        self.render_tag_chips();
        // No content hash (file not indexed yet) → nothing to key a tag to.
        imp.tag_add_row.set_sensitive(hash.is_some());
    }

    /// Load the review controls for the shown image from the index.
    fn update_review(&self, record: Option<&FileRecord>) {
        let imp = self.imp();
        let hash = record
            .map(|r| r.content_hash.clone())
            .filter(|h| !h.is_empty());

        let (rating, comment) = match &hash {
            Some(h) => (
                self.read_rating(h).unwrap_or(0),
                self.read_comment(h).unwrap_or_default(),
            ),
            None => (0, String::new()),
        };
        *imp.current_hash.borrow_mut() = hash.clone();

        self.render_stars(rating);
        imp.setting_comment.set(true);
        imp.comment_row.set_text(&comment);
        imp.setting_comment.set(false);

        self.update_tags(hash.as_ref());

        // No content hash (file not indexed yet) → nothing to key annotations to.
        let enabled = hash.is_some();
        imp.rating_box.set_sensitive(enabled);
        imp.comment_row.set_sensitive(enabled);
    }

    fn on_star_clicked(&self, star: i64) {
        // Clicking the current top star toggles the rating off.
        let new = if self.imp().rating.get() == star {
            0
        } else {
            star
        };
        self.apply_rating(new);
    }

    /// Set the rating (0 clears): update the viewer stars, the shared grid item
    /// (so its overlay repaints and the two views stay in sync), and persist via
    /// the annotator. Works for both the star buttons and the number keys.
    fn apply_rating(&self, rating: i64) {
        let imp = self.imp();
        let Some(hash) = imp.current_hash.borrow().clone() else {
            return;
        };
        self.render_stars(rating);
        if let Some(item) = self.item_at(self.current_position()) {
            item.set_rating(rating as i32);
        }
        if let Some(ann) = imp.annotator.borrow().as_ref() {
            ann.set_rating(&hash, if rating == 0 { None } else { Some(rating) });
        }
    }

    /// Fill the first `rating` stars, outline the rest; record the value.
    fn render_stars(&self, rating: i64) {
        let rating = rating.clamp(0, MAX_STARS);
        self.imp().rating.set(rating);
        for (i, button) in self.imp().stars.borrow().iter().enumerate() {
            let name = if (i as i64) < rating {
                "starred-symbolic"
            } else {
                "non-starred-symbolic"
            };
            button.set_icon_name(name);
        }
    }

    fn read_rating(&self, hash: &str) -> Option<i64> {
        self.ensure_read_db();
        let db = self.imp().read_db.borrow();
        db.as_ref()?.rating(hash).ok().flatten()
    }

    fn read_comment(&self, hash: &str) -> Option<String> {
        self.ensure_read_db();
        let db = self.imp().read_db.borrow();
        db.as_ref()?.comment(hash).ok().flatten()
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
        self.update_pan_cursor();
    }

    fn zoom_by(&self, factor: f64) {
        // Step from the current zoom, or — when fitting — from the *actual* fit
        // scale (which is rarely 100%), so the first +/- press is continuous
        // rather than jumping to an absolute fraction of the natural pixels.
        let current = self.imp().zoom.get().unwrap_or_else(|| self.fit_scale());
        self.apply_zoom(current * factor);
    }

    /// The scale at which the current image fits the viewport (the smaller of the
    /// two axis ratios) — the baseline a zoom step grows or shrinks from.
    fn fit_scale(&self) -> f64 {
        let imp = self.imp();
        let (nw, nh) = imp.natural.get();
        let vw = imp.picture_scroller.width();
        let vh = imp.picture_scroller.height();
        if nw <= 0 || nh <= 0 || vw <= 0 || vh <= 0 {
            return 1.0;
        }
        (vw as f64 / nw as f64).min(vh as f64 / nh as f64)
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
        // Contain (not Fill): the size-request box is already aspect-correct, so
        // Contain fills it with no letterbox — but unlike Fill it never distorts
        // the image if the scroller's viewport over-allocates the widget (which
        // is exactly what squashed the image vertically on zoom-out).
        imp.picture.set_content_fit(gtk::ContentFit::Contain);
        imp.picture.set_halign(gtk::Align::Center);
        imp.picture.set_valign(gtk::Align::Center);
        imp.picture.set_size_request(
            (nw as f64 * zoom).round() as i32,
            (nh as f64 * zoom).round() as i32,
        );
        self.update_pan_cursor();
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
