# PLAN.md — Vitrine

> **Working name: Vitrine.** If a different name is chosen, global-replace `Vitrine`/`vitrine`
> and the app ID before Phase 0. App ID convention: `io.github.superuser_miguel.Vitrine`
> (Flathub GitHub-namespace form; hyphens in the GitHub username become underscores).

A fast, focused, **catalog-aware image browser + reviewer** for GNOME.
Rust · GTK4 · gtk-rs · libadwaita · Blueprint · glycin · SQLite · Flatpak.

This plan is written to be executed phase-by-phase by Claude Code. Each phase has
explicit tasks, acceptance criteria, and tests. Do not start a phase until the previous
phase's acceptance criteria pass. Within a phase, keep commits small and topical.

---

## 0. Positioning (context, read once)

- **Loupe** is the viewer done right but has no grid, no gallery, no filmstrip.
- **Nautilus** has the virtualized grid and selection model but is a file manager, not an image tool.
- **gThumb 4** has the right ideas (catalogs, tags, browser→viewer→filmstrip) but is an alpha
  Vala rewrite; forking is a non-starter.

**Vitrine's position:** browse *images that happen to be files*. Loupe's viewer architecture +
Nautilus's grid/selection model + a catalog/tag layer keyed to survive gallery-dl renames.

**Non-goals (never build):** web albums, contact sheets, burn-to-CD, import wizards,
screensaver slideshows, hand-rolled Vulkan, in-process `.so` plugins, Tracker/TinySPARQL
dependency, gdk-pixbuf decoding.

---

## 1. House rules (apply to every phase)

These are the proven Septima/Ebb conventions. Treat them as hard constraints.

1. **Blueprint-only UI.** All UI in `.blp` files compiled by `blueprint-compiler` via Meson.
   No hand-written XML `.ui` files; no UI constructed in Rust code except where GTK requires
   it (e.g., list-item factories may bind in Rust, but their item templates live in Blueprint).
2. **Engine crate is UI-free.** `vitrine-engine` must have **zero** GTK/GLib/libadwaita
   dependencies. Enforced by CI check (see Phase 0). All parsing, hashing, indexing, dedup
   clustering, and query logic lives there. The app crate is a thin GTK shell over it.
3. **Never link C++.** Decode goes through glycin (sandboxed subprocess per image). The only
   sanctioned future exception is `rexiv2` on the v2 *write* path — not in scope for this plan.
4. **Portals-first.** Use `ashpd` for FileChooser and OpenURI. Static permissions limited to
   the documented manifest set (§4). Do not add `--filesystem=host` ever.
5. **Offline Flatpak build.** All crates vendored via `cargo-vendor` / flatpak-cargo-generator;
   `flatpak-builder` must succeed with networking disabled.
6. **Async discipline.** No blocking I/O or hashing on the main loop. Use `gio::spawn_blocking` /
   worker threads / `tokio` (pick one runtime in Phase 0 and stick to it — recommendation:
   no tokio; use `async-channel` + GLib main context + `rayon` for CPU work, matching the
   gtk-rs ecosystem).
7. **Every bug found empirically gets a fixture + regression test** (the Ebb rsync-parsing
   discipline, applied here to EXIF quirks, weird formats, and thumbnail-cache edge cases).

---

## 2. Repository layout

```
vitrine/
├── Cargo.toml                 # workspace
├── crates/
│   ├── vitrine-engine/        # UI-free: index, schema, scanner, hashing, dedup, queries
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── db.rs          # rusqlite wrapper, migrations
│   │   │   ├── schema.rs      # schema DDL + migration steps
│   │   │   ├── scanner.rs     # directory walk, change detection
│   │   │   ├── hash.rs        # blake3 + perceptual hash
│   │   │   ├── exif.rs        # metadata model (populated from glycin by the app)
│   │   │   ├── query.rs       # sort/filter/smart-collection predicates
│   │   │   └── dedup.rs       # exact + Hamming clustering
│   │   └── tests/             # pure-Rust tests, fixtures in tests/fixtures/
│   └── vitrine-app/           # GTK4/libadwaita shell
│       ├── src/
│       │   ├── main.rs
│       │   ├── application.rs
│       │   ├── window.rs
│       │   ├── grid_view.rs   # GtkGridView + selection model
│       │   ├── viewer.rs      # single-image view, zoom/pan
│       │   ├── filmstrip.rs   # GtkListView
│       │   ├── sidebar.rs     # places/collections sidebar
│       │   ├── metadata_panel.rs
│       │   ├── thumbnails.rs  # freedesktop cache + glycin pipeline
│       │   └── decode.rs      # glycin orchestration, LRU texture cache, prefetch
│       └── ui/                # *.blp only
├── data/
│   ├── io.github.superuser_miguel.Vitrine.desktop.in
│   ├── io.github.superuser_miguel.Vitrine.metainfo.xml.in
│   └── icons/
├── build-aux/
│   └── io.github.superuser_miguel.Vitrine.json   # Flatpak manifest
├── meson.build
├── meson_options.txt
└── tests/fixtures/images/     # tiny sample images (see §6)
```

