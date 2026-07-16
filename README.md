<p align="center">
  <img src="data/icons/hicolor/scalable/apps/io.github.superuser_miguel.Vitrine.svg" alt="Vitrine icon" width="96" height="96">
</p>

<h1 align="center">Vitrine</h1>

<p align="center"><strong>Browse images that happen to be files — with a review layer that survives every rename.</strong></p>

Vitrine is a fast, focused, **catalog-aware image browser + reviewer** for
GNOME: Loupe's viewer architecture + Nautilus's grid and selection model + a
catalog/tag layer keyed by content hash so ratings, tags, comments, and
collections survive gallery-dl renames and moves.

Rust · GTK4 · gtk-rs · libadwaita · Blueprint · glycin · SQLite · Flatpak.
See [`PLAN.md`](PLAN.md) for the phased build plan.

> **Status: v1 feature-complete.** Vitrine browses, views, indexes, reviews,
> organizes, and de-duplicates — all keyed to survive renames. Builds via cargo,
> Meson, and flatpak-builder; the engine ships 73 tests and stays UI-free.
> Distributed as a Flatpak bundle via GitHub Releases, with the project page on
> [GitHub Pages](https://superuser-miguel.github.io/vitrine/) — **not** Flathub.

## Features

**Browse**
- Virtualized `GtkGridView` — rubber-band / Ctrl / Shift selection, adjustable
  thumbnail size (Ctrl +/−, Ctrl+scroll), trash-to-recycle. Bounded RAM + disk
  caches keep memory flat on 27k-image folders; reuses GNOME's shared thumbnail
  cache when it can.
- **First-class AVIF / JXL / HEIF** (plus JPEG/PNG/WebP/…) via glycin — color-
  managed, EXIF-oriented, decoded in sandboxed subprocesses.
- **Sidebar** — a gThumb-style switcher between **Places** (Nautilus-style
  bookmarks: rename, reorder by drag, remove), a lazy **Folders** tree, and
  **Collections**. Back / Forward navigation history.
- **Nautilus-style sorting** — Name / Size / Modified / Type with an independent
  ascending/descending toggle; instant, live, remembered across sessions.

**View**
- Single-image viewer — fit / zoom / pan / 100%, arrow-key navigation, a synced
  filmstrip, and a **properties sidebar** (dimensions, size, format, date taken,
  camera, orientation).

**Review & organize**
- **Ratings** (0–5 stars, keyboard in the grid, star overlays on thumbnails),
  **comments**, and **tags** (apply to a whole selection, autocomplete).
- **Collections** — hand-curated **catalogs** (drag images in, reorder) and
  **smart collections** (a saved filter that updates itself).
- **Filter bar** — narrow the grid live by minimum rating or tag; save the
  filter as a smart collection.

**Find duplicates**
- **Exact** (byte-identical) and **near** (perceptual-hash) clustering, with a
  reclaimable-space readout and one-click "trash the extras, keep the largest".

**Under the hood**
- A background, app-private **SQLite index** keyed by **BLAKE3 content hash**, so
  tags / ratings / comments / collections **survive gallery-dl renames and
  moves**. Move/delete reconciliation; background EXIF + perceptual-hash
  enrichment. Browsing never waits on the index.
- Portable **backup / export** of all annotations (JSON, content-hash keyed).
- **XMP sidecar export** — write `photo.jpg.xmp` sidecars (`xmp:Rating` /
  `dc:description` / `dc:subject`) for the selection so digiKam, darktable,
  Lightroom, and XnView read Vitrine's ratings, comments, and tags.
  Non-destructive: originals are never rewritten.
- **Preferences** for library roots and the thumbnail-cache budget.

## Roadmap

v1 is done. Planned next (see [`PLAN.md`](PLAN.md) for full specs):

- **Navigation** — a Nautilus-style address bar and tabs (Back/Forward shipped).
- **Lua scripting** — custom sort orders, batch ImageMagick ops, rename rules.
- **A non-destructive edit tier** — crop / rotate / resize (Loupe/gThumb-style).
- **WASM compute plugins** — local auto-tagging and embedding-based "find
  similar", plus faces / OCR / quality scoring.
- **In-file XMP embed** — write the packet directly into JPEG/PNG containers, on
  top of the sidecar export that ships today.

## Install

**Not on Flathub** — Vitrine is distributed as a Flatpak **bundle via
[GitHub Releases](https://github.com/superuser-miguel/vitrine/releases)**, with
the project page on [GitHub Pages](https://superuser-miguel.github.io/vitrine/).
Grab a release `.flatpak` and `flatpak install` it, or build it locally (below).

## Layout

```
crates/vitrine-engine/   UI-free core: index, hashing, scanning, dedup, queries
crates/vitrine-app/      GTK4/libadwaita shell (binary: `vitrine`)
data/                    Blueprint UI, gresource, desktop/metainfo, icon
build-aux/               Meson→cargo bridge + local checks
po/                      gettext catalogs
tests/fixtures/images/   Tiny generated sample images (see below)
```

`vitrine-engine` has **zero** GTK/GLib dependencies — enforced by
`build-aux/checks.sh` and CI.

## Build & run

### Host (fast dev iteration)

Requires `gtk4-devel`, `libadwaita-devel`, `blueprint-compiler`, and a Rust
toolchain.

```sh
# Plain cargo: build.rs compiles Blueprints + bundles the gresource into OUT_DIR.
cargo run -p vitrine-app

# Full checks (fmt, clippy, tests, engine-purity gate):
./build-aux/checks.sh
```

### Meson

```sh
meson setup builddir -Dprofile=development
meson compile -C builddir
meson test -C builddir          # desktop + metainfo validation
meson install -C builddir --destdir /tmp/vitrine-prefix
```

### Flatpak

```sh
flatpak-builder --user --install --force-clean build-dir \
  build-aux/io.github.superuser_miguel.Vitrine.yml
flatpak run io.github.superuser_miguel.Vitrine
```

The manifest currently allows build-time network for local iteration. For a
reproducible **release bundle** (`flatpak build-bundle`) to publish on GitHub
Releases, vendor the crate graph
(`python3 flatpak-cargo-generator.py Cargo.lock -o build-aux/cargo-sources.json`),
add it to the app module, remove the `--share=network` **build-arg**, and build
with `-Doffline=true`.

## Test fixtures

`tests/fixtures/images/` holds tiny generated sample images used by engine
tests. Regenerate with:

```sh
python3 tests/fixtures/generate.py
```
