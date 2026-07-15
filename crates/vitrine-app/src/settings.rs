//! Persistent app settings via a GLib key file.
//!
//! Deliberately *not* GSettings: a GSettings schema must be compiled and
//! installed on `XDG_DATA_DIRS` or the app aborts at first access — which would
//! break the plain `cargo run` dev path (and its many `VITRINE_*` hooks). A key
//! file at `$XDG_CONFIG_HOME/vitrine/settings.ini` behaves identically under
//! cargo, Meson, and Flatpak with nothing to install.
//!
//! Two things live here: the **library roots** (folders indexed in the
//! background regardless of what you browse) and the **thumbnail cache budget**.

use std::path::{Path, PathBuf};

use gtk::glib;

const GROUP_ROOTS: &str = "Roots";
const KEY_COUNT: &str = "count";
const GROUP_CACHE: &str = "Cache";
const KEY_CACHE_MB: &str = "thumbnail-mb";
const GROUP_SORT: &str = "Sort";
const KEY_SORT_FIELD: &str = "field";
const KEY_SORT_DESC: &str = "descending";

/// Default thumbnail-cache budget (MB) — matches the historical prune default.
pub const DEFAULT_CACHE_MB: u64 = 2048;

fn config_path() -> PathBuf {
    glib::user_config_dir().join("vitrine").join("settings.ini")
}

/// A loaded settings file. Cheap to construct; load fresh where you need it.
pub struct Settings {
    path: PathBuf,
    key_file: glib::KeyFile,
}

impl Settings {
    /// Load settings, or an empty set if the file doesn't exist yet.
    pub fn load() -> Settings {
        let path = config_path();
        let key_file = glib::KeyFile::new();
        // A missing/!unparseable file just means "no settings yet".
        let _ = key_file.load_from_file(&path, glib::KeyFileFlags::NONE);
        Settings { path, key_file }
    }

    /// The configured library roots (folders to index in the background).
    pub fn roots(&self) -> Vec<PathBuf> {
        let count = self.key_file.uint64(GROUP_ROOTS, KEY_COUNT).unwrap_or(0);
        (0..count)
            .filter_map(|i| self.key_file.string(GROUP_ROOTS, &i.to_string()).ok())
            .map(|s| PathBuf::from(s.as_str()))
            .collect()
    }

    /// Replace the whole roots list (rewrites the numbered keys + count; any
    /// higher-numbered stale keys are ignored because reads stop at `count`).
    pub fn set_roots(&self, roots: &[PathBuf]) {
        for (i, root) in roots.iter().enumerate() {
            if let Some(s) = root.to_str() {
                self.key_file.set_string(GROUP_ROOTS, &i.to_string(), s);
            }
        }
        self.key_file
            .set_uint64(GROUP_ROOTS, KEY_COUNT, roots.len() as u64);
        self.save();
    }

    /// Add `root` if not already present. Returns whether it was added.
    pub fn add_root(&self, root: &Path) -> bool {
        let mut roots = self.roots();
        if roots.iter().any(|r| r == root) {
            return false;
        }
        roots.push(root.to_path_buf());
        self.set_roots(&roots);
        true
    }

    /// Remove `root` from the library.
    pub fn remove_root(&self, root: &Path) {
        let roots: Vec<PathBuf> = self.roots().into_iter().filter(|r| r != root).collect();
        self.set_roots(&roots);
    }

    /// Thumbnail-cache budget in MB (falls back to [`DEFAULT_CACHE_MB`]).
    pub fn cache_mb(&self) -> u64 {
        match self.key_file.uint64(GROUP_CACHE, KEY_CACHE_MB) {
            Ok(v) if v > 0 => v,
            _ => DEFAULT_CACHE_MB,
        }
    }

    /// Set the thumbnail-cache budget in MB.
    pub fn set_cache_mb(&self, mb: u64) {
        self.key_file.set_uint64(GROUP_CACHE, KEY_CACHE_MB, mb);
        self.save();
    }

    /// The remembered grid sort field id ("name"/"size"/"modified"/"type").
    pub fn sort_field(&self) -> String {
        self.key_file
            .string(GROUP_SORT, KEY_SORT_FIELD)
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|_| "name".to_string())
    }

    pub fn set_sort_field(&self, field: &str) {
        self.key_file.set_string(GROUP_SORT, KEY_SORT_FIELD, field);
        self.save();
    }

    /// Whether the grid sort is descending (default ascending).
    pub fn sort_descending(&self) -> bool {
        self.key_file
            .boolean(GROUP_SORT, KEY_SORT_DESC)
            .unwrap_or(false)
    }

    pub fn set_sort_descending(&self, descending: bool) {
        self.key_file
            .set_boolean(GROUP_SORT, KEY_SORT_DESC, descending);
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = self.key_file.save_to_file(&self.path) {
            glib::g_warning!("vitrine", "save settings: {e}");
        }
    }
}