---

## 3. Tech stack (settled — do not relitigate)

| Concern | Choice |
|---|---|
| UI | Rust, gtk-rs (gtk4 ≥ 4.14), libadwaita (≥ 1.5), Blueprint |
| Decode | glycin (`glycin` crate), async API |
| Rendering | GSK via `GtkPicture`/`GdkPaintable` — no manual GL/Vulkan |
| Grid / filmstrip | `GtkGridView` / `GtkListView`, `GtkMultiSelection` |
| Index | SQLite via `rusqlite` (bundled feature OFF in Flatpak; use SDK sqlite) |
| Exact hash | `blake3` |
| Perceptual hash | `image_hasher` (v3 line — the maintained fork of `img_hash`) |
| Parallel CPU work | `rayon` (engine only) |
| Portals | `ashpd` |
| Build | Meson + Cargo, `blueprint-compiler`, flatpak-builder |

---

## 4. Flatpak sandbox posture

Manifest permissions (initial, complete set):

```
--share=ipc
--socket=fallback-x11
--socket=wayland
--device=dri
--filesystem=xdg-pictures
--filesystem=xdg-cache/thumbnails:create
```

- `xdg-pictures` is the one deliberate static hole: a gallery needs persistent,
  re-scannable roots. Additional roots (e.g., the gallery-dl tree if it lives elsewhere)
  are granted at runtime via the FileChooser portal with persistence, and stored as
  document-portal paths in config.
- `xdg-cache/thumbnails:create` enables the **shared freedesktop thumbnail cache**.
  **RISK (verify in Phase 1):** confirm host-visible thumbnail paths resolve identically
  inside the sandbox (the cache keys are MD5 of the file **URI**, so the URI seen by the
  app must match the host URI for cache hits; document-portal paths will NOT match — for
  portal-granted roots, fall back to the app-private cache/glycin-thumbnailer path and
  note it). If sharing proves unreliable, drop to app-private cache without changing
  any other architecture.
- SQLite DB and config live in the app's own data dir (`~/.var/app/<id>/data/vitrine/`) —
  no permission needed.
- Everything else stays closed. No `--talk-name` additions without a documented reason.

---

## 5. Database schema (v1, with v2 seams baked in)

All tag/rating rows key on **`content_hash`** (BLAKE3, hex TEXT), *not* file path — tags must
survive gallery-dl renames/moves. Paths are reconciled by the scanner.

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- meta rows: schema_version, created_at, last_backup_at

CREATE TABLE files (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,        -- absolute, as seen by the app
  content_hash  TEXT NOT NULL,               -- BLAKE3 hex
  phash         INTEGER,                     -- 64-bit perceptual hash (NULL until computed)
  size          INTEGER NOT NULL,
  mtime         INTEGER NOT NULL,            -- unix seconds
  width         INTEGER, height INTEGER,
  format        TEXT,                        -- mime
  date_taken    INTEGER,                     -- EXIF DateTimeOriginal, unix seconds, NULL ok
  camera        TEXT,
  orientation   INTEGER,                     -- EXIF orientation 1-8
  indexed_at    INTEGER NOT NULL,
  missing       INTEGER NOT NULL DEFAULT 0   -- 1 = path vanished, kept for hash reconcile
);
CREATE INDEX idx_files_hash  ON files(content_hash);
CREATE INDEX idx_files_phash ON files(phash);
CREATE INDEX idx_files_date  ON files(date_taken);

