//! Headless verification of the glycin decode path (PLAN Phase 1 acceptance).
//!
//! Decodes every file in a directory at thumbnail resolution via glycin — the
//! exact call the grid cells make — and prints a per-file report. Proves AVIF /
//! JXL / HEIF are first-class and that corrupt files fail gracefully rather than
//! crash. No window; runs on the GLib main context.
//!
//!   cargo run -p vitrine-app --example decode_probe -- ~/Pictures/vitrine-smoke

use gtk::prelude::*;
use gtk::{gio, glib};

use glycin::{FrameRequest, Loader};

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: decode_probe <directory>");

    let ctx = glib::MainContext::default();
    ctx.block_on(async move {
        let folder = gio::File::for_path(&dir);
        let enumerator = folder
            .enumerate_children_future(
                "standard::name,standard::content-type,standard::type",
                gio::FileQueryInfoFlags::NONE,
                glib::Priority::DEFAULT,
            )
            .await
            .expect("enumerate");

        let mut names: Vec<(gio::File, String)> = Vec::new();
        while let Ok(infos) = enumerator
            .next_files_future(64, glib::Priority::DEFAULT)
            .await
        {
            if infos.is_empty() {
                break;
            }
            for info in infos {
                names.push((enumerator.child(&info), info.name().display().to_string()));
            }
        }
        names.sort_by(|a, b| a.1.cmp(&b.1));

        let (mut ok, mut err) = (0, 0);
        for (file, name) in names {
            match Loader::new(file).load().await {
                Ok(image) => match image
                    .specific_frame(FrameRequest::new().scale(256, 256))
                    .await
                {
                    Ok(frame) => {
                        let t = frame.texture();
                        ok += 1;
                        println!("  OK   {name:<20} -> {}x{}", t.width(), t.height());
                    }
                    Err(e) => {
                        err += 1;
                        println!("  ERR  {name:<20} -> frame: {e}");
                    }
                },
                Err(e) => {
                    err += 1;
                    println!("  ERR  {name:<20} -> load: {e}");
                }
            }
        }
        println!("\n{ok} decoded, {err} failed");
    });
}
