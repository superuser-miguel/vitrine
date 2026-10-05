//! The Preferences dialog — the last piece of Phase 2.
//!
//! Two settings, backed by [`crate::settings`]: the **library roots** (folders
//! indexed in the background so search/sort cover them without browsing) and the
//! **thumbnail-cache budget** — plus the **Clean Up Thumbnails…** dialog. Built
//! in code (not Blueprint) because the roots group is dynamic — one removable
//! row per folder, grown by a portal chooser.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use vitrine_engine::thumb_cleanup::AgeRule;

use crate::settings::Settings;
use crate::thumbnails::{CleanupPlan, Survey, Tier};
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
        .subtitle(gettextrs::gettext(
            "Maximum size of Vitrine’s own cache (MB)",
        ))
        .adjustment(&adjustment)
        .build();
    adjustment.connect_value_changed(|adj| {
        Settings::load().set_cache_mb(adj.value() as u64);
    });
    dialog.connect_closed(|_| {
        crate::thumbnails::prune_private_cache();
        crate::thumbnails::prune_removable_cache();
        crate::thumbnails::prune_content_cache();
    });

    group.add(&row);
    group.add(&cleanup_row(dialog));
    group
}

/// The row that opens the cleanup dialog.
fn cleanup_row(prefs: &adw::PreferencesDialog) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(gettextrs::gettext("Clean Up Thumbnails…"))
        .subtitle(gettextrs::gettext(
            "Remove thumbnails you haven’t used, or whose image is gone",
        ))
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row.connect_activated(glib::clone!(
        #[weak]
        prefs,
        move |_| present_cleanup(&prefs)
    ));
    row
}

// ---- Clean Up Thumbnails… ------------------------------------------------------
// Sizes first, then the two options, a live preview of what they'd remove, and
// a confirm step in the dialog itself. Every number comes from a worker thread
// (see `thumbnails::survey` and friends); the main thread only shows them.

/// "Remove thumbnails not used in" choices, in the combo's order.
const AGE_CHOICES: [AgeRule; 6] = [
    AgeRule::Off,
    AgeRule::Days(30),
    AgeRule::Days(60),
    AgeRule::Days(90),
    AgeRule::Days(180),
    AgeRule::All,
];
/// The combo's starting choice: 90 days.
const DEFAULT_AGE_CHOICE: u32 = 3;

/// The dialog's working state, shared by its handlers.
#[derive(Default)]
struct Cleanup {
    /// Every tier, listed once on open.
    survey: RefCell<Option<Arc<Survey>>>,
    /// Missing-source verdicts, computed the first time that option is on.
    gone: RefCell<Option<Arc<HashSet<PathBuf>>>>,
    checking_gone: Cell<bool>,
    /// The current preview, and which refresh it belongs to (stale results
    /// from an earlier option are dropped).
    plan: RefCell<Option<CleanupPlan>>,
    generation: Cell<u64>,
}

/// Format `(files, bytes)` as "1,234 thumbnails · 56.7 MB".
fn describe(count: usize, bytes: u64) -> String {
    let files = if count == 1 {
        gettextrs::gettext("1 thumbnail")
    } else {
        format!("{} {}", count, gettextrs::gettext("thumbnails"))
    };
    format!("{files} · {}", glib::format_size(bytes))
}

