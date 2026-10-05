//! V-29 guard — keep the main window narrow enough to tile.
//!
//! A compositor will not tile a window whose *minimum* width exceeds the
//! half-tile it is being asked to occupy; it silently refuses, so Super+←/→ and
//! drag-to-edge appear to do nothing while fullscreen still works (fullscreen
//! ignores minimum sizes). On a 1920px screen the half-tile is 960px.
//!
//! That is exactly how V-29 shipped. `adb3409` added a `width-request: 240` to
//! the filter bar's search entry — four innocent lines — which pushed the
//! window's minimum from 776px to 1030px, 70px past the limit. Two properties
//! of GTK made it invisible:
//!
//!   * `GtkRevealer`'s slide-down transition collapses **height** only, so the
//!     hidden filter bar still imposes its full width on the window; and
//!   * `AdwNavigationSplitView` never collapses without an `AdwBreakpoint`, so
//!     the sidebar's minimum is permanently added on top.
//!
//! Neither is visible in a diff, and the symptom only appears after the app is
//! restarted — a running window keeps the constraints of the binary it started
//! from. These checks are cheap and static so they fail in review instead.
//!
//! Limits: this reads declared properties, so it catches a hard floor written
//! into the markup — the shape that actually bit us — but not a widget whose
//! *natural* minimum is large on its own (a long button label, say). For the
//! true figure, measure the built widget tree; the numbers above came from
//! `Gtk.Widget.measure()` against a faithful reconstruction of this file.

use std::path::PathBuf;

/// Total `width-request` the filter bar may declare, in px.
///
/// Derived, not guessed. Above the 800sp breakpoint the sidebar stays expanded,
/// so laying out at a 960px half-tile costs `min-sidebar-width` (180) plus the
/// content's own minimum (~596 for this toolbar) = ~776px. That leaves ~184px
/// of headroom before the half-tile is overrun again; 120 keeps a margin inside
/// it. `adb3409` asked for 240 in one widget, which this rejects.
///
/// Re-measure before raising it: the true figure is `Gtk.Widget.measure()` on
/// the built tree, and the shipped window reports it over the Wayland protocol
/// as `xdg_toplevel.set_min_size` (visible under `WAYLAND_DEBUG=1`).
const FILTER_BAR_WIDTH_BUDGET: u32 = 120;

fn window_blp() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/ui/window.blp")
        .canonicalize()
        .expect("data/ui/window.blp should resolve from the app crate");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The `{ … }` block introduced by `opener`, brace-matched.
fn block_after<'a>(src: &'a str, opener: &str) -> &'a str {
    let start = src
        .find(opener)
        .unwrap_or_else(|| panic!("window.blp no longer contains `{opener}`"));
    let rest = &src[start..];
    let open = rest.find('{').expect("block should have an opening brace");
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[open..=open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after `{opener}`");
}

/// Every `width-request: N;` declared in `block`.
fn width_requests(block: &str) -> Vec<u32> {
    block
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // Skip comments so the explanatory notes above each fix don't count.
            if line.starts_with("//") {
                return None;
            }
            let value = line.strip_prefix("width-request:")?;
            value.trim().trim_end_matches(';').trim().parse().ok()
        })
        .collect()
}

#[test]
fn split_view_collapses_at_a_breakpoint() {
    let blp = window_blp();
    assert!(
        blp.contains("Adw.Breakpoint"),
        "window.blp declares no Adw.Breakpoint, so the NavigationSplitView can \
         never collapse and the window's minimum width is permanently \
         sidebar + content. That is what stopped Vitrine tiling (V-29)."
    );
    assert!(
        blp.contains("split_view.collapsed: true"),
        "an Adw.Breakpoint exists but nothing sets `split_view.collapsed`, so \
         the sidebar still pins the window's minimum width open (V-29)."
    );
}

#[test]
fn filter_bar_declares_no_hard_width_floor() {
    let blp = window_blp();
    let bar = block_after(&blp, "Gtk.Revealer filter_revealer");
    let requests = width_requests(bar);
    let total: u32 = requests.iter().sum();

    assert!(
        total <= FILTER_BAR_WIDTH_BUDGET,
        "the filter bar declares {total}px of width-request ({requests:?}), over \
         its {FILTER_BAR_WIDTH_BUDGET}px budget.\n\n\
         This row sits inside a GtkRevealer whose slide-down transition collapses \
         height only, so anything fixed here is a hard floor on the *window's* \
         minimum width even while the filter bar is hidden — and a window that \
         cannot shrink to half the screen cannot be tiled at all (V-29).\n\n\
         Use `hexpand: true` to claim space when it is there, rather than a \
         width-request that demands it always."
    );
}
