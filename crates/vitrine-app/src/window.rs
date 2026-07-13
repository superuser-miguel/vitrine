//! The main application window.
//!
//! Phase 0: a composite-template `AdwApplicationWindow` with a headerbar, a
//! primary menu, and an `AdwStatusPage` placeholder. Phase 1 replaces the
//! placeholder with the browser grid ↔ viewer ↔ filmstrip navigation.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/github/superuser_miguel/Vitrine/window.ui")]
    pub struct VitrineWindow {}

    #[glib::object_subclass]
    impl ObjectSubclass for VitrineWindow {
        const NAME: &'static str = "VitrineWindow";
        type Type = super::VitrineWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
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
                // libadwaita renders the striped "devel" header for dev builds.
                self.obj().add_css_class("devel");
            }
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
}