/// Present the cleanup dialog over Preferences.
fn present_cleanup(prefs: &adw::PreferencesDialog) {
    let dialog = adw::Dialog::builder()
        .title(gettextrs::gettext("Clean Up Thumbnails"))
        .content_width(480)
        .build();
    let page = adw::PreferencesPage::new();

    // What each tier holds now.
    let sizes = adw::PreferencesGroup::builder()
        .title(gettextrs::gettext("On Disk"))
        .build();
    let mut size_labels: Vec<(Tier, gtk::Label)> = Vec::new();
    for (tier, title, subtitle) in [
        (
            Tier::Private,
            gettextrs::gettext("Vitrine"),
            gettextrs::gettext("Large thumbnails only Vitrine uses"),
        ),
        (
            Tier::Removable,
            gettextrs::gettext("Removable Drives"),
            gettextrs::gettext("Kept for USB and network drives"),
        ),
        (
            Tier::Content,
            gettextrs::gettext("By Content"),
            gettextrs::gettext("Shared by identical images"),
        ),
        (
            Tier::Shared,
            gettextrs::gettext("GNOME (shared)"),
            gettextrs::gettext("Shared with Files — only cleaned of missing images"),
        ),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        let label = gtk::Label::builder()
            .label("…")
            .css_classes(["dim-label", "numeric"])
            .build();
        row.add_suffix(&label);
        sizes.add(&row);
        size_labels.push((tier, label));
    }

    // The options.
    let options = adw::PreferencesGroup::builder()
        .title(gettextrs::gettext("Remove"))
        .build();
    let choices = gtk::StringList::new(&[]);
    for rule in AGE_CHOICES {
        choices.append(&match rule {
            AgeRule::Off => gettextrs::gettext("Don’t remove by age"),
            AgeRule::Days(days) => format!("{days} {}", gettextrs::gettext("days")),
            AgeRule::All => gettextrs::gettext("All"),
        });
    }
    let age = adw::ComboRow::builder()
        .title(gettextrs::gettext("Remove thumbnails not used in"))
        .subtitle(gettextrs::gettext("Vitrine’s own caches only"))
        .model(&choices)
        .selected(DEFAULT_AGE_CHOICE)
        .build();
    let missing = adw::SwitchRow::builder()
        .title(gettextrs::gettext(
            "Remove thumbnails of files that no longer exist",
        ))
        .subtitle(gettextrs::gettext(
            "Includes GNOME’s shared cache. Files on unplugged drives are kept.",
        ))
        .build();
    let preview = adw::ActionRow::builder()
        .title(gettextrs::gettext("Will remove"))
        .subtitle(gettextrs::gettext("Calculating…"))
        .build();
    let spinner = gtk::Spinner::builder().spinning(true).build();
    preview.add_suffix(&spinner);
    options.add(&age);
    options.add(&missing);
    options.add(&preview);

    // The action, then the confirm step, then progress — one at a time.
    let clean = gtk::Button::builder()
        .label(gettextrs::gettext("Clean Up…"))
        .halign(gtk::Align::Center)
        .sensitive(false)
        .css_classes(["pill", "destructive-action"])
        .build();
    let question = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let cancel = gtk::Button::builder()
        .label(gettextrs::gettext("Cancel"))
        .css_classes(["pill"])
        .build();
    let remove = gtk::Button::builder()
        .label(gettextrs::gettext("Remove"))
        .css_classes(["pill", "destructive-action"])
        .build();
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    buttons.append(&cancel);
    buttons.append(&remove);
    let confirm = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    confirm.append(&question);
    confirm.append(&buttons);
    let running = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    running.append(&gtk::Spinner::builder().spinning(true).build());
    running.append(&gtk::Label::new(Some(&gettextrs::gettext("Removing…"))));
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vhomogeneous(false)
        .build();
    stack.add_named(&clean, Some("ask"));
    stack.add_named(&confirm, Some("confirm"));
    stack.add_named(&running, Some("running"));
    let actions = adw::PreferencesGroup::new();
    actions.add(&stack);

    page.add(&sizes);
    page.add(&options);
    page.add(&actions);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));

    let state = Rc::new(Cleanup::default());

    // Recompute the preview for the current options. The heavy inputs (the
    // survey, the missing-source check) are each computed once, off-thread;
    // the plan over them is computed off-thread too.
    let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[strong]
        state,
        #[weak]
        age,
        #[weak]
        missing,
        #[weak]
        preview,
        #[weak]
        spinner,
        #[weak]
        clean,
        #[weak]
        stack,
        move || {
            let generation = state.generation.get() + 1;
            state.generation.set(generation);
            state.plan.replace(None);
            stack.set_visible_child_name("ask");
            clean.set_sensitive(false);
            spinner.set_visible(true);

            let Some(survey) = state.survey.borrow().clone() else {
                preview.set_subtitle(&gettextrs::gettext("Calculating…"));
                return;
            };
            let gone = if missing.is_active() {
                match state.gone.borrow().clone() {
                    Some(gone) => Some(gone),
                    None => {
                        preview.set_subtitle(&format!(
                            "{} {} {}",
                            gettextrs::gettext("Checking"),
                            survey.uri_keyed_count(),
                            gettextrs::gettext("thumbnails for missing files…"),
                        ));
                        return; // the check's completion calls refresh again
                    }
                }
            } else {
                None
            };
            let rule = AGE_CHOICES
                .get(age.selected() as usize)
                .copied()
                .unwrap_or(AgeRule::Off);
            preview.set_subtitle(&gettextrs::gettext("Calculating…"));
            glib::spawn_future_local(glib::clone!(
                #[strong]
                state,
                #[weak]
                preview,
                #[weak]
                spinner,
                #[weak]
                clean,
                async move {
                    let plan = gio::spawn_blocking(move || {
                        crate::thumbnails::plan_cleanup(&survey, rule, gone.as_deref())
                    })
                    .await
                    .unwrap_or_default();
                    if state.generation.get() != generation {
                        return; // the options changed meanwhile
                    }
                    spinner.set_visible(false);
                    preview.set_subtitle(&if plan.items.is_empty() {
                        gettextrs::gettext("Nothing to remove")
                    } else {
                        describe(plan.items.len(), plan.bytes)
                    });
                    clean.set_sensitive(!plan.items.is_empty());
                    state.plan.replace(Some(plan));
                }
            ));
        }
    ));

    // Start the missing-source check the first time it's wanted.
    let check_gone: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[strong]
        state,
        #[strong]
        refresh,
        move || {
            if state.gone.borrow().is_some() || state.checking_gone.get() {
                return;
            }
            let Some(survey) = state.survey.borrow().clone() else {
                return; // the survey's completion comes back here
            };
            state.checking_gone.set(true);
            glib::spawn_future_local(glib::clone!(
                #[strong]
                state,
                #[strong]
                refresh,
                async move {
                    let gone = gio::spawn_blocking(move || {
                        let paths = survey
                            .thumbs
                            .iter()
                            .filter(|t| t.tier != Tier::Content)
                            .map(|t| t.path.clone())
                            .collect();
                        crate::thumbnails::find_gone(paths)
                    })
                    .await
                    .unwrap_or_default();
                    state.gone.replace(Some(Arc::new(gone)));
                    state.checking_gone.set(false);
                    refresh();
                }
            ));
        }
    ));

    age.connect_selected_notify(glib::clone!(
        #[strong]
        refresh,
        move |_| refresh()
    ));
    missing.connect_active_notify(glib::clone!(
        #[strong]
        refresh,
        #[strong]
        check_gone,
        move |row| {
            if row.is_active() {
                check_gone();
            }
            refresh();
        }
    ));

    clean.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        question,
        #[weak]
        stack,
        move |_| {
            let Some((count, bytes)) = state
                .plan
                .borrow()
                .as_ref()
                .map(|p| (p.items.len(), p.bytes))
            else {
                return;
            };
            question.set_label(&format!(
                "{} {}? {}",
                gettextrs::gettext("Remove"),
                describe(count, bytes),
                gettextrs::gettext("This can’t be undone."),
            ));
            stack.set_visible_child_name("confirm");
        }
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        stack,
        move |_| stack.set_visible_child_name("ask")
    ));
    remove.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        stack,
        #[weak]
        age,
        #[weak]
        missing,
        #[weak]
        dialog,
        #[weak]
        prefs,
        move |_| {
            let Some(plan) = state.plan.take() else {
                return;
            };
            stack.set_visible_child_name("running");
            age.set_sensitive(false);
            missing.set_sensitive(false);
            glib::spawn_future_local(glib::clone!(
                #[weak]
                dialog,
                #[weak]
                prefs,
                async move {
                    let (count, bytes) =
                        gio::spawn_blocking(move || crate::thumbnails::run_cleanup(plan))
                            .await
                            .unwrap_or_default();
                    prefs.add_toast(adw::Toast::new(&if count == 0 {
                        gettextrs::gettext("No thumbnails were removed")
                    } else {
                        format!(
                            "{} {}",
                            gettextrs::gettext("Removed"),
                            describe(count, bytes)
                        )
                    }));
                    dialog.close();
                }
            ));
        }
    ));

    // List the tiers, then fill in the sizes and the first preview.
    glib::spawn_future_local(glib::clone!(
        #[strong]
        state,
        #[strong]
        refresh,
        #[strong]
        check_gone,
        #[weak]
        missing,
        async move {
            let survey = gio::spawn_blocking(crate::thumbnails::survey)
                .await
                .unwrap_or_default();
            for (tier, label) in &size_labels {
                let (count, bytes) = survey.usage(*tier);
                label.set_label(&describe(count, bytes));
            }
            state.survey.replace(Some(Arc::new(survey)));
            if missing.is_active() {
                check_gone();
            }
            refresh();
        }
    ));

    dialog.present(Some(prefs));
}