CREATE TABLE tags (
  id    INTEGER PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE file_tags (
  content_hash TEXT NOT NULL,
  tag_id       INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  sync_state   TEXT NOT NULL DEFAULT 'db-only',  -- 'db-only' | 'synced' | 'dirty'  (v2 seam, dormant)
  source       TEXT NOT NULL DEFAULT 'manual',   -- 'manual' | 'rule'               (v2 seam, dormant)
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (content_hash, tag_id)
);

CREATE TABLE ratings (
  content_hash TEXT PRIMARY KEY,
  rating       INTEGER NOT NULL CHECK (rating BETWEEN 0 AND 5),  -- Xmp.xmp.Rating semantics
  sync_state   TEXT NOT NULL DEFAULT 'db-only',
  updated_at   INTEGER NOT NULL
);

CREATE TABLE collections (
  id     INTEGER PRIMARY KEY,
  name   TEXT NOT NULL,
  kind   TEXT NOT NULL CHECK (kind IN ('smart','catalog')),
  query  TEXT,          -- JSON predicate for smart collections, NULL for catalogs
  created_at INTEGER NOT NULL
);

CREATE TABLE collection_items (       -- catalogs only, ordered
  collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
  content_hash  TEXT NOT NULL,
  position      INTEGER NOT NULL,
  PRIMARY KEY (collection_id, content_hash)
);
```

Rules:
- Migrations are forward-only, numbered, applied by `vitrine-engine::db` at open.
- `sync_state` and `source` are **written with defaults and never read in v1**. Do not
  build UI for them. They exist so v2 metadata-write and the rule engine are additive.
- **Backup/export ships in v1** (Phase 2): `VACUUM INTO` a timestamped file + a JSON export
  of tags/ratings/collections keyed by content_hash.

---

## 6. Test fixtures

Create `tests/fixtures/images/` in Phase 0 with *tiny* generated images (keep repo < ~2 MB):

- `rgb_100x50.png`, `rgb_100x50.jpg` — same pixels, different bytes (near-dup pair).
- `rgb_100x50_copy.jpg` — byte-identical copy of the jpg (exact-dup pair).
- `rgb_100x50_scaled.jpg` — same image at 50x25 (near-dup, resize).
- `exif_dated.jpg` — carries DateTimeOriginal + Orientation=6 (generate with a small
  Rust build script or check in a hand-made one).
- `corrupt.jpg` — truncated file (decode-failure path).
- `not_an_image.txt` renamed to `fake.png` (mime sniff failure path).
- One each of `.webp` and `.avif` if the toolchain in CI can generate them; otherwise skip
  and note it.

Engine tests run against these with **no GTK and no glycin** (hashing and scanning read raw
bytes; EXIF fixtures are parsed by whatever pure-Rust reader the engine uses for tests, or
EXIF fields are injected by the test since production EXIF comes from glycin via the app).

---

## 7. Phases

### Phase 0 — Scaffold

**Goal:** empty-but-real application: builds three ways (cargo, meson, flatpak-builder),
window opens, CI checks in place.

Tasks:
1. Cargo workspace with `vitrine-engine` and `vitrine-app` as in §2.
2. Meson build: compiles Blueprints, generates gresource, builds via cargo, installs
   desktop file + metainfo + icon. `meson_options.txt` with `profile` (default/devel).
3. `AdwApplication` + `AdwApplicationWindow` from a Blueprint template; app ID wired;
   about dialog with version from meson.
4. Flatpak manifest against `org.gnome.Platform//48` (or current stable at execution
   time), with generated `cargo-sources.json` for offline build. Permissions exactly §4.
5. Fixtures per §6.
6. Check scripts (run in CI and locally via `just` or a `checks.sh`):
   - `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`
   - **Engine purity check:** `cargo tree -p vitrine-engine -e normal | grep -E "gtk4|glib|gio|libadwaita"`
     must return nothing.
   - `blueprint-compiler` compiles all `.blp` (implicit in meson build).
7. `README.md` stub with build instructions.

Acceptance:
- `flatpak-builder --disable-download` (after one vendored fetch) builds and the app runs
  showing an empty AdwApplicationWindow with a headerbar.
- All check scripts pass.

### Phase 1 — Grid + viewer + filmstrip (the product's feel lives here)

**Goal:** open a folder, see a fast virtualized thumbnail grid, click into a GPU-composited
viewer with a filmstrip, navigate with keyboard. No database yet.

Tasks:
1. **Folder model:** `GtkDirectoryList` (or custom `gio::ListStore` fed by async enumerate)
   filtered to image mimes; sort by name/mtime.
2. **Grid:** `GtkGridView` + `GtkMultiSelection`. Item template in Blueprint: thumbnail
   picture + filename label. Rubber-band selection, Ctrl/Shift ranges, Ctrl+A, Delete
   (to trash via `gio::File::trash`), Space = quick preview, Enter/double-click = viewer.
