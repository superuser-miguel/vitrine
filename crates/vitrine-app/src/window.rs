//! The main application window: the browser grid and folder-open flow.
//!
//! Phase 1 browser. Opening a folder enumerates its images asynchronously into
//! a `gio::ListStore` of [`ImageObject`]s, shown in a virtualized `GtkGridView`
//! with `GtkMultiSelection` (rubber-band + Ctrl/Shift ranges). Thumbnails decode
//! lazily per visible cell (see [`crate::grid_cell`]). Activating a cell pushes
//! the [`crate::viewer`] page onto the `AdwNavigationView`, sharing the store.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use vitrine_engine::Db;

use crate::grid_cell::VitrineGridCell;
use crate::image_object::ImageObject;
use crate::index::{IndexProgress, Indexer};
use crate::viewer::VitrineViewer;

/// A sort criterion — the filesystem facts every item carries, so sorting is
/// instant and never waits on the background index (this is the Nautilus model).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Name,
    Size,
    Modified,
    Type,
}

impl SortField {
    /// The menu-action target string ↔ field.
    fn from_id(id: &str) -> SortField {
        match id {
            "size" => SortField::Size,
            "modified" => SortField::Modified,
            "type" => SortField::Type,
            _ => SortField::Name,
        }
    }

    fn id(self) -> &'static str {
        match self {
            SortField::Name => "name",
            SortField::Size => "size",
            SortField::Modified => "modified",
            SortField::Type => "type",
        }
    }
}

/// The active sort: a field plus direction. `Copy` so it lives in a `Cell` the
/// sorter closure reads on every comparison.
#[derive(Clone, Copy)]
pub struct SortState {
    field: SortField,
    descending: bool,
}

impl Default for SortState {
    fn default() -> Self {
        SortState {
            field: SortField::Name,
            descending: false,
        }
    }
}

/// A browsable location — for back-navigation history (a folder or a collection).
#[derive(Clone, PartialEq, Eq)]
pub enum Location {
    Folder(PathBuf),
    Collection(i64),
}

/// The browser filter bar's live state, read by the grid's `CustomFilter`.
#[derive(Default)]
pub struct FilterState {
    /// Minimum star rating (0 = any).
    min_rating: i32,
    /// If set, only items whose content hash is in this set pass (one tag).
    tag_hashes: Option<HashSet<String>>,
    /// The selected tag's name (for building a smart-collection predicate).
    tag_name: Option<String>,
}

/// Gio attributes fetched per child when enumerating a folder.
const ENUMERATE_ATTRS: &str = "standard::name,standard::display-name,standard::content-type,\
     standard::type,standard::size,time::modified";

/// Thumbnail display sizes (px) the +/- control steps through. Chosen so a
/// typical window spans ~1 column (largest) to ~6 (smallest) — below that is
/// uselessly tiny, so max-columns is also capped (see `setup_grid`).
const ICON_SIZES: &[u32] = &[128, 176, 240, 320, 448, 640];
/// Default icon-size index into `ICON_SIZES`.
const DEFAULT_ICON: usize = 2;
/// Never show more than this many columns, however wide the window / small the
/// icons (user preference: more than this is useless).
const MAX_COLUMNS: u32 = 7;

/// When scrolling settles, prefetch this many items past the last visible one
/// (and a few before) into the RAM cache, so resuming the scroll shows loaded
/// thumbnails instead of blanks.
const PREFETCH_AHEAD: u32 = 64;
const PREFETCH_BEHIND: u32 = 16;

/// A pending thumbnail load for the bounded scheduler. `cell = None` is a
/// prefetch (load into cache only); `Some` is a visible cell to paint when done.
pub struct LoadRequest {
    pub cell: Option<glib::WeakRef<VitrineGridCell>>,
    pub item: ImageObject,
    pub position: u32,
    pub load_size: u32,
}

/// Cap on queued (not-yet-started) load requests; farthest-from-viewport dropped.
const LOAD_QUEUE_CAP: usize = 512;

