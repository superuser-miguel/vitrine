# Vitrine

A fast, focused, **catalog-aware image browser + reviewer** for GNOME.

Rust · GTK4 · gtk-rs · libadwaita · Blueprint · glycin · SQLite · Flatpak.

> Browse *images that happen to be files*: Loupe's viewer architecture +
> Nautilus's grid/selection model + a catalog/tag layer keyed to survive
> gallery-dl renames. See [`PLAN.md`](PLAN.md) for the phased build plan.

## Status

**Phases 0–2 complete.** Vitrine is a working image browser + reviewer with a
content-hash-keyed catalog index. Builds via cargo, Meson, and flatpak-builder.

**What works today:**

- **Browser grid** — virtualized `GtkGridView` with rubber-band/Ctrl/Shift
  selection, adjustable thumbnail size (Ctrl +/−, Ctrl+scroll), and
  trash-to-recycle (`Delete`). Bounded RAM + disk thumbnail caches keep memory
  flat on 27k-image folders; thumbnails reuse GNOME's shared cache when possible.
- **Viewer** — fit / zoom / pan / 100%, arrow-key navigation, a synced
  filmstrip, and an **image properties sidebar** (dimensions, size, format, date
  taken, camera, orientation).
- **Nautilus-style sorting** — sort by Name / Size / Modified / Type with an
  independent Ascending/Descending toggle; instant and live (no rebuild), your
  choice remembered across sessions.
- **First-class AVIF / JXL / HEIF** (plus JPEG/PNG/WebP/…) via glycin.
- **Background index** — an app-private SQLite catalog, BLAKE3 content-hash keyed
  so tags/ratings survive gallery-dl renames, with move/delete reconciliation and
  background enrichment (dimensions, EXIF, perceptual hash). Browsing never waits
  on it.
- **Preferences** — manage **library folders** (indexed in the background) and
  the thumbnail-cache budget.

**Next:** Phase 3 (tags, stars, Collections) and Phase 4 (find-duplicates UI —
the engine primitives, content-hash grouping + perceptual-hash distance, already
exist). See [`PLAN.md`](PLAN.md).

### Planned: scripting

A v2 Lua/Rhai scripting tier is planned (see PLAN.md §10.3). Its likely first
use case is **user-defined custom sort orders** (§10.3.1) — write a small `key`
function in Lua (natural filename order, aspect ratio, camera-then-date, rating,
…) and it shows up alongside the built-in sorts.

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

The manifest currently allows build-time network for local iteration. Before a
Flathub submission, vendor the crate graph
(`python3 flatpak-cargo-generator.py Cargo.lock -o build-aux/cargo-sources.json`),
add it to the app module, remove the `--share=network` **build-arg**, and build
with `-Doffline=true`.

## Test fixtures

`tests/fixtures/images/` holds tiny generated sample images used by engine
tests. Regenerate with:

```sh
python3 tests/fixtures/generate.py
```