3. **Thumbnails (`thumbnails.rs`):**
   - Look up shared freedesktop cache first (MD5-of-URI path, `normal`/`large`/`x-large`
     sizes; validate `Thumb::MTime`).
   - On miss: async glycin decode **at thumbnail resolution** (set frame request size),
     write cache entry per spec, hand texture to the cell.
   - Fan out misses as concurrent async glycin requests (glycin pools/keeps loaders warm
     ~30 s — do NOT build a rayon full-decode pipeline for the grid).
   - Verify the §4 sandbox RISK here and document the outcome in README.
4. **Viewer (`viewer.rs` + `decode.rs`):** single image, zoom/pan (scroll, pinch,
   double-tap-to-fit), fit/100% toggle, arrow-key prev/next.
   - Decode-at-target-resolution: request glycin frames sized to the widget scale;
     re-request higher res on zoom-in past a threshold.
   - **LRU texture cache** (config: ~256 MB default) keyed by (path, mip level).
   - **Prefetch** ±2 neighbors at current view size on navigation.
5. **Filmstrip:** `GtkListView`, horizontal, synced both ways with the grid selection and
   the viewer's current item.
6. Window layout: `AdwNavigationSplitView` (sidebar placeholder + content),
   `AdwToolbarView`, browser page ↔ viewer page via `AdwNavigationView`.

Acceptance:
- Opening a folder of 5,000 images scrolls the grid at 60 fps after warm cache
  (measure with `GDK_DEBUG=frames` manually; automated criterion: no decode work on the
  main thread — assert via code review + a debug counter).
- Viewer next/prev with prefetch shows no visible decode flash for ±1 navigation on
  a warm cache.
- Corrupt/fake fixtures render a broken-image placeholder, never crash.
- Keyboard model works: arrows, Space, Enter, Escape-back, Ctrl+A, rubber-band.