/// Max in-flight load futures — the bound that stops a fling from spawning
/// thousands of decode futures (the large-folder freeze). Override via
/// `VITRINE_LOAD_LIMIT`.
fn max_load_inflight() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VITRINE_LOAD_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(24)
    })
}

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/superuser_miguel/Vitrine/window.ui")]
    pub struct VitrineWindow {
        #[template_child]
        pub content_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub grid_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub places_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub places_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub new_collection_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub collections_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub bookmarks_heading: TemplateChild<gtk::Label>,
        #[template_child]
        pub bookmarks_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub folder_tree: TemplateChild<gtk::ListView>,
        #[template_child]
        pub back_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub forward_button: TemplateChild<gtk::Button>,
        /// Collection ids parallel to `collections_list` rows (by index).
        pub collection_ids: RefCell<Vec<i64>>,
        /// Bookmarks parallel to `bookmarks_list` rows (by index) — the UI's
        /// source of truth, synced to settings on every change.
        pub bookmarks: RefCell<Vec<crate::settings::Bookmark>>,
        /// Back-navigation history of visited locations (the current one excluded).
        pub history: RefCell<Vec<Location>>,
        /// Forward stack: locations we backed out of, re-reachable with Forward.
        pub forward: RefCell<Vec<Location>>,
        /// Where we are now (folder or collection), for history recording.
        pub current_location: RefCell<Option<Location>>,
        /// True while navigating via Back/Forward, so it doesn't re-record history.
        pub navigating_back: Cell<bool>,
        /// The lazily-built "Duplicates" page + its content box (rebuilt per scan).
        pub duplicates_page: RefCell<Option<adw::NavigationPage>>,
        pub duplicates_content: RefCell<Option<gtk::Box>>,
        /// Duplicate mode: false = exact (byte-identical), true = near (pHash).
        pub dedup_near: Cell<bool>,
        /// Bumped on every dedup scan so a slow off-thread result that finishes
        /// after the user switched modes (or left) is discarded, not rendered.
        pub dedup_generation: Cell<u64>,
        #[template_child]
        pub nav_view: TemplateChild<adw::NavigationView>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub icon_smaller: TemplateChild<gtk::Button>,
        #[template_child]
        pub icon_larger: TemplateChild<gtk::Button>,
        #[template_child]
        pub index_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub tag_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub filter_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub filter_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub rating_filter: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub tag_filter: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub filter_clear: TemplateChild<gtk::Button>,
        #[template_child]
        pub filter_save: TemplateChild<gtk::Button>,

        /// The grid filter (min-rating + tag); `changed()` re-runs it.
        pub filter: RefCell<Option<gtk::CustomFilter>>,
        /// Live filter criteria the `CustomFilter` reads.
        pub filter_state: Rc<RefCell<FilterState>>,
        /// Tag names parallel to `tag_filter` rows (offset by the "All tags" row).
        pub tag_names: RefCell<Vec<String>>,

        /// The tag popover's entry + existing-tag chip box (rebuilt on open).
        pub tag_entry: RefCell<Option<gtk::Entry>>,
        pub tag_flowbox: RefCell<Option<gtk::FlowBox>>,

        /// Background library indexer (created in `constructed`). Owns the one
        /// writer `Db`; the UI only enqueues folders and reads progress.
        pub indexer: RefCell<Option<Indexer>>,
        /// Read-only index connection, opened lazily to stamp the grid's items
        /// with their content hash + rating.
        pub read_db: RefCell<Option<Db>>,
        /// Local path of the folder currently shown (to scope the rating stamp).
        pub current_folder: RefCell<Option<PathBuf>>,

        /// Backing model for the grid (one row per image file); the mutation
        /// source (populate/trash act here).
        pub store: gio::ListStore,
        /// A sorted view of `store` — this is what the selection/grid show, so
        /// re-sorting is live and preserves selection/scroll.
        pub sort_model: RefCell<Option<gtk::SortListModel>>,
        /// The grid's sorter; `changed()` re-sorts when the criterion changes.
        pub sorter: RefCell<Option<gtk::CustomSorter>>,
        /// Active sort, shared with the sorter closure.
        pub sort_state: Rc<Cell<SortState>>,
        /// Selection model the grid renders.
        pub selection: RefCell<Option<gtk::MultiSelection>>,
        /// The grid view (its factory is rebuilt when the icon size changes).
        pub grid_view: RefCell<Option<gtk::GridView>>,
        /// Current icon-size index into `ICON_SIZES`.
        pub icon_index: std::cell::Cell<usize>,
        /// The viewer page, created lazily on first activation.
        pub viewer: RefCell<Option<VitrineViewer>>,
        /// Bounded RAM thumbnail cache, shared by the grid and the filmstrip.
        pub thumb_cache: crate::thumbnails::ThumbCache,
        /// Cells bound while scrolling that still need a thumbnail load (with the
        /// item's position); drained when scrolling settles so fast scroll doesn't
        /// spawn a load per cell.
        pub pending: RefCell<Vec<(glib::WeakRef<VitrineGridCell>, ImageObject, u32)>>,
        /// Bounded load scheduler: the priority queue of pending decode requests,
        /// the count of in-flight load futures, and the viewport-centre position
        /// (from the last flush) that orders the queue visible-first.
        pub load_queue: RefCell<Vec<LoadRequest>>,
        pub load_inflight: Cell<usize>,
        pub visible_center: Cell<u32>,
        /// Debounce timer for flushing `pending` after scrolling stops.
        pub flush_source: RefCell<Option<glib::SourceId>>,
    }

    impl Default for VitrineWindow {
        fn default() -> Self {
            Self {
                content_stack: Default::default(),
                grid_scroller: Default::default(),
                places_scroller: Default::default(),
                places_list: Default::default(),
                new_collection_button: Default::default(),
                collections_list: Default::default(),
                bookmarks_heading: Default::default(),
                bookmarks_list: Default::default(),
                folder_tree: Default::default(),
                back_button: Default::default(),
                forward_button: Default::default(),
                collection_ids: RefCell::new(Vec::new()),
                bookmarks: RefCell::new(Vec::new()),
                history: RefCell::new(Vec::new()),
                forward: RefCell::new(Vec::new()),
                current_location: RefCell::new(None),
                navigating_back: Cell::new(false),
                duplicates_page: RefCell::new(None),
                duplicates_content: RefCell::new(None),
                dedup_near: Cell::new(false),
                dedup_generation: Cell::new(0),
                nav_view: Default::default(),
                toast_overlay: Default::default(),
                icon_smaller: Default::default(),
                icon_larger: Default::default(),
                index_banner: Default::default(),
                tag_button: Default::default(),
                filter_button: Default::default(),
                filter_revealer: Default::default(),
                rating_filter: Default::default(),
                tag_filter: Default::default(),
                filter_clear: Default::default(),
                filter_save: Default::default(),
                filter: RefCell::new(None),
                filter_state: Rc::new(RefCell::new(FilterState::default())),
                tag_names: RefCell::new(Vec::new()),
                tag_entry: RefCell::new(None),
                tag_flowbox: RefCell::new(None),
                indexer: RefCell::new(None),
                read_db: RefCell::new(None),
                current_folder: RefCell::new(None),
                store: gio::ListStore::new::<ImageObject>(),
                sort_model: RefCell::new(None),
                sorter: RefCell::new(None),
                sort_state: Rc::new(Cell::new(SortState::default())),
                selection: RefCell::new(None),
                grid_view: RefCell::new(None),
                icon_index: std::cell::Cell::new(DEFAULT_ICON),
                viewer: RefCell::new(None),
                thumb_cache: crate::thumbnails::new_ram_cache(),
                pending: RefCell::new(Vec::new()),
                load_queue: RefCell::new(Vec::new()),
                load_inflight: Cell::new(0),
                visible_center: Cell::new(0),
                flush_source: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VitrineWindow {
        const NAME: &'static str = "VitrineWindow";
        type Type = super::VitrineWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            VitrineGridCell::ensure_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for VitrineWindow {
        fn constructed(&self) {
            self.parent_constructed();
            if crate::config::PROFILE == "Devel" {
                self.obj().add_css_class("devel");
            }
            self.obj().setup_grid();
            self.obj().setup_actions();
            self.obj().setup_indexer();
            self.obj().setup_tagging();
            self.obj().setup_collections();
            self.obj().setup_filtering();
            self.obj().setup_navigation();
            self.obj().setup_debug_hud();
            self.obj().maybe_soak();
            self.obj().maybe_openfolder_test();
            self.obj().maybe_cycle();
            self.obj().maybe_prefs();
        }
    }

    impl WidgetImpl for VitrineWindow {}
    impl WindowImpl for VitrineWindow {}
    impl ApplicationWindowImpl for VitrineWindow {}
    impl AdwApplicationWindowImpl for VitrineWindow {}
}

glib::wrapper! {
    pub struct VitrineWindow(ObjectSubclass<imp::VitrineWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl VitrineWindow {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    /// Build the grid: a `GtkGridView` + `GtkMultiSelection` whose factory is
    /// (re)built per icon size.
    fn setup_grid(&self) {
        let imp = self.imp();

        // Dev aid: VITRINE_ICON=<index> sets the initial icon-size level.
        if let Some(idx) = std::env::var("VITRINE_ICON")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            imp.icon_index.set(idx.min(ICON_SIZES.len() - 1));
        }

        // A live sorter over the store: comparisons read `sort_state`, so
        // changing the criterion is a `sorter.changed()` away — GTK re-sorts in
        // place, keeping selection and scroll (no rebuild). Sorting uses only
        // per-item filesystem facts, so it's instant and index-independent.
        // Filter (min-rating + tag) closest to the store, then sort. Both read
        // per-item facts already stamped onto the item — no DB hit per item.
        let filter_state = imp.filter_state.clone();
        let filter = gtk::CustomFilter::new(move |obj| {
            let Some(item) = obj.downcast_ref::<ImageObject>() else {
                return true;
            };
            let state = filter_state.borrow();
            if item.rating() < state.min_rating {
                return false;
            }
            if let Some(hashes) = &state.tag_hashes {
                if !hashes.contains(&item.content_hash()) {
                    return false;
                }
            }
            true
        });
        let filter_model = gtk::FilterListModel::new(Some(imp.store.clone()), Some(filter.clone()));

        let state = imp.sort_state.clone();
        let sorter = gtk::CustomSorter::new(move |a, b| {
            let a = a.downcast_ref::<ImageObject>().unwrap();
            let b = b.downcast_ref::<ImageObject>().unwrap();
            compare_images(a, b, state.get())
        });
        let sort_model = gtk::SortListModel::new(Some(filter_model), Some(sorter.clone()));
        *imp.filter.borrow_mut() = Some(filter);

        let selection = gtk::MultiSelection::new(Some(sort_model.clone()));
        let grid_view =
            gtk::GridView::new(Some(selection.clone()), None::<gtk::SignalListItemFactory>);
        // Columns flow from cell width; cap the max (do NOT raise it high — a
        // high cap makes GtkGridView realize a working set that scales with the
        // *folder* size, e.g. 800 cells for 800 images). 1..MAX_COLUMNS gives
        // the user's desired range with a constant, bounded working set.
        grid_view.set_min_columns(1);
        grid_view.set_max_columns(MAX_COLUMNS);
        grid_view.set_enable_rubberband(true);
        grid_view.set_vexpand(true);
        grid_view.set_factory(Some(&self.build_factory()));

        // Enter / double-click opens the viewer at that image.
        grid_view.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, position| window.open_viewer(position)
        ));

        // Delete trashes, Space previews, Ctrl +/-/0 changes icon size.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, mods| {
                use gtk::gdk::Key;
                let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                match (ctrl, key) {
                    (true, Key::plus | Key::equal | Key::KP_Add) => window.change_icon(1),
                    (true, Key::minus | Key::KP_Subtract) => window.change_icon(-1),
                    (true, Key::_0 | Key::KP_0) => window.reset_icon(),
                    (_, Key::Delete) => window.trash_selected(),
                    (_, Key::space | Key::KP_Space) => window.preview_selected(),
                    // Number keys rate the selection (no zoom in the grid, so 0–5
                    // are free); 0 clears.
                    (false, Key::_0 | Key::KP_0) => window.rate_selection(0),
                    (false, Key::_1 | Key::KP_1) => window.rate_selection(1),
                    (false, Key::_2 | Key::KP_2) => window.rate_selection(2),
                    (false, Key::_3 | Key::KP_3) => window.rate_selection(3),
                    (false, Key::_4 | Key::KP_4) => window.rate_selection(4),
                    (false, Key::_5 | Key::KP_5) => window.rate_selection(5),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        ));
        grid_view.add_controller(keys);

        // Ctrl+scroll on the grid zooms icon size.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |ctrl, _, dy| {
                if !ctrl
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    return glib::Propagation::Proceed;
                }
                window.change_icon(if dy < 0.0 { 1 } else { -1 });
                glib::Propagation::Stop
            }
        ));
        imp.grid_scroller.add_controller(scroll);

        imp.grid_scroller.set_child(Some(&grid_view));
        *imp.selection.borrow_mut() = Some(selection);
        *imp.grid_view.borrow_mut() = Some(grid_view);
        *imp.sort_model.borrow_mut() = Some(sort_model);
        *imp.sorter.borrow_mut() = Some(sorter);

        imp.icon_smaller.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.change_icon(-1)
        ));
        imp.icon_larger.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.change_icon(1)
        ));
    }

    /// (Re)start the debounce that flushes queued thumbnail loads. Every cell
    /// bind calls this, so during continuous scrolling the timer keeps resetting
    /// and nothing loads until scrolling pauses — keeping the main loop free.
    fn schedule_flush(&self) {
        let imp = self.imp();
        if let Some(id) = imp.flush_source.borrow_mut().take() {
            id.remove();
        }
        let id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(90),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.imp().flush_source.replace(None);
                    window.flush_pending();
                }
            ),
        );
        imp.flush_source.replace(Some(id));
    }

    /// Load thumbnails for the cells queued since the last flush that are still
    /// showing the item they were queued for (scrolled-past cells are skipped),
    /// then prefetch a margin around the visible range so resuming the scroll
    /// shows loaded thumbnails.
    fn flush_pending(&self) {
        let imp = self.imp();
        let pending: Vec<_> = imp.pending.borrow_mut().drain(..).collect();
        let (mut lo, mut hi) = (u32::MAX, 0u32);
        let load_size = self.icon_px().max(256);
        for (weak_cell, item, position) in pending {
            // Skip cells that recycled to a different item while we debounced.
            let live = weak_cell
                .upgrade()
                .is_some_and(|c| c.item().as_ref() == Some(&item));
            if !live {
                continue;
            }
            lo = lo.min(position);
            hi = hi.max(position);
            self.enqueue_load(LoadRequest {
                cell: Some(weak_cell),
                item,
                position,
                load_size,
            });
        }
        if lo <= hi {
            imp.visible_center.set((lo + hi) / 2);
            self.prefetch_range(lo, hi);
        }
        self.pump_loads();
    }

    /// Add a load request to the bounded scheduler's queue (coalescing re-binds of
    /// the same slot; a prefetch never clobbers a visible-cell request). Caller
    /// pumps when the batch is enqueued.
    fn enqueue_load(&self, req: LoadRequest) {
        let imp = self.imp();
        let mut q = imp.load_queue.borrow_mut();
        if let Some(existing) = q.iter_mut().find(|r| r.position == req.position) {
            if req.cell.is_some() || existing.cell.is_none() {
                *existing = req;
            }
        } else {
            q.push(req);
        }
        // Cap the queue: drop the farthest-from-viewport requests.
        if q.len() > LOAD_QUEUE_CAP {
            let center = imp.visible_center.get() as i64;
            q.sort_by_key(|r| (r.position as i64 - center).unsigned_abs());
            q.truncate(LOAD_QUEUE_CAP);
        }
    }

    /// Spawn load futures up to the in-flight bound, each pulling the queued
    /// request nearest the viewport (visible-first fill). This is what keeps a
    /// fling from spawning thousands of decode futures.
    fn pump_loads(&self) {
        let imp = self.imp();
        while imp.load_inflight.get() < max_load_inflight() {
            let Some(req) = self.pop_best_load() else {
                break;
            };
            imp.load_inflight.set(imp.load_inflight.get() + 1);
            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = window)]
                self,
                async move {
                    window.run_load(req).await;
                    let imp = window.imp();
                    imp.load_inflight
                        .set(imp.load_inflight.get().saturating_sub(1));
                    window.pump_loads();
                }
            ));
        }
    }

    /// Remove and return the queued request nearest the current viewport centre.
    fn pop_best_load(&self) -> Option<LoadRequest> {
        let imp = self.imp();
        let mut q = imp.load_queue.borrow_mut();
        let center = imp.visible_center.get() as i64;
        let idx = q
            .iter()
            .enumerate()
            .min_by_key(|(_, r)| (r.position as i64 - center).unsigned_abs())
            .map(|(i, _)| i)?;
        Some(q.swap_remove(idx))
    }

    /// Load one request: decode (or reuse cache), store, and paint the cell if it
    /// is still showing the item.
    async fn run_load(&self, req: LoadRequest) {
        // Skip if the cell recycled to another item before its turn came up.
        if let Some(w) = &req.cell {
            match w.upgrade() {
                Some(c) if c.is_showing(&req.item) => {}
                _ => return,
            }
        }
        let cache = self.imp().thumb_cache.clone();
        let orientation = req.item.orientation();
        let key = crate::thumbnails::ram_key(&req.item.file().uri(), req.load_size)
            + &crate::thumbnails::orient_key(orientation);
        let cached = cache.borrow_mut().get(&key).cloned();
        let cached_hit = cached.is_some();
        let result = if cached_hit {
            cached
        } else {
            let renderer = crate::thumbnails::renderer_source(self);
            let loaded = crate::thumbnails::load(
                req.item.file(),
                req.item.mtime(),
                req.load_size,
                req.item.size(),
                renderer,
            )
            .await;
            let loaded = match loaded {
                Some(tex) => crate::thumbnails::orient_cpu(tex, orientation).await,
                None => None,
            };
            match &loaded {
                Some(tex) => {
                    cache
                        .borrow_mut()
                        .put(key, tex.clone(), crate::thumbnails::texture_cost(tex))
                }
                None => req.item.mark_failed(),
            }
            loaded
        };
        if let Some(w) = &req.cell {
            if let Some(c) = w.upgrade() {
                c.apply(&req.item, result.as_ref());
            }
        }
        // Fill-order metric (§13.3): each completion's position vs the viewport
        // centre at that moment, plus whether it was a visible cell or prefetch
        // and whether it came from cache. Lets a test assert viewport-first fill.
        if crate::debug::enabled() && result.is_some() {
            eprintln!(
                "VDBG-GRIDFILL ms={} pos={} center={} visible={} hit={}",
                crate::debug::since_start_ms(),
                req.position,
                self.imp().visible_center.get(),
                req.cell.is_some(),
                cached_hit
            );
        }
    }

    /// The sorted model backing the grid. Positions from grid callbacks (bind,
    /// activate, selection) index *this*, not the raw store.
    fn model(&self) -> Option<gtk::SortListModel> {
        self.imp().sort_model.borrow().clone()
    }

    /// VITRINE_DEBUG: a MangoHUD-style readout of the thumbnail pipeline. Samples
    /// render frame time, worst main-loop stall, decode throughput, cache hit
    /// rate, pending-queue depth, and RSS, and logs a `VDBG` line to stderr each
    /// second (forward with `2>> file`). Pure observation — no behaviour change.
    fn setup_debug_hud(&self) {
        if !crate::debug::enabled() {
            return;
        }
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        // Rolling render frame-time (from the widget's frame clock).
        let last_frame = Rc::new(Cell::new(0i64)); // frame_clock time, microseconds
        let frame_max = Rc::new(Cell::new(0i64));
        let frame_count = Rc::new(Cell::new(0u32));
        self.add_tick_callback(glib::clone!(
            #[strong]
            last_frame,
            #[strong]
            frame_max,
            #[strong]
            frame_count,
            move |_, clock| {
                let now = clock.frame_time();
                let prev = last_frame.replace(now);
                if prev != 0 {
                    let dt = now - prev;
                    if dt > frame_max.get() {
                        frame_max.set(dt);
                    }
                    frame_count.set(frame_count.get() + 1);
                }
                glib::ControlFlow::Continue
            }
        ));

        // Worst main-loop stall: a 16ms heartbeat measuring its own lateness.
        let stall_max = Rc::new(Cell::new(0u128));
        let last_beat = Rc::new(Cell::new(Instant::now()));
        glib::timeout_add_local(
            Duration::from_millis(16),
            glib::clone!(
                #[strong]
                stall_max,
                #[strong]
                last_beat,
                move || {
                    let interval = last_beat.replace(Instant::now()).elapsed().as_millis();
                    let late = interval.saturating_sub(16);
                    if late > stall_max.get() {
                        stall_max.set(late);
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        // Per-second stats line to stderr (+ reset the rolling maxes).
        let last_done = Rc::new(Cell::new(0u64));
        let last_log = Rc::new(Cell::new(Instant::now()));
        glib::timeout_add_seconds_local(
            1,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                frame_max,
                #[strong]
                frame_count,
                #[strong]
                stall_max,
                #[strong]
                last_done,
                #[strong]
                last_log,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let c = crate::debug::snapshot();
                    let secs = last_log
                        .replace(Instant::now())
                        .elapsed()
                        .as_secs_f64()
                        .max(0.001);
                    let fps = (frame_count.replace(0) as f64 / secs).round() as u32;
                    let fmax_ms = (frame_max.replace(0) as f64 / 1000.0).round() as i64;
                    let stall = stall_max.replace(0);
                    let done_delta = c.done.saturating_sub(last_done.replace(c.done));
                    let rate = (done_delta as f64 / secs).round() as u64;
                    let cache_total = c.hits + c.misses;
                    let hit = if cache_total > 0 {
                        c.hits * 100 / cache_total
                    } else {
                        0
                    };
                    let queued = window.imp().pending.borrow().len();
                    let (cache_mb, cache_n) = {
                        let cache = window.imp().thumb_cache.borrow();
                        (cache.used_bytes() / (1024 * 1024), cache.len())
                    };
                    eprintln!(
                        "VDBG fps={fps} frame_max={fmax_ms}ms stall={stall}ms \
                         decode[live={} done={} +{rate}/s] queued={queued} \
                         cache_hit={hit}% ram_cache={cache_mb}MB/{cache_n} rss={}MB",
                        c.inflight,
                        c.done,
                        crate::debug::rss_mb()
                    );
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Prefetch the items just outside `[lo, hi]` into the RAM cache.
    fn prefetch_range(&self, lo: u32, hi: u32) {
        let n = self.model().map_or(0, |m| m.n_items());
        let start = lo.saturating_sub(PREFETCH_BEHIND);
        let end = hi.saturating_add(PREFETCH_AHEAD).min(n.saturating_sub(1));
        let load_size = self.icon_px().max(256);
        for pos in start..=end {
            if pos >= lo && pos <= hi {
                continue; // already handled as a visible cell
            }
            self.prefetch_one(pos, load_size);
        }
    }

    /// Load one item's thumbnail into the RAM cache (no cell), gated.
    fn prefetch_one(&self, position: u32, load_size: u32) {
        let imp = self.imp();
        let Some(item) = self
            .model()
            .and_then(|m| m.item(position))
            .and_downcast::<ImageObject>()
        else {
            return;
        };
        if item.has_failed() {
            return;
        }
        let key = crate::thumbnails::ram_key(&item.file().uri(), load_size);
        if imp.thumb_cache.borrow().contains(&key) {
            return;
        }
        self.enqueue_load(LoadRequest {
            cell: None,
            item,
            position,
            load_size,
        });
    }

    /// The current thumbnail display size in pixels.
    fn icon_px(&self) -> u32 {
        ICON_SIZES[self.imp().icon_index.get()]
    }

    /// A factory whose cells are sized to the current icon size and load the
    /// resolution appropriate for it.
    fn build_factory(&self) -> gtk::SignalListItemFactory {
        let icon_px = self.icon_px();
        let cache = self.imp().thumb_cache.clone();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let cell = VitrineGridCell::default();
            cell.set_icon_size(icon_px);
            cell.add_drag_source();
            list_item.set_child(Some(&cell));
        });
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let cell = list_item.child().and_downcast::<VitrineGridCell>().unwrap();
                let item = list_item.item().and_downcast::<ImageObject>().unwrap();
                // Display synchronously; queue the load (spawned when scrolling
                // settles) only if the thumbnail isn't already cached.
                if cell.bind(&item, &cache) {
                    let position = list_item.position();
                    let mut pending = window.imp().pending.borrow_mut();
                    pending.push((cell.downgrade(), item, position));
                    // During a long fling keep only the most recent binds — older
                    // ones have scrolled off and would be skipped at flush anyway
                    // (this stops the debounce queue ballooning to thousands).
                    if pending.len() > 400 {
                        let drop = pending.len() - 400;
                        pending.drain(0..drop);
                    }
                    drop(pending);
                    window.schedule_flush();
                }
            }
        ));
        factory.connect_unbind(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(cell) = list_item.child().and_downcast::<VitrineGridCell>() {
                cell.unbind();
            }
        });
        factory
    }

    /// Step the icon size by `delta` levels (clamped) and rebuild the factory.
    fn change_icon(&self, delta: i32) {
        let imp = self.imp();
        let new = (imp.icon_index.get() as i32 + delta).clamp(0, ICON_SIZES.len() as i32 - 1);
        if new as usize == imp.icon_index.get() {
            return;
        }
        imp.icon_index.set(new as usize);
        self.apply_icon_size();
    }

    fn reset_icon(&self) {
        let imp = self.imp();
        if imp.icon_index.get() != DEFAULT_ICON {
            imp.icon_index.set(DEFAULT_ICON);
            self.apply_icon_size();
        }
    }

    /// Rebuild the grid factory at the current icon size (recreates visible
    /// cells) and update the +/- buttons' sensitivity.
    fn apply_icon_size(&self) {
        let imp = self.imp();
        if let Some(grid_view) = imp.grid_view.borrow().as_ref() {
            grid_view.set_factory(Some(&self.build_factory()));
        }
        let idx = imp.icon_index.get();
        imp.icon_smaller.set_sensitive(idx > 0);
        imp.icon_larger.set_sensitive(idx < ICON_SIZES.len() - 1);
    }

    /// Positions currently selected in the grid, ascending (into the sorted model).
    fn selected_positions(&self) -> Vec<u32> {
        let Some(selection) = self.imp().selection.borrow().clone() else {
            return Vec::new();
        };
        let n = self.model().map_or(0, |m| m.n_items());
        (0..n).filter(|&pos| selection.is_selected(pos)).collect()
    }

    /// Space: quick-preview the first selected image in the viewer.
    fn preview_selected(&self) {
        if let Some(&pos) = self.selected_positions().first() {
            self.open_viewer(pos);
        }
    }

    /// Number keys 0–5: rate every selected image (0 clears). Updates the items'
    /// `rating` property (so the star overlays repaint at once) and persists via
    /// the annotator, keyed on the content hash stamped from the index.
    fn rate_selection(&self, rating: i32) {
        let Some(model) = self.model() else { return };
        let annotator = self
            .imp()
            .indexer
            .borrow()
            .as_ref()
            .map(|indexer| indexer.annotator());
        let mut rated = 0;
        for pos in self.selected_positions() {
            let Some(item) = model.item(pos).and_downcast::<ImageObject>() else {
                continue;
            };
            item.set_rating(rating);
            let hash = item.content_hash();
            if !hash.is_empty() {
                if let Some(annotator) = &annotator {
                    annotator.set_rating(
                        &hash,
                        if rating == 0 {
                            None
                        } else {
                            Some(rating as i64)
                        },
                    );
                }
                rated += 1;
            }
        }
        if rated == 0 {
            self.toast("Rating needs the image indexed — try again in a moment");
        }
        self.refilter(); // ratings changed → re-evaluate a rating filter
    }

    // --- tagging -------------------------------------------------------------

    /// Build the "Tag Selection" popover: a new-tag entry plus a chip cloud of
    /// existing tags (click to apply), live-filtered by what you type.
    fn setup_tagging(&self) {
        let imp = self.imp();

        let popover = gtk::Popover::new();
        popover.set_width_request(260);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let entry = gtk::Entry::builder()
            .placeholder_text(gettextrs::gettext("Tag selection… (Enter)"))
            .build();
        content.append(&entry);

        let flowbox = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(4)
            .row_spacing(4)
            .max_children_per_line(4)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(220)
            .propagate_natural_height(true)
            .child(&flowbox)
            .build();
        content.append(&scroller);
        popover.set_child(Some(&content));
        imp.tag_button.set_popover(Some(&popover));

        // Enter applies the typed tag.
        entry.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |entry| {
                window.apply_tag_to_selection(&entry.text());
                entry.set_text("");
            }
        ));
        // Typing filters the existing-tag chips (case-insensitive substring).
        let filter = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        flowbox.set_filter_func(glib::clone!(
            #[strong]
            filter,
            move |child| {
                let needle = filter.borrow();
                if needle.is_empty() {
                    return true;
                }
                child
                    .child()
                    .and_downcast::<gtk::Button>()
                    .and_then(|b| b.label())
                    .map(|l| l.to_lowercase().contains(needle.as_str()))
                    .unwrap_or(false)
            }
        ));
        entry.connect_changed(glib::clone!(
            #[weak]
            flowbox,
            move |entry| {
                *filter.borrow_mut() = entry.text().to_lowercase();
                flowbox.invalidate_filter();
            }
        ));
        // Rebuild the chips (with fresh counts) each time the popover opens.
        popover.connect_show(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            entry,
            move |_| {
                entry.set_text("");
                window.rebuild_tag_chips();
                entry.grab_focus();
            }
        ));

        *imp.tag_entry.borrow_mut() = Some(entry);
        *imp.tag_flowbox.borrow_mut() = Some(flowbox);
    }

    /// Repopulate the tag chip cloud from the index (existing tags + counts).
    fn rebuild_tag_chips(&self) {
        let Some(flowbox) = self.imp().tag_flowbox.borrow().clone() else {
            return;
        };
        while let Some(child) = flowbox.first_child() {
            flowbox.remove(&child);
        }
        self.ensure_read_db();
        let db = self.imp().read_db.borrow();
        let Some(db) = db.as_ref() else { return };
        for tag in db.all_tags().unwrap_or_default() {
            let chip = gtk::Button::builder()
                .label(&tag.name)
                .tooltip_text(format!("{} image(s)", tag.count))
                .css_classes(["pill"])
                .build();
            let name = tag.name.clone();
            chip.connect_clicked(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.apply_tag_to_selection(&name)
            ));
            flowbox.insert(&chip, -1);
        }
    }

    /// Apply `name` to the current grid selection (batch write, one transaction).
    fn apply_tag_to_selection(&self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let hashes = self.selected_hashes();
        if hashes.is_empty() {
            self.toast("Select one or more indexed images to tag");
            return;
        }
        if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
            indexer.annotator().tag(name, &hashes, true);
        }
        self.toast(&match hashes.len() {
            1 => format!("Tagged 1 image “{name}”"),
            n => format!("Tagged {n} images “{name}”"),
        });
        self.imp().tag_button.popdown();
    }

    // --- filter bar ----------------------------------------------------------

    /// Wire the filter bar: min-rating + tag dropdowns and a clear button. The
    /// bar's visibility is bound to the header toggle in Blueprint.
    fn setup_filtering(&self) {
        let imp = self.imp();

        imp.rating_filter.connect_selected_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |dropdown| {
                window.imp().filter_state.borrow_mut().min_rating = dropdown.selected() as i32;
                window.refilter();
            }
        ));
        imp.tag_filter.connect_selected_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |dropdown| window.apply_tag_filter(dropdown.selected())
        ));
        imp.filter_clear.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                let imp = window.imp();
                imp.rating_filter.set_selected(0);
                imp.tag_filter.set_selected(0);
            }
        ));
        imp.filter_save.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.save_filter_as_collection()
        ));
        // Refresh the tag list from the index whenever the bar is opened.
        imp.filter_revealer
            .connect_reveal_child_notify(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |revealer| {
                    if revealer.reveals_child() {
                        window.refresh_tag_filter();
                    }
                }
            ));
    }

    /// Rebuild the tag dropdown from the index (an "All tags" row + each tag).
    fn refresh_tag_filter(&self) {
        let imp = self.imp();
        self.ensure_read_db();
        let tags: Vec<String> = {
            let db = imp.read_db.borrow();
            match db.as_ref() {
                Some(db) => db
                    .all_tags()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.name)
                    .collect(),
                None => Vec::new(),
            }
        };
        let mut labels = vec![gettextrs::gettext("All tags")];
        labels.extend(tags.iter().cloned());
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        imp.tag_filter.set_model(Some(&gtk::StringList::new(&refs)));
        *imp.tag_names.borrow_mut() = tags;
        // set_model resets selection to 0 → clears any prior tag filter.
    }

    /// Apply the tag dropdown selection (0 = All tags → no tag filter).
    fn apply_tag_filter(&self, selected: u32) {
        let imp = self.imp();
        let name = if selected == 0 {
            None
        } else {
            imp.tag_names.borrow().get((selected - 1) as usize).cloned()
        };
        let hashes = name.as_ref().map(|name| {
            self.ensure_read_db();
            let db = imp.read_db.borrow();
            db.as_ref()
                .and_then(|db| db.hashes_with_tag(name).ok())
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<String>>()
        });
        {
            let mut state = imp.filter_state.borrow_mut();
            state.tag_hashes = hashes;
            state.tag_name = name;
        }
        self.refilter();
    }

    /// Re-run the grid filter (after criteria or ratings change).
    fn refilter(&self) {
        if let Some(filter) = self.imp().filter.borrow().as_ref() {
            filter.changed(gtk::FilterChange::Different);
        }
    }

    /// The current filter bar state as a smart-collection predicate (library-wide;
    /// the filter's per-view tag hash-set becomes a `tags_any` name).
    fn current_filter_query(&self) -> Option<vitrine_engine::Query> {
        let state = self.imp().filter_state.borrow();
        if state.min_rating <= 0 && state.tag_name.is_none() {
            return None; // nothing to save
        }
        let mut query = vitrine_engine::Query::default();
        if state.min_rating > 0 {
            query.rating_min = Some(state.min_rating as i64);
        }
        if let Some(name) = &state.tag_name {
            query.tags_any = vec![name.clone()];
        }
        Some(query)
    }

    /// Save the current filter as a named smart collection.
    fn save_filter_as_collection(&self) {
        let Some(query) = self.current_filter_query() else {
            self.toast("Set a rating or tag filter first");
            return;
        };
        let entry = gtk::Entry::builder()
            .placeholder_text(gettextrs::gettext("Collection name"))
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(Some(&gettextrs::gettext("Save as Collection")), None);
        dialog.set_body(&gettextrs::gettext(
            "A smart collection updates automatically as images match this filter.",
        ));
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", &gettextrs::gettext("Cancel"));
        dialog.add_response("save", &gettextrs::gettext("Save"));
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                entry,
                move |_, response| {
                    if response != "save" {
                        return;
                    }
                    let name = entry.text().trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    if let Some(indexer) = window.imp().indexer.borrow().as_ref() {
                        indexer
                            .annotator()
                            .create_smart_collection(&name, query.clone());
                    }
                    window.toast(&gettextrs::gettext("Smart collection saved"));
                }
            ),
        );
        dialog.present(Some(self));
    }

    // --- navigation: bookmarks, folder tree, back button ---------------------

    fn setup_navigation(&self) {
        let imp = self.imp();

        imp.back_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.go_back()
        ));

        imp.forward_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.go_forward()
        ));

        // Bookmarks: click to open.
        imp.bookmarks_list.connect_row_activated(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, row| {
                let idx = row.index();
                if let Some(bookmark) = window.imp().bookmarks.borrow().get(idx as usize).cloned() {
                    window.open_location(gio::File::for_path(&bookmark.path));
                }
            }
        ));

        // Drop folders onto the bookmarks list to add them (Nautilus gesture).
        // Drop folders anywhere on the Places pane (a ListBox only spans its own
        // rows, so target the whole scroller as the drop zone).
        let drop = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE,
        );
        drop.connect_drop(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let Ok(list) = value.get::<gtk::gdk::FileList>() else {
                    return false;
                };
                let settings = crate::settings::Settings::load();
                let mut added = false;
                for file in list.files() {
                    if let Some(path) = file.path() {
                        if path.is_dir() && settings.add_bookmark(&path) {
                            added = true;
                        }
                    }
                }
                if added {
                    window.refresh_bookmarks();
                    window.toast("Folder bookmarked");
                }
                added
            }
        ));
        imp.places_scroller.add_controller(drop);

        // Reorder bookmarks: a list-level drop target lands the dragged bookmark
        // wherever you drop it (the row under the cursor, or the end).
        let reorder = gtk::DropTarget::new(i32::static_type(), gtk::gdk::DragAction::MOVE);
        // Show a drop-line at the insert position as you drag over the list.
        reorder.connect_motion(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            gtk::gdk::DragAction::empty(),
            move |_, _, y| {
                window.highlight_bookmark_drop(y);
                gtk::gdk::DragAction::MOVE
            }
        ));
        reorder.connect_leave(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.clear_bookmark_drop()
        ));
        reorder.connect_drop(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, y| {
                window.clear_bookmark_drop();
                let Ok(from) = value.get::<i32>() else {
                    return false;
                };
                // The row under the cursor (or the end if dropped past the last).
                let to = match window.imp().bookmarks_list.row_at_y(y as i32) {
                    Some(row) => row.index() as usize,
                    None => window.imp().bookmarks.borrow().len(),
                };
                window.reorder_bookmark(from as usize, to);
                true
            }
        ));
        imp.bookmarks_list.add_controller(reorder);

        // Ctrl+D bookmarks the current folder.
        let bookmark = gio::SimpleAction::new("bookmark-current", None);
        bookmark.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| window.bookmark_current()
        ));
        self.add_action(&bookmark);

        self.refresh_bookmarks();
        self.setup_folder_tree();
    }

    /// Move to `new`, pushing the current location onto the Back history (unless
    /// we're navigating via Back/Forward). A fresh navigation invalidates the
    /// Forward stack, as in a web browser. No-op if it's where we already are.
    fn set_location(&self, new: Location) {
        let imp = self.imp();
        let previous = imp.current_location.borrow().clone();
        if previous.as_ref() == Some(&new) {
            return;
        }
        if !imp.navigating_back.get() {
            if let Some(previous) = previous {
                imp.history.borrow_mut().push(previous);
            }
            imp.forward.borrow_mut().clear();
        }
        *imp.current_location.borrow_mut() = Some(new);
        self.update_nav_sensitivity();
    }

    /// Go to the previously-visited location, banking the current one on Forward.
    fn go_back(&self) {
        let Some(previous) = self.imp().history.borrow_mut().pop() else {
            return;
        };
        if let Some(current) = self.imp().current_location.borrow().clone() {
            self.imp().forward.borrow_mut().push(current);
        }
        self.navigate_to(previous);
    }

    /// Go to the location we last backed out of, banking the current one on Back.
    fn go_forward(&self) {
        let Some(next) = self.imp().forward.borrow_mut().pop() else {
            return;
        };
        if let Some(current) = self.imp().current_location.borrow().clone() {
            self.imp().history.borrow_mut().push(current);
        }
        self.navigate_to(next);
    }

    /// Load `target` without recording history (used by Back/Forward, which
    /// manage the stacks themselves).
    fn navigate_to(&self, target: Location) {
        self.imp().navigating_back.set(true);
        match &target {
            Location::Folder(path) => self.load_folder(gio::File::for_path(path)),
            Location::Collection(id) => self.open_collection(*id),
        }
        *self.imp().current_location.borrow_mut() = Some(target);
        self.imp().navigating_back.set(false);
        self.update_nav_sensitivity();
    }

    fn update_nav_sensitivity(&self) {
        let imp = self.imp();
        imp.back_button
            .set_sensitive(!imp.history.borrow().is_empty());
        imp.forward_button
            .set_sensitive(!imp.forward.borrow().is_empty());
    }

    /// Bookmark the folder currently shown.
    fn bookmark_current(&self) {
        let Some(folder) = self.imp().current_folder.borrow().clone() else {
            self.toast("Open a folder to bookmark it");
            return;
        };
        if crate::settings::Settings::load().add_bookmark(&folder) {
            self.refresh_bookmarks();
            self.toast("Bookmarked");
        } else {
            self.toast("Already bookmarked");
        }
    }

    /// Rebuild the bookmarks list from settings.
    fn refresh_bookmarks(&self) {
        let imp = self.imp();
        while let Some(row) = imp.bookmarks_list.row_at_index(0) {
            imp.bookmarks_list.remove(&row);
        }

        let bookmarks = crate::settings::Settings::load().bookmarks();
        imp.bookmarks_heading.set_visible(!bookmarks.is_empty());
        for (index, bookmark) in bookmarks.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.append(&gtk::Image::from_icon_name("folder-symbolic"));
            row.append(
                &gtk::Label::builder()
                    .label(&bookmark.name)
                    .halign(gtk::Align::Start)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );

            // Right-click → context menu (rename / remove / move), Nautilus-style.
            let menu = gtk::GestureClick::new();
            menu.set_button(gtk::gdk::BUTTON_SECONDARY);
            menu.connect_pressed(glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                row,
                move |gesture, _, x, y| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    window.show_bookmark_menu(index, row.upcast_ref(), x, y);
                }
            ));
            row.add_controller(menu);

            // Drag to reorder: the source carries this row's index; the drop is
            // handled at the list level (see setup_navigation) so you can drop
            // anywhere to land the bookmark there.
            let source = gtk::DragSource::new();
            source.set_actions(gtk::gdk::DragAction::MOVE);
            source.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(
                    &(index as i32).to_value(),
                ))
            });
            // Drag the row's own image as the cursor icon.
            source.connect_drag_begin(glib::clone!(
                #[weak]
                row,
                move |source, _| {
                    let paintable = gtk::WidgetPaintable::new(Some(&row)).current_image();
                    source.set_icon(Some(&paintable), 0, 0);
                }
            ));
            row.add_controller(source);

            imp.bookmarks_list.append(&row);
        }
        *imp.bookmarks.borrow_mut() = bookmarks;
    }

    /// Show a drop-line at the row the cursor is over (where a reordered
    /// bookmark would land), or at the bottom of the last row past the end.
    fn highlight_bookmark_drop(&self, y: f64) {
        self.clear_bookmark_drop();
        let list = &self.imp().bookmarks_list;
        if let Some(row) = list.row_at_y(y as i32) {
            row.add_css_class("drop-before");
        } else {
            // Below the last row → indicate an append.
            let mut last = None;
            let mut i = 0;
            while let Some(row) = list.row_at_index(i) {
                last = Some(row);
                i += 1;
            }
            if let Some(row) = last {
                row.add_css_class("drop-after");
            }
        }
    }

    /// Clear the reorder drop-line from every bookmark row.
    fn clear_bookmark_drop(&self) {
        let list = &self.imp().bookmarks_list;
        let mut i = 0;
        while let Some(row) = list.row_at_index(i) {
            row.remove_css_class("drop-before");
            row.remove_css_class("drop-after");
            i += 1;
        }
    }

    /// Right-click menu for a bookmark: open, rename, remove, move up/down.
    fn show_bookmark_menu(&self, index: usize, anchor: &gtk::Widget, x: f64, y: f64) {
        let popover = gtk::Popover::new();
        popover.set_parent(anchor);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| popover.unparent());

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let count = self.imp().bookmarks.borrow().len();
        let entries: &[(&str, i64)] = &[
            ("Open", 0),
            ("Rename…", 1),
            ("Remove", 2),
            ("Move Up", 3),
            ("Move Down", 4),
        ];
        for (label, action) in entries {
            // Move Up/Down only when there's somewhere to move.
            if (*action == 3 && index == 0) || (*action == 4 && index + 1 >= count) {
                continue;
            }
            let button = gtk::Button::builder()
                .label(gettextrs::gettext(*label))
                .css_classes(["flat"])
                .build();
            if let Some(child) = button.child().and_downcast::<gtk::Label>() {
                child.set_xalign(0.0);
            }
            let action = *action;
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                popover,
                move |_| {
                    window.bookmark_action(index, action);
                    popover.popdown();
                }
            ));
            content.append(&button);
        }
        popover.set_child(Some(&content));
        popover.popup();
    }

    fn bookmark_action(&self, index: usize, action: i64) {
        let bookmark = match self.imp().bookmarks.borrow().get(index).cloned() {
            Some(b) => b,
            None => return,
        };
        match action {
            0 => self.open_location(gio::File::for_path(&bookmark.path)),
            1 => self.rename_bookmark_dialog(index),
            2 => {
                crate::settings::Settings::load().remove_bookmark(&bookmark.path);
                self.refresh_bookmarks();
            }
            3 => self.move_bookmark(index, -1),
            4 => self.move_bookmark(index, 1),
            _ => {}
        }
    }

    /// Move a bookmark up (`-1`) or down (`+1`) one position.
    fn move_bookmark(&self, index: usize, delta: i32) {
        let mut list = self.imp().bookmarks.borrow().clone();
        let target = index as i32 + delta;
        if target < 0 || target as usize >= list.len() {
            return;
        }
        list.swap(index, target as usize);
        crate::settings::Settings::load().set_bookmarks(&list);
        self.refresh_bookmarks();
    }

    /// Move the bookmark at `from` to sit at position `to` (drag reorder).
    fn reorder_bookmark(&self, from: usize, to: usize) {
        let mut list = self.imp().bookmarks.borrow().clone();
        if from >= list.len() || from == to {
            return;
        }
        let bookmark = list.remove(from);
        let dest = if from < to { to.saturating_sub(1) } else { to };
        list.insert(dest.min(list.len()), bookmark);
        crate::settings::Settings::load().set_bookmarks(&list);
        self.refresh_bookmarks();
    }

    /// Rename a bookmark's display name (its target folder is unchanged).
    fn rename_bookmark_dialog(&self, index: usize) {
        let Some(bookmark) = self.imp().bookmarks.borrow().get(index).cloned() else {
            return;
        };
        let entry = gtk::Entry::builder()
            .text(&bookmark.name)
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(Some(&gettextrs::gettext("Rename Bookmark")), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", &gettextrs::gettext("Cancel"));
        dialog.add_response("rename", &gettextrs::gettext("Rename"));
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                entry,
                move |_, response| {
                    if response != "rename" {
                        return;
                    }
                    let name = entry.text().trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let mut list = window.imp().bookmarks.borrow().clone();
                    if let Some(bookmark) = list.get_mut(index) {
                        bookmark.name = name;
                        crate::settings::Settings::load().set_bookmarks(&list);
                        window.refresh_bookmarks();
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }

    /// The folders the sandbox can browse (Pictures + library roots), deduped.
    fn accessible_roots(&self) -> Vec<gio::File> {
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |path: PathBuf| {
            if seen.insert(path.clone()) {
                roots.push(gio::File::for_path(path));
            }
        };
        if let Some(pictures) = glib::user_special_dir(glib::UserDirectory::Pictures) {
            push(pictures);
        }
        for root in crate::settings::Settings::load().roots() {
            push(root);
        }
        roots
    }

    /// Build the lazy directory tree (rooted at the accessible locations — the
    /// sandbox can't see a full host tree, §4).
    fn setup_folder_tree(&self) {
        let root_store = gio::ListStore::new::<gio::File>();
        for root in self.accessible_roots() {
            root_store.append(&root);
        }
        let tree = gtk::TreeListModel::new(root_store, false, false, |item| {
            item.downcast_ref::<gio::File>().and_then(dir_children)
        });
        let selection = gtk::SingleSelection::new(Some(tree));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&gtk::Image::from_icon_name("folder-symbolic"));
            content.append(
                &gtk::Label::builder()
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&content));
            list_item.set_child(Some(&expander));
        });
        factory.connect_bind(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(expander) = list_item.child().and_downcast::<gtk::TreeExpander>() else {
                return;
            };
            let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            expander.set_list_row(Some(&row));
            if let Some(file) = row.item().and_downcast::<gio::File>() {
                if let Some(label) = expander
                    .child()
                    .and_downcast::<gtk::Box>()
                    .and_then(|b| b.last_child())
                    .and_downcast::<gtk::Label>()
                {
                    let name = file
                        .basename()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    label.set_text(&name);
                }
            }
        });

        let listview = &self.imp().folder_tree;
        listview.set_model(Some(&selection));
        listview.set_factory(Some(&factory));
        listview.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[strong]
            selection,
            move |_, pos| {
                if let Some(file) = selection
                    .item(pos)
                    .and_downcast::<gtk::TreeListRow>()
                    .and_then(|r| r.item())
                    .and_downcast::<gio::File>()
                {
                    window.open_location(file);
                }
            }
        ));
    }

    // --- duplicates ----------------------------------------------------------

    /// Open (or refresh) the Duplicates page and push it onto the nav view.
    fn show_duplicates(&self) {
        let imp = self.imp();
        if imp.duplicates_page.borrow().is_none() {
            self.build_duplicates_page();
        }
        self.refresh_duplicates();
        if imp.nav_view.find_page("duplicates").is_none() {
            if let Some(page) = imp.duplicates_page.borrow().as_ref() {
                imp.nav_view.push(page);
            }
        } else {
            imp.nav_view.pop_to_tag("duplicates");
        }
    }

    /// Build the Duplicates page shell: a header with an Exact/Similar switch and
    /// a scrollable content box (filled by `refresh_duplicates`).
    fn build_duplicates_page(&self) {
        let imp = self.imp();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        let scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&content)
            .build();

        let mode = gtk::DropDown::from_strings(&[
            &gettextrs::gettext("Exact"),
            &gettextrs::gettext("Similar"),
        ]);
        mode.set_tooltip_text(Some(&gettextrs::gettext(
            "Exact: byte-identical · Similar: visually alike (perceptual hash)",
        )));
        mode.connect_selected_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |dropdown| {
                window.imp().dedup_near.set(dropdown.selected() == 1);
                window.refresh_duplicates();
            }
        ));

        let header = adw::HeaderBar::new();
        header.pack_end(&mode);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroller));

        let page = adw::NavigationPage::builder()
            .title(gettextrs::gettext("Duplicates"))
            .tag("duplicates")
            .child(&toolbar)
            .build();
        *imp.duplicates_page.borrow_mut() = Some(page);
        *imp.duplicates_content.borrow_mut() = Some(content);
    }

    /// Run the dedup scan and rebuild the cluster cards.
    /// Cap on how many cluster cards we render — a loose "Similar" scan over a
    /// large library can produce thousands of clusters, and building a card (with
    /// thumbnails) for each will exhaust memory/FDs. Show the worst offenders.
    const DEDUP_MAX_CLUSTERS: usize = 200;

    fn refresh_duplicates(&self) {
        let imp = self.imp();
        let Some(content) = imp.duplicates_content.borrow().clone() else {
            return;
        };
        while let Some(child) = content.first_child() {
            content.remove(&child);
        }

        let near = imp.dedup_near.get();

        // The near-duplicate scan is O(n²) over every indexed file — seconds of
        // work on a big library — so run it off the main thread behind a spinner
        // instead of freezing (and eventually crashing) the UI.
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.start();
        let status = adw::StatusPage::builder()
            .title(if near {
                gettextrs::gettext("Scanning for similar images…")
            } else {
                gettextrs::gettext("Scanning for duplicates…")
            })
            .child(&spinner)
            .vexpand(true)
            .build();
        content.append(&status);

        let generation = imp.dedup_generation.get().wrapping_add(1);
        imp.dedup_generation.set(generation);
        let db_path = crate::index::index_db_path();

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                let clusters = gio::spawn_blocking(move || {
                    let Ok(db) = vitrine_engine::Db::open(db_path) else {
                        return Vec::new();
                    };
                    if near {
                        db.near_duplicates(8)
                    } else {
                        db.exact_duplicates()
                    }
                    .unwrap_or_default()
                })
                .await
                .unwrap_or_default();

                // Discard if the user changed mode or left while we scanned.
                if window.imp().dedup_generation.get() != generation {
                    return;
                }
                window.render_duplicates(&clusters, near);
            }
        ));
    }

    /// Render the scan results into the duplicates content box (capped).
    fn render_duplicates(&self, clusters: &[vitrine_engine::DuplicateCluster], near: bool) {
        let Some(content) = self.imp().duplicates_content.borrow().clone() else {
            return;
        };
        while let Some(child) = content.first_child() {
            content.remove(&child);
        }

        if clusters.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("edit-copy-symbolic")
                    .title(gettextrs::gettext("No Duplicates Found"))
                    .description(if near {
                        gettextrs::gettext("No visually-similar images in the index yet.")
                    } else {
                        gettextrs::gettext("No byte-identical images in the index yet.")
                    })
                    .vexpand(true)
                    .build(),
            );
            return;
        }

        if clusters.len() > Self::DEDUP_MAX_CLUSTERS {
            content.append(
                &gtk::Label::builder()
                    .label(format!(
                        "Showing the {} largest of {} duplicate groups.",
                        Self::DEDUP_MAX_CLUSTERS,
                        clusters.len()
                    ))
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        for cluster in clusters.iter().take(Self::DEDUP_MAX_CLUSTERS) {
            content.append(&self.build_cluster_card(cluster));
        }
    }

    /// One card per duplicate cluster: reclaimable size, the images (keeper
    /// first), and a button to trash the extra copies.
    fn build_cluster_card(&self, cluster: &vitrine_engine::DuplicateCluster) -> gtk::Widget {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card.add_css_class("card");
        card.set_margin_bottom(4);

        let reclaimable: i64 = cluster.files.iter().skip(1).map(|f| f.size).sum();
        let heading = gtk::Label::builder()
            .label(format!(
                "{} copies · {} reclaimable",
                cluster.files.len(),
                glib::format_size(reclaimable.max(0) as u64)
            ))
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .margin_start(10)
            .margin_top(10)
            .build();
        card.append(&heading);

        // Cap thumbnails per card: a near-dup group can chain into hundreds of
        // members, and each thumbnail spawns a decode. The trash button still
        // acts on the whole group; the strip just previews the first few.
        const DEDUP_MAX_THUMBS: usize = 12;
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        strip.set_margin_start(10);
        strip.set_margin_end(10);
        for (i, file) in cluster.files.iter().take(DEDUP_MAX_THUMBS).enumerate() {
            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .width_request(96)
                .height_request(96)
                .tooltip_text(&file.path)
                .css_classes(if i == 0 {
                    vec!["card", "dedup-keeper"]
                } else {
                    vec!["card"]
                })
                .build();
            self.load_dedup_thumb(&picture, &file.path, file.mtime);
            strip.append(&picture);
        }
        if cluster.files.len() > DEDUP_MAX_THUMBS {
            strip.append(
                &gtk::Label::builder()
                    .label(format!("+{}", cluster.files.len() - DEDUP_MAX_THUMBS))
                    .css_classes(["dim-label", "title-2"])
                    .valign(gtk::Align::Center)
                    .build(),
            );
        }
        card.append(&strip);

        let others: Vec<String> = cluster
            .files
            .iter()
            .skip(1)
            .map(|f| f.path.clone())
            .collect();
        let trash = gtk::Button::builder()
            .label(match others.len() {
                1 => gettextrs::gettext("Trash the other copy (keep largest)"),
                n => format!("Trash the other {n} copies (keep largest)"),
            })
            .halign(gtk::Align::Start)
            .css_classes(["destructive-action"])
            .margin_start(10)
            .margin_bottom(10)
            .build();
        trash.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.trash_duplicate_others(&others)
        ));
        card.append(&trash);
        card.upcast()
    }

    /// Load a thumbnail for a path into a `Picture` (dedup cards, size 128).
    fn load_dedup_thumb(&self, picture: &gtk::Picture, path: &str, mtime: i64) {
        let file = gio::File::for_path(path);
        let key = crate::thumbnails::ram_key(&file.uri(), 128);
        let cache = self.imp().thumb_cache.clone();
        if let Some(texture) = cache.borrow_mut().get(&key).cloned() {
            picture.set_paintable(Some(&texture));
            return;
        }
        let renderer = crate::thumbnails::renderer_source(picture);
        let weak = picture.downgrade();
        glib::spawn_future_local(async move {
            let _permit = crate::thumbnails::load_gate().acquire().await;
            if let Some(texture) = crate::thumbnails::load(file, mtime, 128, 0, renderer).await {
                cache.borrow_mut().put(
                    key,
                    texture.clone(),
                    crate::thumbnails::texture_cost(&texture),
                );
                if let Some(picture) = weak.upgrade() {
                    picture.set_paintable(Some(&texture));
                }
            }
        });
    }

    /// Trash the non-keeper copies of a cluster, drop them from the index, and
    /// refresh the page.
    fn trash_duplicate_others(&self, paths: &[String]) {
        for path in paths {
            gio::File::for_path(path).trash_async(
                glib::Priority::DEFAULT,
                gio::Cancellable::NONE,
                glib::clone!(
                    #[weak(rename_to = window)]
                    self,
                    move |result| {
                        if let Err(err) = result {
                            window.toast(&format!("Couldn’t move to trash: {}", err.message()));
                        }
                    }
                ),
            );
        }
        if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
            indexer.annotator().mark_missing(paths);
        }
        self.toast(&match paths.len() {
            1 => "Moved 1 copy to Trash".to_string(),
            n => format!("Moved {n} copies to Trash"),
        });
        // Give the writer a beat to mark them missing, then rebuild the list.
        glib::timeout_add_seconds_local_once(
            1,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || window.refresh_duplicates()
            ),
        );
    }

    // --- collections ---------------------------------------------------------

    /// Wire the sidebar collections list + "new catalog" button, and do an
    /// initial populate from the index.
    fn setup_collections(&self) {
        let imp = self.imp();

        imp.new_collection_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.new_catalog_dialog()
        ));

        imp.collections_list.connect_row_activated(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, row| {
                let idx = row.index();
                if let Some(&id) = window.imp().collection_ids.borrow().get(idx as usize) {
                    window.open_collection(id);
                }
            }
        ));

        self.refresh_collections();
    }

    /// Rebuild the sidebar collections list from the index.
    fn refresh_collections(&self) {
        let imp = self.imp();
        let list = &imp.collections_list;
        while let Some(row) = list.row_at_index(0) {
            list.remove(&row);
        }
        imp.collection_ids.borrow_mut().clear();

        self.ensure_read_db();
        let collections = {
            let db = imp.read_db.borrow();
            match db.as_ref() {
                Some(db) => db.list_collections().unwrap_or_default(),
                None => return,
            }
        };

        for collection in collections {
            let icon = match collection.kind {
                vitrine_engine::CollectionKind::Smart => "folder-saved-search-symbolic",
                vitrine_engine::CollectionKind::Catalog => "view-list-symbolic",
            };
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.set_margin_top(8);
            row.set_margin_bottom(8);
            row.append(&gtk::Image::from_icon_name(icon));
            let name = gtk::Label::builder()
                .label(&collection.name)
                .halign(gtk::Align::Start)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            row.append(&name);
            row.append(
                &gtk::Label::builder()
                    .label(collection.count.to_string())
                    .css_classes(["dim-label", "caption"])
                    .build(),
            );

            // Right-click → delete (+ add-selection for catalogs).
            let is_catalog = collection.kind == vitrine_engine::CollectionKind::Catalog;
            let id = collection.id;
            let menu = gtk::GestureClick::new();
            menu.set_button(gtk::gdk::BUTTON_SECONDARY);
            menu.connect_pressed(glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                row,
                move |gesture, _, x, y| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    window.show_collection_menu(id, is_catalog, row.upcast_ref(), x, y);
                }
            ));
            row.add_controller(menu);

            // Drag an image from the grid onto a catalog to add it.
            if is_catalog {
                let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::COPY);
                drop.connect_drop(glib::clone!(
                    #[weak(rename_to = window)]
                    self,
                    #[upgrade_or]
                    false,
                    move |_, value, _, _| {
                        let Ok(hash) = value.get::<String>() else {
                            return false;
                        };
                        // Dragging one of a multi-selection adds the whole
                        // selection (file-manager behaviour); otherwise just it.
                        let selected = window.selected_hashes();
                        let hashes = if selected.contains(&hash) && selected.len() > 1 {
                            selected
                        } else if !hash.is_empty() {
                            vec![hash]
                        } else {
                            return true;
                        };
                        if let Some(indexer) = window.imp().indexer.borrow().as_ref() {
                            indexer.annotator().add_to_catalog(id, &hashes);
                        }
                        window.toast(&match hashes.len() {
                            1 => "Added 1 image to catalog".to_string(),
                            n => format!("Added {n} images to catalog"),
                        });
                        true
                    }
                ));
                row.add_controller(drop);
            }

            list.append(&row);
            imp.collection_ids.borrow_mut().push(collection.id);
        }
    }

    /// Load a collection's files into the grid (spanning folders; not a folder
    /// browse, so the folder-scoped rating stamp is skipped — items are stamped
    /// inline from the index instead).
    fn open_collection(&self, id: i64) {
        self.ensure_read_db();
        let items: Vec<ImageObject> = {
            let db = self.imp().read_db.borrow();
            let Some(db) = db.as_ref() else { return };
            db.collection_files(id)
                .unwrap_or_default()
                .into_iter()
                .map(|record| {
                    let file = gio::File::for_path(&record.path);
                    let name = std::path::Path::new(&record.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| record.path.clone());
                    let content_type = record.format.clone().unwrap_or_default();
                    let item =
                        ImageObject::new(file, &name, record.mtime, record.size, &content_type);
                    item.set_content_hash(&record.content_hash);
                    let rating = db.rating(&record.content_hash).ok().flatten().unwrap_or(0);
                    item.set_rating(rating as i32);
                    item
                })
                .collect()
        };

        // Record the collection as a location so Back returns to the folder you
        // were in (and to it, if you navigate on).
        self.set_location(Location::Collection(id));
        let imp = self.imp();
        *imp.current_folder.borrow_mut() = None; // multi-folder; no folder stamp
        imp.store.remove_all();
        imp.store.extend_from_slice(&items);
        imp.content_stack
            .set_visible_child_name(if items.is_empty() { "empty" } else { "grid" });
    }

    /// Show the right-click menu for a collection row: delete, and for catalogs
    /// "Add Selection".
    fn show_collection_menu(
        &self,
        id: i64,
        is_catalog: bool,
        anchor: &gtk::Widget,
        x: f64,
        y: f64,
    ) {
        let popover = gtk::Popover::new();
        popover.set_parent(anchor);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| popover.unparent());

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        if is_catalog {
            let add = gtk::Button::builder()
                .label(gettextrs::gettext("Add Selection"))
                .css_classes(["flat"])
                .build();
            add.connect_clicked(glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                popover,
                move |_| {
                    let hashes = window.selected_hashes();
                    if hashes.is_empty() {
                        window.toast("Select images to add");
                    } else if let Some(indexer) = window.imp().indexer.borrow().as_ref() {
                        indexer.annotator().add_to_catalog(id, &hashes);
                        window.toast(&format!("Added {} to catalog", hashes.len()));
                    }
                    popover.popdown();
                }
            ));
            content.append(&add);
        }
        let delete = gtk::Button::builder()
            .label(gettextrs::gettext("Delete"))
            .css_classes(["flat"])
            .build();
        delete.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            popover,
            move |_| {
                if let Some(indexer) = window.imp().indexer.borrow().as_ref() {
                    indexer.annotator().delete_collection(id);
                }
                popover.popdown();
            }
        ));
        content.append(&delete);
        popover.set_child(Some(&content));
        popover.popup();
    }

    /// Prompt for a name and create a catalog seeded with the current selection.
    fn new_catalog_dialog(&self) {
        let hashes = self.selected_hashes();
        let entry = gtk::Entry::builder()
            .placeholder_text(gettextrs::gettext("Catalog name"))
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(Some(&gettextrs::gettext("New Catalog")), None);
        dialog.set_body(&match hashes.len() {
            0 => gettextrs::gettext("Create an empty catalog."),
            1 => gettextrs::gettext("Create a catalog with the selected image."),
            n => format!("Create a catalog with the {n} selected images."),
        });
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", &gettextrs::gettext("Cancel"));
        dialog.add_response("create", &gettextrs::gettext("Create"));
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                entry,
                move |_, response| {
                    if response != "create" {
                        return;
                    }
                    let name = entry.text().trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    if let Some(indexer) = window.imp().indexer.borrow().as_ref() {
                        indexer.annotator().create_catalog(&name, &hashes);
                    }
                    window.toast(&gettextrs::gettext("Catalog created"));
                }
            ),
        );
        dialog.present(Some(self));
    }

    /// Content hashes of the selected items that are indexed (have a hash).
    fn selected_hashes(&self) -> Vec<String> {
        let Some(model) = self.model() else {
            return Vec::new();
        };
        self.selected_positions()
            .into_iter()
            .filter_map(|pos| model.item(pos).and_downcast::<ImageObject>())
            .map(|item| item.content_hash())
            .filter(|h| !h.is_empty())
            .collect()
    }

    /// Stamp each item with its content hash + rating from the index, so cell
    /// overlays and rating writes need no per-cell database hit. Best-effort:
    /// items the index hasn't caught up on stay unstamped until the re-stamp on
    /// scan completion.
    fn stamp_annotations(&self, items: &[ImageObject]) {
        let Some(folder) = self.imp().current_folder.borrow().clone() else {
            return;
        };
        self.ensure_read_db();
        let db = self.imp().read_db.borrow();
        let Some(db) = db.as_ref() else { return };
        let map: std::collections::HashMap<String, (String, i64, i64)> = db
            .ratings_under(&folder.to_string_lossy())
            .unwrap_or_default()
            .into_iter()
            .map(|(path, hash, rating, orientation)| (path, (hash, rating, orientation)))
            .collect();
        for item in items {
            if let Some(path) = item.file().path() {
                if let Some((hash, rating, orientation)) =
                    map.get(&path.to_string_lossy().into_owned())
                {
                    item.set_content_hash(hash);
                    item.set_rating(*rating as i32);
                    item.set_orientation(*orientation as i32);
                }
            }
        }
    }

    /// Re-stamp the items currently in the store (after indexing catches up).
    fn restamp_store(&self) {
        let items: Vec<ImageObject> = (0..self.imp().store.n_items())
            .filter_map(|i| self.imp().store.item(i).and_downcast::<ImageObject>())
            .collect();
        self.stamp_annotations(&items);
        self.refilter(); // freshly-stamped ratings may change a rating filter
    }

    fn ensure_read_db(&self) {
        let imp = self.imp();
        if imp.read_db.borrow().is_none() {
            match Db::open(crate::index::index_db_path()) {
                Ok(db) => *imp.read_db.borrow_mut() = Some(db),
                Err(e) => glib::g_warning!("vitrine", "window read db: {e}"),
            }
        }
    }

    /// Delete: move the selected images to the trash (reversible — never unlink),
    /// dropping each from the grid as it is trashed.
    fn trash_selected(&self) {
        let model = self.model();
        let items: Vec<ImageObject> = self
            .selected_positions()
            .into_iter()
            .filter_map(|pos| model.as_ref()?.item(pos).and_downcast::<ImageObject>())
            .collect();
        if items.is_empty() {
            return;
        }
        let total = items.len();
        for item in items {
            let file = item.file();
            file.trash_async(
                glib::Priority::DEFAULT,
                gio::Cancellable::NONE,
                glib::clone!(
                    #[weak(rename_to = window)]
                    self,
                    #[strong]
                    item,
                    move |result| match result {
                        Ok(()) => window.remove_item(&item),
                        Err(err) =>
                            window.toast(&format!("Couldn’t move to trash: {}", err.message())),
                    }
                ),
            );
        }
        self.toast(&match total {
            1 => "Moved 1 image to Trash".to_string(),
            n => format!("Moved {n} images to Trash"),
        });
    }

    fn remove_item(&self, item: &ImageObject) {
        let imp = self.imp();
        if let Some(pos) = imp.store.find(item) {
            imp.store.remove(pos);
        }
        if imp.store.n_items() == 0 {
            imp.content_stack.set_visible_child_name("empty");
        }
    }

    fn toast(&self, message: &str) {
        self.imp().toast_overlay.add_toast(adw::Toast::new(message));
    }

    /// Push the viewer page showing the image at `position`.
    fn open_viewer(&self, position: u32) {
        let imp = self.imp();
        let viewer = imp
            .viewer
            .borrow_mut()
            .get_or_insert_with(VitrineViewer::new)
            .clone();
        if let Some(indexer) = imp.indexer.borrow().as_ref() {
            viewer.set_annotator(indexer.annotator());
        }
        let Some(model) = self.model() else { return };
        viewer.open(model.upcast(), position, imp.thumb_cache.clone());
        if imp.nav_view.find_page("viewer").is_none() {
            imp.nav_view.push(&viewer);
        } else {
            imp.nav_view.pop_to_tag("viewer");
        }
    }

    fn setup_actions(&self) {
        let imp = self.imp();

        imp.places_list.connect_row_activated(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| window.open_folder_dialog()
        ));

        // Two independent stateful actions — field and direction — exactly like
        // Nautilus (pick what to sort by, then flip the order). Both restore
        // from settings so the choice is remembered across sessions.
        let saved = crate::settings::Settings::load();
        let state = SortState {
            field: SortField::from_id(&saved.sort_field()),
            descending: saved.sort_descending(),
        };
        self.imp().sort_state.set(state);

        let field = gio::SimpleAction::new_stateful(
            "sort-field",
            Some(glib::VariantTy::STRING),
            &state.field.id().to_variant(),
        );
        field.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |action, param| {
                if let Some(id) = param.and_then(|v| v.str()) {
                    action.set_state(&id.to_variant());
                    window.set_sort_field(SortField::from_id(id));
                }
            }
        ));
        self.add_action(&field);

        let direction = gio::SimpleAction::new_stateful(
            "sort-direction",
            Some(glib::VariantTy::STRING),
            &direction_id(state.descending).to_variant(),
        );
        direction.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |action, param| {
                if let Some(id) = param.and_then(|v| v.str()) {
                    action.set_state(&id.to_variant());
                    window.set_sort_descending(id == "descending");
                }
            }
        ));
        self.add_action(&direction);

        let preferences = gio::SimpleAction::new("preferences", None);
        preferences.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| crate::preferences::present(&window)
        ));
        self.add_action(&preferences);

        let find_duplicates = gio::SimpleAction::new("find-duplicates", None);
        find_duplicates.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| window.show_duplicates()
        ));
        self.add_action(&find_duplicates);

        let write_xmp = gio::SimpleAction::new("write-xmp", None);
        write_xmp.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| window.write_xmp_sidecars()
        ));
        self.add_action(&write_xmp);
    }

    /// Write XMP sidecar files (`photo.jpg.xmp`) for the current selection — or
    /// the whole visible grid if nothing is selected — so other photo tools can
    /// read Vitrine's ratings, comments, and tags. Non-destructive: originals
    /// are never touched.
    fn write_xmp_sidecars(&self) {
        use vitrine_engine::sidecar_path;

        // The images to export: the selection, or everything if none is selected.
        let items: Vec<(String, PathBuf)> = {
            let selected = self.selected_positions();
            let positions: Vec<u32> = if selected.is_empty() {
                self.model()
                    .map(|m| (0..m.n_items()).collect())
                    .unwrap_or_default()
            } else {
                selected
            };
            let Some(model) = self.model() else {
                return;
            };
            positions
                .into_iter()
                .filter_map(|pos| model.item(pos).and_downcast::<ImageObject>())
                .filter_map(|item| {
                    let hash = item.content_hash();
                    let path = item.file().path()?;
                    (!hash.is_empty()).then_some((hash, path))
                })
                .collect()
        };

        if items.is_empty() {
            self.toast("Nothing to export");
            return;
        }

        // A short-lived read connection; the index may not have spawned yet.
        let Ok(db) = Db::open(crate::index::index_db_path()) else {
            self.toast("Could not open the catalog");
            return;
        };

        let mut written = 0usize;
        let mut skipped = 0usize;
        for (hash, path) in &items {
            let Ok(meta) = db.xmp_for_hash(hash) else {
                continue;
            };
            if meta.is_empty() {
                skipped += 1;
                continue;
            }
            if std::fs::write(sidecar_path(path), meta.to_packet()).is_ok() {
                written += 1;
            }
        }

        self.toast(&match (written, skipped) {
            (0, _) => "No ratings, comments, or tags to export".to_string(),
            (1, _) => "Wrote 1 XMP sidecar".to_string(),
            (n, _) => format!("Wrote {n} XMP sidecars"),
        });
    }

    /// Enqueue a library root for background indexing (used by Preferences).
    pub fn index_root(&self, path: PathBuf) {
        if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
            indexer.request(path);
        }
    }

    /// Change the sort field (and persist it), re-sorting the grid live.
    fn set_sort_field(&self, field: SortField) {
        let mut state = self.imp().sort_state.get();
        state.field = field;
        self.apply_sort_state(state);
        crate::settings::Settings::load().set_sort_field(field.id());
    }

    /// Flip ascending/descending (and persist it), re-sorting the grid live.
    fn set_sort_descending(&self, descending: bool) {
        let mut state = self.imp().sort_state.get();
        state.descending = descending;
        self.apply_sort_state(state);
        crate::settings::Settings::load().set_sort_descending(descending);
    }

    /// Store the new sort state and tell the sorter to re-run — GTK reorders the
    /// model in place, so the grid keeps its selection and scroll position.
    fn apply_sort_state(&self, state: SortState) {
        let imp = self.imp();
        imp.sort_state.set(state);
        if let Some(sorter) = imp.sorter.borrow().as_ref() {
            sorter.changed(gtk::SorterChange::Different);
        }
    }

    /// Spawn the background indexer and start draining its progress into the
    /// banner on the main context. The index lives in the app's private data
    /// dir (per-app under Flatpak), so it never touches the browsed folders.
    fn setup_indexer(&self) {
        let indexer = Indexer::spawn(crate::index::index_db_path());
        let progress = indexer.progress.clone();
        // Drain any files left un-enriched by a previous session.
        indexer.start_enrichment(|| {});
        // Index the persistent library roots in the background at launch, so the
        // index covers them even before (or without) browsing.
        for root in crate::settings::Settings::load().roots() {
            indexer.request(root);
        }
        *self.imp().indexer.borrow_mut() = Some(indexer);

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                while let Ok(msg) = progress.recv().await {
                    window.on_index_progress(msg);
                }
            }
        ));
    }

    /// Reflect indexer progress in the banner (a transient status line; browsing
    /// is unaffected either way).
    fn on_index_progress(&self, msg: IndexProgress) {
        let banner = &self.imp().index_banner;
        match msg {
            IndexProgress::Started { total } => {
                if total > 0 {
                    banner.set_title(&gettextrs::gettext("Indexing library…"));
                    banner.set_revealed(true);
                }
            }
            IndexProgress::Advanced { done, total } => {
                banner.set_title(&format!(
                    "{} ({done} / {total})",
                    gettextrs::gettext("Indexing library…")
                ));
            }
            IndexProgress::Finished { .. } => {
                banner.set_revealed(false);
                // Identity rows exist now → stamp the grid's items with their
                // content hash + rating (star overlays appear; keyboard rating
                // can key writes). Then backfill dimensions/EXIF/pHash.
                self.restamp_store();
                if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
                    indexer.start_enrichment(|| {});
                }
            }
            IndexProgress::CollectionsChanged => self.refresh_collections(),
        }
    }

    /// Enqueue a folder for background indexing, if it has a local path. Portal
    /// document paths are indexed as-is; content-hash keying keeps tags stable
    /// even as those opaque paths churn across sessions.
    fn index_folder(&self, folder: &gio::File) {
        if let Some(path) = folder.path() {
            if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
                indexer.request(path);
            }
        }
    }

    fn open_folder_dialog(&self) {
        let dialog = gtk::FileDialog::builder()
            .title(gettextrs::gettext("Open Folder"))
            .modal(true)
            .build();

        dialog.select_folder(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result| match result {
                    Ok(folder) => window.load_folder(folder),
                    Err(err) => {
                        if !err.matches(gtk::DialogError::Dismissed) {
                            eprintln!("open folder: {err}");
                        }
                    }
                }
            ),
        );
    }

    /// Open a location from the file manager / command line: a folder is loaded
    /// directly; a file loads its parent folder.
    pub fn open_location(&self, file: gio::File) {
        let folder =
            match file.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE) {
                gio::FileType::Directory => file,
                _ => match file.parent() {
                    Some(parent) => parent,
                    None => return,
                },
            };
        self.load_folder(folder);
    }

    /// Enumerate `folder`'s image children asynchronously and populate the grid.
    fn load_folder(&self, folder: gio::File) {
        // Opening a new folder while the single-image viewer is up returns to the
        // grid — otherwise the viewer would keep showing the *old* folder's image
        // and Properties (show_position never re-runs) with a half-swapped
        // filmstrip. Re-entering the viewer then reopens it fresh on the new folder.
        let nav = &self.imp().nav_view;
        if nav
            .visible_page()
            .and_then(|p| p.tag())
            .map(|t| t == "viewer")
            .unwrap_or(false)
        {
            nav.pop_to_tag("browser");
        }

        let new_path = folder.path();
        // Record the move (for Back) and remember the folder (for the rating
        // stamp scope).
        if let Some(path) = &new_path {
            self.set_location(Location::Folder(path.clone()));
        }
        *self.imp().current_folder.borrow_mut() = new_path;
        // Kick off background indexing in parallel — the grid never waits on it.
        self.index_folder(&folder);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = window)]
            self,
            async move {
                match collect_images(&folder).await {
                    Ok(items) => window.populate(items),
                    Err(err) => eprintln!("enumerate {}: {err}", folder.uri()),
                }
            }
        ));
    }

    fn populate(&self, items: Vec<ImageObject>) {
        let imp = self.imp();
        // Stamp hash + rating before the items are shown, so cells paint their
        // star overlay on first bind (a re-stamp follows once indexing catches up).
        self.stamp_annotations(&items);
        // A new folder invalidates any queued/debounced loads from the old one.
        imp.pending.borrow_mut().clear();
        imp.load_queue.borrow_mut().clear();
        imp.store.remove_all();
        imp.store.extend_from_slice(&items);
        imp.content_stack
            .set_visible_child_name(if items.is_empty() { "empty" } else { "grid" });
        // The grid's SortListModel orders items live per the active sort — no
        // manual reordering needed here.
        // Dev aid: VITRINE_OPEN=<index> auto-opens the viewer (for screenshots).
        if let Some(idx) = std::env::var("VITRINE_OPEN")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            if idx < self.imp().store.n_items() {
                self.open_viewer(idx);
            }
        }
        self.maybe_screenshot();
        self.maybe_scrolltest();
        self.maybe_loadtest();
        self.maybe_sorttest();
        self.maybe_tagtest();
        self.maybe_filtertest();
        if std::env::var_os("VITRINE_DUPES").is_some() {
            glib::timeout_add_seconds_local_once(
                1,
                glib::clone!(
                    #[weak(rename_to = window)]
                    self,
                    move || window.show_duplicates()
                ),
            );
        }
    }

    /// Dev aid: if `VITRINE_FILTER=<n>` is set, reveal the filter bar and apply a
    /// minimum-rating filter of `n` — for verifying grid filtering.
    fn maybe_filtertest(&self) {
        let Some(spec) = std::env::var_os("VITRINE_FILTER") else {
            return;
        };
        let min: u32 = spec.to_string_lossy().parse().unwrap_or(0);
        glib::timeout_add_seconds_local_once(
            1,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    let imp = window.imp();
                    imp.filter_button.set_active(true);
                    imp.rating_filter.set_selected(min);
                    let shown = window.model().map_or(0, |m| m.n_items());
                    eprintln!("VITRINE_FILTER min={min} → {shown} visible");
                }
            ),
        );
    }

    /// Dev aid: if `VITRINE_TAG=<name>` is set, select all, apply that tag to the
    /// selection, and reveal the tag popover — for verifying tagging end-to-end.
    fn maybe_tagtest(&self) {
        let Some(name) = std::env::var_os("VITRINE_TAG") else {
            return;
        };
        let name = name.to_string_lossy().into_owned();
        glib::timeout_add_seconds_local_once(
            1,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    if let Some(sel) = window.imp().selection.borrow().as_ref() {
                        sel.select_all();
                    }
                    let hashes = window.selected_hashes();
                    if let Some(indexer) = window.imp().indexer.borrow().as_ref() {
                        indexer.annotator().tag(&name, &hashes, true);
                    }
                    eprintln!("VITRINE_TAG applied '{name}' to {} images", hashes.len());
                    window.imp().tag_button.popup();
                }
            ),
        );
    }

    /// Dev aid: if `VITRINE_PREFS` is set, open Preferences after a beat (for
    /// screenshots of the settings dialog).
    fn maybe_prefs(&self) {
        if std::env::var_os("VITRINE_PREFS").is_none() {
            return;
        }
        glib::timeout_add_seconds_local_once(
            1,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || crate::preferences::present(&window)
            ),
        );
    }

    /// Dev aid: if `VITRINE_SORT=<field>[:desc]` is set, apply that sort, print
    /// the resulting top order, and quit — for verifying grid sorting end-to-end.
    fn maybe_sorttest(&self) {
        let Some(spec) = std::env::var_os("VITRINE_SORT") else {
            return;
        };
        let spec = spec.to_string_lossy().into_owned();
        let (field, descending) = match spec.split_once(':') {
            Some((f, dir)) => (f.to_string(), dir == "desc"),
            None => (spec.clone(), false),
        };
        glib::timeout_add_seconds_local_once(
            1,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.set_sort_field(SortField::from_id(&field));
                    window.set_sort_descending(descending);
                    let top: Vec<String> = window
                        .model()
                        .map(|m| {
                            (0..m.n_items().min(6))
                                .filter_map(|i| m.item(i).and_downcast::<ImageObject>())
                                .map(|o| o.display_name().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    eprintln!("VITRINE_SORT[{spec}] top: {}", top.join(", "));
                    if let Some(app) = window.application() {
                        app.quit();
                    }
                }
            ),
        );
    }

    /// Dev aid: if `VITRINE_CYCLE=/a:/b:/c` is set, open each folder in turn every
    /// ~2.5s, looping — to reproduce leaks from repeated folder switching.
    fn maybe_cycle(&self) {
        let Some(spec) = std::env::var_os("VITRINE_CYCLE") else {
            return;
        };
        let dirs: Vec<String> = spec
            .to_string_lossy()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if dirs.is_empty() {
            return;
        }
        let idx = std::rc::Rc::new(std::cell::Cell::new(0usize));
        glib::timeout_add_seconds_local(
            2,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let i = idx.get();
                    idx.set(i + 1);
                    let dir = &dirs[i % dirs.len()];
                    window.open_location(gio::File::for_path(dir));
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Dev aid: if `VITRINE_LOADTEST` is set, sit still while thumbnails load and
    /// report the worst main-loop stall (a 16ms heartbeat measures its own
    /// lateness) — this is what "sluggish while populating" feels like.
    fn maybe_loadtest(&self) {
        if std::env::var_os("VITRINE_LOADTEST").is_none() {
            return;
        }
        use std::time::Instant;
        let last = std::rc::Rc::new(std::cell::Cell::new(Instant::now()));
        let max_ms = std::rc::Rc::new(std::cell::Cell::new(0u128));
        let start = Instant::now();
        glib::timeout_add_local(
            std::time::Duration::from_millis(16),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let now = Instant::now();
                    let gap = now.saturating_duration_since(last.get()).as_millis();
                    if gap > max_ms.get() {
                        max_ms.set(gap);
                    }
                    last.set(now);
                    if start.elapsed().as_secs() >= 10 {
                        eprintln!("LOADTEST max main-loop stall = {} ms", max_ms.get());
                        if let Some(app) = window.application() {
                            app.quit();
                        }
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                }
            ),
        );
    }

    /// VITRINE_SOAK=<dir1:dir2:...>: a scripted UI journey for soak-testing (pair
    /// with VITRINE_DEBUG / VITRINE_NOCACHE). Per dir: open, scroll the grid, open
    /// the viewer, toggle Properties (2s each way), scroll the filmstrip, back out.
    /// Two laps, then a back-button pass, then quit. `SOAK` phase markers interleave
    /// with the `VDBG` stats so they can be correlated.
    fn maybe_soak(&self) {
        let Some(spec) = std::env::var_os("VITRINE_SOAK") else {
            return;
        };
        let dirs: Vec<PathBuf> = spec
            .to_string_lossy()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if dirs.is_empty() {
            return;
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                use std::time::Duration;
                // Let the window map before we start driving it.
                glib::timeout_future(Duration::from_millis(1500)).await;
                for lap in 0..2 {
                    eprintln!("SOAK ===== lap {lap} =====");
                    for dir in &dirs {
                        eprintln!("SOAK open-dir {}", dir.display());
                        win.open_location(gio::File::for_path(dir));
                        glib::timeout_future(Duration::from_millis(1000)).await;

                        eprintln!("SOAK scroll-grid");
                        win.soak_scroll_grid().await;

                        let n = win.model().map_or(0, |m| m.n_items());
                        if n == 0 {
                            continue;
                        }
                        eprintln!("SOAK open-viewer");
                        win.open_viewer(n / 2);
                        glib::timeout_future(Duration::from_millis(700)).await;

                        let viewer = win.imp().viewer.borrow().as_ref().cloned();
                        if let Some(v) = viewer {
                            eprintln!("SOAK properties-on");
                            v.set_properties_shown(true);
                            glib::timeout_future(Duration::from_secs(2)).await;
                            eprintln!("SOAK properties-off");
                            v.set_properties_shown(false);
                            glib::timeout_future(Duration::from_millis(500)).await;
                        }

                        eprintln!("SOAK scroll-filmstrip");
                        win.soak_scroll_filmstrip().await;

                        eprintln!("SOAK back-to-grid");
                        win.imp().nav_view.pop();
                        glib::timeout_future(Duration::from_millis(600)).await;
                    }
                }
                eprintln!("SOAK ===== back-button pass =====");
                for _ in 0..dirs.len().saturating_sub(1) {
                    win.go_back();
                    glib::timeout_future(Duration::from_millis(700)).await;
                }
                eprintln!("SOAK done");
                if let Some(app) = win.application() {
                    app.quit();
                }
            }
        ));
    }

    /// VITRINE_OPENFOLDER=<dirA>:<dirB>: reproduce "open a folder while in the
    /// viewer". Open A → viewer → Properties → open B *while the viewer is up* →
    /// check we returned to the grid on B (not stranded stale) → re-enter the
    /// viewer on B and scroll the filmstrip (loads should register). Logs OFTEST
    /// markers (pair with VITRINE_DEBUG to watch decode activity).
    fn maybe_openfolder_test(&self) {
        let Some(spec) = std::env::var_os("VITRINE_OPENFOLDER") else {
            return;
        };
        let dirs: Vec<PathBuf> = spec
            .to_string_lossy()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if dirs.len() < 2 {
            return;
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                use std::time::Duration;
                glib::timeout_future(Duration::from_millis(1500)).await;
                eprintln!("OFTEST open dir A: {}", dirs[0].display());
                win.open_location(gio::File::for_path(&dirs[0]));
                glib::timeout_future(Duration::from_secs(2)).await;
                let na = win.model().map_or(0, |m| m.n_items());
                eprintln!("OFTEST dir A items={na}");
                eprintln!("OFTEST open viewer + properties");
                win.open_viewer(0);
                glib::timeout_future(Duration::from_millis(1000)).await;
                if let Some(v) = win.imp().viewer.borrow().as_ref().cloned() {
                    v.set_properties_shown(true);
                }
                glib::timeout_future(Duration::from_millis(1500)).await;
                eprintln!(
                    "OFTEST *** open dir B WHILE IN VIEWER: {}",
                    dirs[1].display()
                );
                win.open_location(gio::File::for_path(&dirs[1]));
                glib::timeout_future(Duration::from_secs(2)).await;
                let page = win
                    .imp()
                    .nav_view
                    .visible_page()
                    .and_then(|p| p.tag())
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                let nb = win.model().map_or(0, |m| m.n_items());
                eprintln!("OFTEST after open-B: visible_page={page:?} grid_items={nb} (want browser + B's count)");
                eprintln!("OFTEST re-enter viewer on B + scroll filmstrip (loads should register)");
                win.open_viewer(nb / 2);
                glib::timeout_future(Duration::from_millis(800)).await;
                win.soak_scroll_filmstrip().await;
                glib::timeout_future(Duration::from_millis(1500)).await;
                eprintln!("OFTEST done");
                if let Some(app) = win.application() {
                    app.quit();
                }
            }
        ));
    }

    /// Fling the grid top→bottom in steps (VITRINE_SOAK).
    async fn soak_scroll_grid(&self) {
        use std::time::Duration;
        let adj = self.imp().grid_scroller.vadjustment();
        let span = (adj.upper() - adj.page_size() - adj.lower()).max(0.0);
        for i in 0..=24 {
            adj.set_value(adj.lower() + span * (i as f64 / 24.0));
            glib::timeout_future(Duration::from_millis(60)).await;
        }
    }

    /// Scroll the current viewer's filmstrip end-to-end (VITRINE_SOAK).
    async fn soak_scroll_filmstrip(&self) {
        use std::time::Duration;
        let viewer = self.imp().viewer.borrow().as_ref().cloned();
        let Some(viewer) = viewer else {
            return;
        };
        for i in 0..=15 {
            viewer.soak_scroll_filmstrip_to(i as f64 / 15.0);
            glib::timeout_future(Duration::from_millis(70)).await;
        }
    }

    /// Dev aid: if `VITRINE_SCROLLTEST` is set, fast-scroll the grid top→bottom
    /// (churning thousands of cells) then quit — for verifying bounded memory.
    fn maybe_scrolltest(&self) {
        if std::env::var_os("VITRINE_SCROLLTEST").is_none() {
            return;
        }
        let step = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let scroller = self.imp().grid_scroller.clone();
        glib::timeout_add_local(
            std::time::Duration::from_millis(15),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let adj = scroller.vadjustment();
                    let s = step.get();
                    step.set(s + 1);
                    let span = (adj.upper() - adj.page_size() - adj.lower()).max(0.0);
                    let frac = (s as f64 / 600.0).min(1.0);
                    adj.set_value(adj.lower() + frac * span);
                    if s >= 600 {
                        if let Some(app) = window.application() {
                            app.quit();
                        }
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                }
            ),
        );
    }

    /// Dev aid: if `VITRINE_SHOT=/path.png` is set, snapshot the window (after a
    /// beat, so thumbnails decode) and quit. Renders through GTK's own GSK
    /// renderer, so it needs no external screenshot tool or compositor.
    fn maybe_screenshot(&self) {
        let Some(path) = std::env::var_os("VITRINE_SHOT") else {
            return;
        };
        glib::timeout_add_seconds_local_once(
            3,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    if let Some(texture) = window.snapshot_to_texture() {
                        if let Err(e) = texture.save_to_png(&path) {
                            eprintln!("screenshot: {e}");
                        }
                    }
                    if let Some(app) = window.application() {
                        app.quit();
                    }
                }
            ),
        );
    }

    fn snapshot_to_texture(&self) -> Option<gtk::gdk::Texture> {
        let renderer = self.native()?.renderer()?;
        let paintable = gtk::WidgetPaintable::new(Some(self));
        let (w, h) = (self.width() as f64, self.height() as f64);
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(snapshot.upcast_ref::<gtk::gdk::Snapshot>(), w, h);
        let node = snapshot.to_node()?;
        Some(renderer.render_texture(node, None))
    }
}

