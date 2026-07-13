# Vitrine

A fast, focused, **catalog-aware image browser + reviewer** for GNOME.

Rust · GTK4 · gtk-rs · libadwaita · Blueprint · glycin · SQLite · Flatpak.

> Browse *images that happen to be files*: Loupe's viewer architecture +
> Nautilus's grid/selection model + a catalog/tag layer keyed to survive
> gallery-dl renames. See [`PLAN.md`](PLAN.md) for the phased build plan.

## Status

Phase 0 (scaffold): empty-but-real application — builds via cargo, Meson, and
flatpak-builder; opens an `AdwApplicationWindow` with a headerbar and About
dialog. The browser grid, viewer, filmstrip, index and dedup layers land in
later phases.

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
