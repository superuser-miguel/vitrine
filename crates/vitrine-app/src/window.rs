//! The main application window: the browser grid and folder-open flow.
//!
//! Phase 1 browser. Opening a folder enumerates its images asynchronously into
//! a `gio::ListStore` of [`ImageObject`]s, shown in a virtualized `GtkGridView`
//! with `GtkMultiSelection` (rubber-band + Ctrl/Shift ranges). Thumbnails decode
//! lazily per visible cell (see [`crate::grid_cell`]). The single-image viewer
//! and filmstrip arrive in the next increment.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use crate::grid_cell::VitrineGridCell;
use crate::image_object::ImageObject;

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

        /// Backing model for the grid (one row per image file).
        pub store: gio::ListStore,
        /// Selection model the grid renders.
        pub selection: RefCell<Option<gtk::MultiSelection>>,
    }

    impl Default for VitrineWindow {
        fn default() -> Self {
            Self {
                content_stack: Default::default(),
                grid_scroller: Default::default(),
                open_button: Default::default(),
                places_list: Default::default(),
                store: gio::ListStore::new::<ImageObject>(),
                selection: RefCell::new(None),
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

        imp.grid_scroller.set_child(Some(&grid_view));
        *imp.selection.borrow_mut() = Some(selection);
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