/// The menu-action target string for a direction.
fn direction_id(descending: bool) -> &'static str {
    if descending {
        "descending"
    } else {
        "ascending"
    }
}

/// Compare two items for the grid sorter: the chosen field, then a case-folded
/// name tiebreak for a stable order, all reversed together when descending.
fn compare_images(a: &ImageObject, b: &ImageObject, state: SortState) -> gtk::Ordering {
    use std::cmp::Ordering;
    let by_name = || {
        a.display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase())
    };
    let primary = match state.field {
        SortField::Name => Ordering::Equal,
        SortField::Size => a.size().cmp(&b.size()),
        SortField::Modified => a.mtime().cmp(&b.mtime()),
        SortField::Type => a
            .content_type()
            .to_lowercase()
            .cmp(&b.content_type().to_lowercase()),
    };
    let ord = primary.then_with(by_name);
    let ord = if state.descending { ord.reverse() } else { ord };
    match ord {
        Ordering::Less => gtk::Ordering::Smaller,
        Ordering::Equal => gtk::Ordering::Equal,
        Ordering::Greater => gtk::Ordering::Larger,
    }
}

/// The sub-directories of `dir` as a `ListModel` of `gio::File` for the folder
/// tree, or `None` if it has none (so the tree shows no expander). Hidden dirs
/// are skipped; enumerated synchronously (only on user expand).
fn dir_children(dir: &gio::File) -> Option<gio::ListModel> {
    let enumerator = dir
        .enumerate_children(
            "standard::name,standard::type",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let mut dirs: Vec<gio::File> = Vec::new();
    while let Ok(Some(info)) = enumerator.next_file(gio::Cancellable::NONE) {
        if info.file_type() == gio::FileType::Directory
            && !info.name().to_string_lossy().starts_with('.')
        {
            dirs.push(enumerator.child(&info));
        }
    }
    if dirs.is_empty() {
        return None;
    }
    dirs.sort_by_key(|f| {
        f.basename()
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    let store = gio::ListStore::new::<gio::File>();
    store.extend_from_slice(&dirs);
    Some(store.upcast())
}

/// Async-enumerate `folder`, returning its browsable images sorted by name.
async fn collect_images(folder: &gio::File) -> Result<Vec<ImageObject>, glib::Error> {
    let enumerator = folder
        .enumerate_children_future(
            ENUMERATE_ATTRS,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await?;

    let mut items: Vec<ImageObject> = Vec::new();
    loop {
        let infos = enumerator
            .next_files_future(64, glib::Priority::DEFAULT)
            .await?;
        if infos.is_empty() {
            break;
        }
        for info in infos {
            if info.file_type() != gio::FileType::Regular {
                continue;
            }
            let Some(content_type) = info.content_type() else {
                continue;
            };
            if !crate::decode::is_supported_image(&content_type) {
                continue;
            }
            let child = enumerator.child(&info);
            let display = info.display_name();
            let mtime = info.attribute_uint64("time::modified") as i64;
            let size = info.size();
            items.push(ImageObject::new(
                child,
                &display,
                mtime,
                size,
                &content_type,
            ));
        }
    }

    items.sort_by(|a, b| {
        a.display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase())
    });
    Ok(items)
}
