//! The Preferences dialog — the last piece of Phase 2.
//!
//! Two settings, backed by [`crate::settings`]: the **library roots** (folders
//! indexed in the background so search/sort cover them without browsing) and the
//! **thumbnail-cache budget**. Built in code (not Blueprint) because the roots
//! group is dynamic — one removable row per folder, grown by a portal chooser.

use std::path::Path;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::settings::Settings;
use crate::window::VitrineWindow;

/// Present the Preferences dialog over `window`.
pub fn present(window: &VitrineWindow) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title(&gettextrs::gettext("Preferences"));

    let page = adw::PreferencesPage::builder()
        .title(gettextrs::gettext("Library"))
        .icon_name("view-grid-symbolic")
        .build();

    page.add(&roots_group(window));
    page.add(&cache_group(&dialog));
    dialog.add(&page);
    dialog.present(Some(window));
}

/// The "Library Folders" group: an add-folder button in the header and one
/// removable row per configured root.
fn roots_group(window: &VitrineWindow) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettextrs::gettext("Library Folders"))
        .description(gettextrs::gettext(
            "Folders indexed in the background, so search and sort cover them \
             even before you open them.",
        ))
        .build();

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text(gettextrs::gettext("Add Folder"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    add.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        group,
        move |_| add_folder(&window, &group)
    ));
    group.set_header_suffix(Some(&add));

    for root in Settings::load().roots() {
        add_root_row(&group, &root);
    }
    group
}

/// Portal-choose a folder, add it to the library, index it, and show its row.
fn add_folder(window: &VitrineWindow, group: &adw::PreferencesGroup) {
    let dialog = gtk::FileDialog::builder()
        .title(gettextrs::gettext("Add Library Folder"))
        .modal(true)
        .build();
    dialog.select_folder(
        Some(window),
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            window,
            #[weak]
            group,
            move |result| {
                let Ok(folder) = result else { return };
                let Some(path) = folder.path() else { return };
                if Settings::load().add_root(&path) {
                    add_root_row(&group, &path);
                    window.index_root(path);
                }
            }
        ),
    );
}

/// One library-folder row: basename as title, full path as subtitle, with a
/// remove button that drops it from settings and the list.
fn add_root_row(group: &adw::PreferencesGroup, path: &Path) {
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(path.to_string_lossy())
        .subtitle_selectable(true)
        .build();

    let remove = gtk::Button::builder()
        .icon_name("edit-delete-symbolic")
        .tooltip_text(gettextrs::gettext("Remove from Library"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let path = path.to_path_buf();
    remove.connect_clicked(glib::clone!(
        #[weak]
        group,
        #[weak]
        row,
        move |_| {
            Settings::load().remove_root(&path);
            group.remove(&row);
        }
    ));
    row.add_suffix(&remove);
    group.add(&row);
}

/// The "Cache" group: a spin row for the thumbnail-cache budget. The value is
/// persisted as it changes; the cache is re-pruned once, when the dialog closes.
fn cache_group(dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettextrs::gettext("Cache"))
        .build();

    let mb = Settings::load().cache_mb();
    let adjustment = gtk::Adjustment::new(mb as f64, 128.0, 65536.0, 128.0, 512.0, 0.0);
    let row = adw::SpinRow::builder()
        .title(gettextrs::gettext("Thumbnail Cache"))
        .subtitle(gettextrs::gettext("Maximum size on disk (MB)"))
        .adjustment(&adjustment)
        .build();
    adjustment.connect_value_changed(|adj| {
        Settings::load().set_cache_mb(adj.value() as u64);
    });
    dialog.connect_closed(|_| crate::thumbnails::prune_private_cache());

    group.add(&row);
    group
}