Tests (engine): none new (this phase is app-side). App-side: a `#[cfg(test)]` unit for the
LRU cache eviction policy (pure logic — consider hosting LRU in the engine crate instead
so it's testable without GTK: **do that**).

### Phase 2 — Index

**Goal:** background scanner populates SQLite; EXIF-aware sort/filter; metadata sidebar;
backup/export. The grid can now show library roots, not just ad-hoc folders.

Tasks:
1. Engine `schema.rs`/`db.rs`: schema from §5, migrations, open/backup (`VACUUM INTO`),
   JSON export/import of tags/ratings/collections.
2. Engine `scanner.rs`: walk roots; for each file compare (path, size, mtime) against DB;
   changed/new files → hash queue. Handle: new, modified, moved (same content_hash appears
   at new path → update path, clear `missing`), deleted (set `missing=1`; purge policy:
   rows with `missing=1` and no tags/ratings are deleted after 30 days).
3. **One-pass ingestion (app-side orchestration):** for each new/changed file, a single
   glycin decode yields: thumbnail texture (cache write) + EXIF fields → engine; raw file
   bytes stream → `blake3` (engine, rayon); downscaled thumbnail pixels → `image_hasher`
   pHash (engine — bridge glycin frame to an `image::ImageBuffer`; write the small
   pixel-format conversion once, in the app, and pass a plain RGB8 buffer to the engine).
4. EXIF sort/filter in `query.rs`: sort by date-taken/name/size/dimensions; filter by
   camera, orientation, format, date range. Wire into a `GtkFilterListModel` /
   `GtkSortListModel` or engine-side SQL (prefer SQL — the DB is the query engine).
5. Metadata sidebar (`metadata_panel.rs`): EXIF display for current selection.
6. Scan progress UI: unobtrusive `AdwBanner`/progressbar; scanner is cancellable.
7. Settings: manage library roots (portal chooser for extra roots), cache size.

Acceptance:
- Fresh scan of the fixture set produces correct rows (exact expected values asserted in
  an engine integration test using a temp DB and the fixture files).
- Rename a fixture file on disk, rescan → same content_hash row keeps its path updated;
  a tag applied before the rename survives (THE core promise — this test is mandatory).
- `VACUUM INTO` backup produced on schedule (on clean exit, ≥ daily) and restorable.
- UI sort/filter by date-taken works against EXIF fixture.

Tests (engine): scanner state machine (new/modified/moved/deleted), hash correctness
against known digests, migration idempotence, backup/restore round-trip, export/import
round-trip.

### Phase 3 — Tags, stars, Collections

**Goal:** the differentiator layer.

Tasks:
1. Tagging: tag entry with autocomplete (existing tags), apply/remove to selection
   (snapshot semantics — "select all in folder + apply" is the v1 directory-tagging story).
   Batch-write in one transaction.
2. Ratings: 0–5 stars on selection; keyboard 0–5 in grid and viewer; shown in cell overlay
   (small) and metadata panel.
3. Collections sidebar (one list, two kinds):
   - **Smart:** name + predicate builder (tags any/all, rating ≥, date range, camera,
     format). Stored as JSON in `collections.query`; engine compiles JSON → SQL. Live count.
   - **Catalog:** hand-curated ordered list; add-to-catalog from selection context menu;
     manual reorder via DnD in catalog view.
4. Tag/rating/collection changes reflected in open smart-collection views without restart
   (simple approach: re-run query on change signal; optimize only if it's actually slow).
5. Filter bar in the browser: quick filter by tag / min rating.

Acceptance:
- Tag 500 selected files < 1 s (single transaction, engine test with temp DB).
- Smart collection "tag=X AND rating≥4" returns exactly the expected fixture rows
  (engine test) and updates live in the UI when a rating changes (manual check).
- Catalog preserves manual order across restart.
- The Phase 2 rename-survival test extended: tag + rating + catalog membership all
  survive a file move.

Tests (engine): JSON-predicate → SQL compiler (property: never interpolates strings —
parameters only; include an injection-attempt fixture), catalog ordering ops,
tag CRUD invariants.

### Phase 4 — Find Duplicates + ship

**Goal:** dedup as an engine-isolated module; release polish; Flathub submission.

Tasks:
1. Engine `dedup.rs` (zero GTK imports, no glycin — operates purely on DB rows):
   - **Exact:** GROUP BY content_hash HAVING count > 1.
   - **Near:** linear scan Hamming distance over `phash` (XOR + popcount), threshold
     configurable (default ≤ 8 bits); cluster via union-find. Linear is fine at
     personal-collection scale; no BK-tree in v1.
2. Duplicates UI: cluster list → side-by-side compare (reuse viewer), per-file
   keep/trash actions (trash only, never unlink), "keep largest / keep oldest" bulk
   helpers. Every destructive action goes to trash via `gio`.
3. Polish pass: HIG review, `AdwStatusPage` empty states, shortcuts window
   (Ctrl+?), app icon, metainfo screenshots + release notes, translations scaffolding
   (gettext + `po/`), a11y labels on grid cells and viewer controls.
4. Release engineering: version tagging, `flatpak-builder` offline verification,
   Flathub submission PR per current submission docs; `flatpak run --command=sh` smoke
   checklist documented in README.

Acceptance:
- Dedup engine tests: exact pair found; near pair (png/jpg + scaled fixtures) clusters at
  threshold 8; unrelated images do not cluster; union-find produces stable cluster IDs.
- Dedup on 10k synthetic rows completes < 1 s (engine bench-ish test, generous bound).
- `appstreamcli validate` passes on metainfo; `desktop-file-validate` passes.
- Offline flatpak build from a clean tree succeeds.

---

## 8. Empirical notes & gotchas (accumulated design intel — trust these)

- glycin spawns **sandboxed loader subprocesses**, runs them in parallel, and keeps them
  warm ~30 s. For the grid, prefer thumbnail-cache reads + async glycin fan-out for misses.
  Reserve the custom decode-at-resolution + prefetch machinery for viewer/filmstrip.
- Feed `image_hasher` the **downscaled thumbnail pixels** already produced by the ingestion
  decode — never a second full decode.
- freedesktop thumbnail cache keys are **MD5 of the file URI** — inside Flatpak, portal-doc
  paths produce different URIs than the host; expect cache misses for portal-granted roots
  (see §4 RISK).
- `blake3` hashes the **file bytes** (identity), `image_hasher` hashes **pixels**
  (similarity) — do not conflate; both are index columns, computed in the same ingestion
  pass but from different inputs.
- Ratings use `Xmp.xmp.Rating` 0–5 semantics now so v2 embedded write is a pure sync step.
- EXIF Orientation must be applied for display AND for pHash input consistency (hash the
  oriented pixels; otherwise a rotated copy won't near-match).
- rusqlite: enable WAL; all multi-row mutations in explicit transactions; one writer
  connection + read pool is sufficient at this scale.
- gtk-rs: `GtkGridView` item factories must never do sync I/O in `bind` — bind a placeholder,
  fill via async completion, and guard against recycled cells (compare a generation token
  before setting the texture).

## 9. Deferred (v2+, do not build now, do not preclude)

Embedded metadata write via rexiv2 (activates `sync_state`); sidecar-only mode; dynamic
path-based tag rules (activates `source='rule'`); Lua/Rhai scripting tier; batch
rename/convert/rotate; "find similar" UI over the already-indexed pHash; WASM plugin tier
with Flatpak `add-extension`; editing tools; device import; slideshow.
