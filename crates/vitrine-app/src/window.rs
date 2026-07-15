//! The main application window: the browser grid and folder-open flow.
//!
//! Phase 1 browser. Opening a folder enumerates its images asynchronously into
//! a `gio::ListStore` of [`ImageObject`]s, shown in a virtualized `GtkGridView`
//! with `GtkMultiSelection` (rubber-band + Ctrl/Shift ranges). Thumbnails decode
//! lazily per visible cell (see [`crate::grid_cell`]). Activating a cell pushes
//! the [`crate::viewer`] page onto the `AdwNavigationView`, sharing the store.

use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use vitrine_engine::{Db, Direction, Query, SortKey};

use crate::grid_cell::VitrineGridCell;
use crate::image_object::ImageObject;
use crate::index::{IndexProgress, Indexer};
use crate::viewer::VitrineViewer;

/// Gio attributes fetched per child when enumerating a folder.
const ENUMERATE_ATTRS: &str =
    "standard::name,standard::display-name,standard::content-type,standard::type,time::modified";

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
        pub open_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub places_list: TemplateChild<gtk::ListBox>,
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

        /// Background library indexer (created in `constructed`). Owns the one
        /// writer `Db`; the UI only enqueues folders and reads progress.
        pub indexer: RefCell<Option<Indexer>>,
        /// A read-only connection to the index, opened lazily for grid sorting
        /// (the writer stays on the indexer thread; this only ever queries).
        pub read_db: RefCell<Option<Db>>,
        /// Local path of the folder currently shown (for scoping sort queries).
        pub current_folder: RefCell<Option<PathBuf>>,
        /// Active grid sort (key + direction). Default: by name, ascending.
        pub sort: std::cell::Cell<(SortKey, Direction)>,

        /// Backing model for the grid (one row per image file).
        pub store: gio::ListStore,
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
        /// Debounce timer for flushing `pending` after scrolling stops.
        pub flush_source: RefCell<Option<glib::SourceId>>,
    }

    impl Default for VitrineWindow {
        fn default() -> Self {
            Self {
                content_stack: Default::default(),
                grid_scroller: Default::default(),
                open_button: Default::default(),
                places_list: Default::default(),
                nav_view: Default::default(),
                toast_overlay: Default::default(),
                icon_smaller: Default::default(),
                icon_larger: Default::default(),
                index_banner: Default::default(),
                indexer: RefCell::new(None),
                read_db: RefCell::new(None),
                current_folder: RefCell::new(None),
                sort: std::cell::Cell::new((SortKey::Name, Direction::Asc)),
                store: gio::ListStore::new::<ImageObject>(),
                selection: RefCell::new(None),
                grid_view: RefCell::new(None),
                icon_index: std::cell::Cell::new(DEFAULT_ICON),
                viewer: RefCell::new(None),
                thumb_cache: crate::thumbnails::new_ram_cache(),
                pending: RefCell::new(Vec::new()),
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

        let selection = gtk::MultiSelection::new(Some(imp.store.clone()));
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
                let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                match (ctrl, key) {
                    (true, gtk::gdk::Key::plus | gtk::gdk::Key::equal | gtk::gdk::Key::KP_Add) => {
                        window.change_icon(1)
                    }
                    (true, gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract) => {
                        window.change_icon(-1)
                    }
                    (true, gtk::gdk::Key::_0 | gtk::gdk::Key::KP_0) => window.reset_icon(),
                    (_, gtk::gdk::Key::Delete) => window.trash_selected(),
                    (_, gtk::gdk::Key::space | gtk::gdk::Key::KP_Space) => {
                        window.preview_selected()
                    }
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
        for (weak_cell, item, position) in pending {
            if let Some(cell) = weak_cell.upgrade() {
                if cell.item().as_ref() == Some(&item) {
                    lo = lo.min(position);
                    hi = hi.max(position);
                }
                cell.load(item, imp.thumb_cache.clone());
            }
        }
        if lo <= hi {
            self.prefetch_range(lo, hi);
        }
    }

    /// Prefetch the items just outside `[lo, hi]` into the RAM cache.
    fn prefetch_range(&self, lo: u32, hi: u32) {
        let n = self.imp().store.n_items();
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
        let Some(item) = imp.store.item(position).and_downcast::<ImageObject>() else {
            return;
        };
        if item.has_failed() {
            return;
        }
        let key = crate::thumbnails::ram_key(&item.file().uri(), load_size);
        if imp.thumb_cache.borrow().contains(&key) {
            return;
        }
        let file = item.file();
        let mtime = item.mtime();
        let cache = imp.thumb_cache.clone();
        let renderer_widget = crate::thumbnails::renderer_source(self);
        glib::spawn_future_local(async move {
            let _permit = crate::thumbnails::load_gate().acquire().await;
            if cache.borrow().contains(&key) {
                return;
            }
            if let Some(texture) =
                crate::thumbnails::load(file, mtime, load_size, renderer_widget).await
            {
                cache.borrow_mut().put(
                    key,
                    texture.clone(),
                    crate::thumbnails::texture_cost(&texture),
                );
            }
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
                    window
                        .imp()
                        .pending
                        .borrow_mut()
                        .push((cell.downgrade(), item, position));
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

    /// Positions currently selected in the grid, ascending.
    fn selected_positions(&self) -> Vec<u32> {
        let Some(selection) = self.imp().selection.borrow().clone() else {
            return Vec::new();
        };
        let n = self.imp().store.n_items();
        (0..n).filter(|&pos| selection.is_selected(pos)).collect()
    }

    /// Space: quick-preview the first selected image in the viewer.
    fn preview_selected(&self) {
        if let Some(&pos) = self.selected_positions().first() {
            self.open_viewer(pos);
        }
    }

    /// Delete: move the selected images to the trash (reversible — never unlink),
    /// dropping each from the grid as it is trashed.
    fn trash_selected(&self) {
        let items: Vec<ImageObject> = self
            .selected_positions()
            .into_iter()
            .filter_map(|pos| self.imp().store.item(pos).and_downcast::<ImageObject>())
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
        viewer.open(imp.store.clone(), position, imp.thumb_cache.clone());
        if imp.nav_view.find_page("viewer").is_none() {
            imp.nav_view.push(&viewer);
        } else {
            imp.nav_view.pop_to_tag("viewer");
        }
    }

    fn setup_actions(&self) {
        let imp = self.imp();

        imp.open_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.open_folder_dialog()
        ));

        imp.places_list.connect_row_activated(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| window.open_folder_dialog()
        ));

        // Stateful sort action driving the header sort menu (radio items).
        let sort = gio::SimpleAction::new_stateful(
            "sort",
            Some(glib::VariantTy::STRING),
            &"name".to_variant(),
        );
        sort.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |action, param| {
                if let Some(preset) = param.and_then(|v| v.str()) {
                    action.set_state(&preset.to_variant());
                    window.set_sort(preset);
                }
            }
        ));
        self.add_action(&sort);

        let preferences = gio::SimpleAction::new("preferences", None);
        preferences.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| crate::preferences::present(&window)
        ));
        self.add_action(&preferences);
    }

    /// Enqueue a library root for background indexing (used by Preferences).
    pub fn index_root(&self, path: PathBuf) {
        if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
            indexer.request(path);
        }
    }

    /// Map a menu preset to a (key, direction) and re-sort the grid.
    fn set_sort(&self, preset: &str) {
        let sort = match preset {
            "newest" => (SortKey::DateTaken, Direction::Desc),
            "oldest" => (SortKey::DateTaken, Direction::Asc),
            "largest" => (SortKey::Area, Direction::Desc),
            "size" => (SortKey::Size, Direction::Desc),
            _ => (SortKey::Name, Direction::Asc),
        };
        self.imp().sort.set(sort);
        self.apply_sort();
    }

    /// Reorder the grid to the active sort. `Name` sorts by display name with no
    /// database round-trip; metadata sorts query the index for the current
    /// folder's rows in order and reorder the store to match, with any rows the
    /// index hasn't caught up on trailing in name order.
    fn apply_sort(&self) {
        let imp = self.imp();
        let (key, dir) = imp.sort.get();

        let mut items: Vec<ImageObject> = (0..imp.store.n_items())
            .filter_map(|i| imp.store.item(i).and_downcast::<ImageObject>())
            .collect();
        if items.is_empty() {
            return;
        }

        if key == SortKey::Name {
            items.sort_by(|a, b| {
                a.display_name()
                    .to_lowercase()
                    .cmp(&b.display_name().to_lowercase())
            });
        } else {
            let Some(folder) = imp.current_folder.borrow().clone() else {
                return;
            };
            let rank = self.query_ranks(&folder, key, dir);
            if rank.is_empty() {
                return; // index not ready for this folder yet — leave as-is
            }
            // Rows the index knows sort by its rank; unknown rows trail by name.
            items.sort_by(|a, b| {
                let ra = item_path(a).and_then(|p| rank.get(&p)).copied();
                let rb = item_path(b).and_then(|p| rank.get(&p)).copied();
                match (ra, rb) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a
                        .display_name()
                        .to_lowercase()
                        .cmp(&b.display_name().to_lowercase()),
                }
            });
        }

        imp.store.remove_all();
        imp.store.extend_from_slice(&items);
    }

    /// Query the index for `folder`'s files in `(key, dir)` order, returning a
    /// path→rank map. Opens the read-only connection on first use.
    fn query_ranks(
        &self,
        folder: &std::path::Path,
        key: SortKey,
        dir: Direction,
    ) -> std::collections::HashMap<String, usize> {
        let imp = self.imp();
        if imp.read_db.borrow().is_none() {
            match Db::open(crate::index::index_db_path()) {
                Ok(db) => *imp.read_db.borrow_mut() = Some(db),
                Err(e) => {
                    glib::g_warning!("vitrine", "open read db: {e}");
                    return std::collections::HashMap::new();
                }
            }
        }
        let db = imp.read_db.borrow();
        let Some(db) = db.as_ref() else {
            return std::collections::HashMap::new();
        };
        let query = Query {
            under: Some(folder.to_string_lossy().into_owned()),
            sort: key,
            direction: dir,
            ..Default::default()
        };
        match db.query(&query) {
            Ok(rows) => rows
                .into_iter()
                .enumerate()
                .map(|(i, r)| (r.path, i))
                .collect(),
            Err(e) => {
                glib::g_warning!("vitrine", "sort query: {e}");
                std::collections::HashMap::new()
            }
        }
    }

    /// Spawn the background indexer and start draining its progress into the
    /// banner on the main context. The index lives in the app's private data
    /// dir (per-app under Flatpak), so it never touches the browsed folders.
    fn setup_indexer(&self) {
        let indexer = Indexer::spawn(crate::index::index_db_path());
        let progress = indexer.progress.clone();
        // Drain any files left un-enriched by a previous session, refreshing the
        // sort once their metadata lands.
        indexer.start_enrichment(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move || window.apply_sort()
        ));
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
                // Identity rows exist now → name/size/mtime sorts can apply
                // immediately; date/area sorts refine as enrichment fills in.
                self.apply_sort();
                if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
                    indexer.start_enrichment(glib::clone!(
                        #[weak(rename_to = window)]
                        self,
                        move || window.apply_sort()
                    ));
                }
            }
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
        // Remember the folder so metadata sorts can scope to it.
        *self.imp().current_folder.borrow_mut() = folder.path();
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
        imp.store.remove_all();
        imp.store.extend_from_slice(&items);
        imp.content_stack
            .set_visible_child_name(if items.is_empty() { "empty" } else { "grid" });
        // Apply the active sort (a no-op reshuffle for Name; metadata sorts
        // reorder once the index has this folder — and refresh via apply_sort
        // on scan/enrichment completion).
        self.apply_sort();
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

    /// Dev aid: if `VITRINE_SORT=<preset>` is set, apply that sort after a beat
    /// (letting the background index settle), print the resulting top order, and
    /// quit — for verifying metadata sorting end-to-end.
    fn maybe_sorttest(&self) {
        let Some(preset) = std::env::var_os("VITRINE_SORT") else {
            return;
        };
        let preset = preset.to_string_lossy().into_owned();
        glib::timeout_add_seconds_local_once(
            2,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.set_sort(&preset);
                    let imp = window.imp();
                    let top: Vec<String> = (0..imp.store.n_items().min(6))
                        .filter_map(|i| imp.store.item(i).and_downcast::<ImageObject>())
                        .map(|o| o.display_name().to_string())
                        .collect();
                    eprintln!("VITRINE_SORT[{preset}] top: {}", top.join(", "));
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

/// The local filesystem path of an image item, as stored in the index.
fn item_path(item: &ImageObject) -> Option<String> {
    item.file().path().map(|p| p.to_string_lossy().into_owned())
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
            items.push(ImageObject::new(child, &display, mtime));
        }
    }

    items.sort_by(|a, b| {
        a.display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase())
    });
    Ok(items)
}
