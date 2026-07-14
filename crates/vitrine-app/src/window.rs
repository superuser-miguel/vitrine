//! The main application window: the browser grid and folder-open flow.
//!
//! Phase 1 browser. Opening a folder enumerates its images asynchronously into
//! a `gio::ListStore` of [`ImageObject`]s, shown in a virtualized `GtkGridView`
//! with `GtkMultiSelection` (rubber-band + Ctrl/Shift ranges). Thumbnails decode
//! lazily per visible cell (see [`crate::grid_cell`]). Activating a cell pushes
//! the [`crate::viewer`] page onto the `AdwNavigationView`, sharing the store.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use crate::grid_cell::VitrineGridCell;
use crate::image_object::ImageObject;
use crate::viewer::VitrineViewer;

/// Gio attributes fetched per child when enumerating a folder.
const ENUMERATE_ATTRS: &str =
    "standard::name,standard::display-name,standard::content-type,standard::type";

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, CompositeTemplate)]
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

        /// Backing model for the grid (one row per image file).
        pub store: gio::ListStore,
        /// Selection model the grid renders.
        pub selection: RefCell<Option<gtk::MultiSelection>>,
        /// The viewer page, created lazily on first activation.
        pub viewer: RefCell<Option<VitrineViewer>>,
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
                store: gio::ListStore::new::<ImageObject>(),
                selection: RefCell::new(None),
                viewer: RefCell::new(None),
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

    /// Build the grid: a `SignalListItemFactory` that produces [`VitrineGridCell`]s
    /// over a `GtkMultiSelection` of the image store.
    fn setup_grid(&self) {
        let imp = self.imp();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("factory item is a ListItem");
            let cell = VitrineGridCell::default();
            list_item.set_child(Some(&cell));
        });
        factory.connect_bind(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("factory item is a ListItem");
            let cell = list_item
                .child()
                .and_downcast::<VitrineGridCell>()
                .expect("cell set up in setup");
            let item = list_item
                .item()
                .and_downcast::<ImageObject>()
                .expect("model holds ImageObjects");
            cell.bind(&item);
        });
        factory.connect_unbind(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("factory item is a ListItem");
            if let Some(cell) = list_item.child().and_downcast::<VitrineGridCell>() {
                cell.unbind();
            }
        });

        let selection = gtk::MultiSelection::new(Some(imp.store.clone()));
        let grid_view = gtk::GridView::new(Some(selection.clone()), Some(factory));
        grid_view.set_max_columns(16);
        grid_view.set_enable_rubberband(true);
        grid_view.set_vexpand(true);

        // Enter / double-click opens the viewer at that image.
        grid_view.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, position| window.open_viewer(position)
        ));

        // Delete trashes the selection; Space quick-previews the first selected.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| match key {
                gtk::gdk::Key::Delete => {
                    window.trash_selected();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::space | gtk::gdk::Key::KP_Space => {
                    window.preview_selected();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        ));
        grid_view.add_controller(keys);

        imp.grid_scroller.set_child(Some(&grid_view));
        *imp.selection.borrow_mut() = Some(selection);
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
                        Err(err) => window.toast(&format!(
                            "Couldn’t move to trash: {}",
                            err.message()
                        )),
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
        viewer.open(imp.store.clone(), position);
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
            items.push(ImageObject::new(child, &display));
        }
    }

    items.sort_by(|a, b| {
        a.display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase())
    });
    Ok(items)
}
