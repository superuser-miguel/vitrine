//! Add-on helpers (PLAN §16.1 decision 3): external tools shipped as Flatpak
//! extensions of Vitrine through `vitrine-repo`, each mounted by the
//! `io.github.superuser_miguel.Vitrine.Helper` extension point at
//! `/app/helpers/<name>/`. Core only detects them: a helper that isn't
//! installed means its feature never appears — no error, no dialog.
//!
//! The first helper is ImageMagick (`…Vitrine.Helper.magick`, see
//! `build-aux/helpers/magick/`), for the colour tools. Nothing uses it yet;
//! detection lands first so the extension point ships and can be built
//! against (groundwork G2).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where the extension point mounts helpers (`directory: helpers` in the
/// manifest, relative to `/app`).
pub const HELPERS_ROOT: &str = "/app/helpers";

/// The helpers found at launch.
#[derive(Debug, Default)]
pub struct Helpers {
    /// The `magick` binary, if the ImageMagick helper is installed.
    pub magick: Option<PathBuf>,
}

/// The helpers installed for this run, detected once. Flatpak mounts
/// extensions at startup, so the answer can't change while the app runs.
pub fn get() -> &'static Helpers {
    static HELPERS: OnceLock<Helpers> = OnceLock::new();
    HELPERS.get_or_init(|| detect(Path::new(HELPERS_ROOT)))
}

/// Look for each known helper under `root`.
pub fn detect(root: &Path) -> Helpers {
    Helpers {
        magick: executable(&root.join("magick/bin/magick")),
    }
}

/// `path` if it is a regular file someone may execute.
fn executable(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn magick_is_found_only_when_installed_and_executable() {
        let root = std::env::temp_dir().join(format!("vitrine-helpers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // No helpers directory at all: nothing found.
        assert!(detect(&root).magick.is_none());

        // Present but not executable: not a usable helper.
        let bin = root.join("magick/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let magick = bin.join("magick");
        std::fs::write(&magick, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&magick, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(detect(&root).magick.is_none());

        // Executable: found, at its full path.
        std::fs::set_permissions(&magick, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(detect(&root).magick.as_deref(), Some(magick.as_path()));

        // A directory with the right name is not a binary.
        std::fs::remove_file(&magick).unwrap();
        std::fs::create_dir(&magick).unwrap();
        assert!(detect(&root).magick.is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
