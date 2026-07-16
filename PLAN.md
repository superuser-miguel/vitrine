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
2a. **Comments** (per-image free-text caption). A third annotation alongside tags/stars,
   content-hash keyed so it survives renames. New `comments` table mirroring `ratings`
   (`content_hash` PK, `body TEXT`, `sync_state` v2 seam, `updated_at`); added as a
   forward-only migration when built. **Store `dc:description`-compatible semantics from
   day one** — same trick §8 uses for `Xmp.xmp.Rating`, so the deferred v2 embedded-write
   (rexiv2, §9) is a pure sync and the field round-trips with gThumb/digiKam/Lightroom /
   Nautilus properties. v2 read side: enrichment can seed the comment from an existing
   embedded `dc:description`, importing comments users already made in gThumb. v1 is
   **DB-only** (sidecar/embedded sync rides the same deferred write-back as tags/ratings).
   UI: an editable row (AdwEntryRow / GtkTextView) in the viewer's properties sidebar —
   the one editable field beside the read-only dimensions/camera/date. Backup/export gains
   a content-hash-keyed `CommentExport`. Single free-text caption per image — **not** a
   threaded/multi-comment model.
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
path-based tag rules (activates `source='rule'`); Lua/Rhai scripting tier — incl.
**user-defined custom sort orders** (§10.3.1, the likely first use case) and rename/predicate
rules; batch rename/convert/rotate; "find similar" UI over the already-indexed pHash; WASM
plugin tier with Flatpak `add-extension`; editing tools; device import; slideshow.

---

## 10. v2+ feature drafts (design intent — refine before building)

> Cleaned-up brainstorming for the §9 deferred items. Some of this restates work
> already scheduled for v1 (dedup = Phase 4, collections = Phase 3, BLAKE3 =
> §5/Phase 2) — flagged inline. Treat as intent, not committed scope.

### 10.1 Duplicate finder — fuller spec

Core dedup already ships in **v1 Phase 4**; this is the expanded spec.

**Goal:** better than gThumb's — fast, accurate, visual, non-destructive.

**Core (engine, `vitrine-engine::dedup`, zero GTK):**
- **Exact:** group by `content_hash` (BLAKE3).
- **Near:** perceptual hash (`image_hasher`) + Hamming distance (configurable, default ≤ 8 bits).
- Clustering via union-find (connected components).
- Operate purely on DB rows (linear scan is fine at personal scale; optional BK-tree later).

**Inputs:**
- Scope: current folder, collection, or entire library.
- Filters: min size, date range, file types.

**Outputs (to app):**
- Clusters, each with a representative (largest / oldest / best quality) and its files + metadata (size, date, path, preview hash).
- Actions: keep one (auto or manual), trash the rest via `gio::File::trash`.

**UI (app layer):**
- Dedicated page/dialog with a cluster list (thumbnails side-by-side).
- Bulk actions: "keep largest", "keep newest", "keep manual".
- Preview: reuse the viewer for side-by-side compare.

**Perf & safety:** incremental (only scan new/changed files); progress banner; dry-run mode.

**Forward (v2+):** WASM plugin for custom similarity metrics (e.g. AI embeddings).

### 10.2 Catalogs / Collections — integration outline

This is **v1 Phase 3** (schema in §5); kept here as the fuller outline. Unified
"Collections" (smart + catalog) in the sidebar, SQLite as source of truth.

- **Schema (`schema.rs`):** `collections` (`id`, `name`, `kind` `smart`|`catalog`, `query` JSON for smart); `collection_items` (catalogs, ordered by `position`).
- **Engine API (`query.rs` + `collections.rs`):**
  - Smart: JSON predicate → SQL (tags any/all, rating ≥, date range, EXIF).
  - Catalog: manual add / remove / reorder (DnD in UI).
  - Live counts + change signals.
- **Scanner integration:** on file change/move, smart-collection memberships update automatically.
- **App layer (`sidebar.rs`):** one unified list — smart (predicate builder) + catalogs; drag-from-grid to catalog; "Add to Collection" context menu.
- **Persistence:** backup/export includes collections (JSON keyed by `content_hash`).

**Acceptance:**
- Rename a tagged file → survives in the catalog / smart collection.
- Smart collections update live when tags/ratings change.

### 10.3 Scripting tier — Lua/Rhai (ImageMagick investigation)

> **Two extension tiers, distinct jobs (do not conflate):**
> **Lua/Rhai (§10.3)** = the *lightweight, in-process* tier — rules, rename
> patterns, smart-collection predicates, and custom **sort keys** (§10.3.1); pure,
> hot-reloadable, no heavy deps.
> **WASM (§10.5)** = the *heavy-compute* tier — image processing, custom
> similarity metrics / AI embeddings; sandboxed, out-of-process, shipped as
> add-extensions. Built later.

ImageMagick ships Lua support (the `magick` Lua module): full access to its
processing (resize, convert, effects, composites, format conversion, metadata),
run via `magick -script script.lua` or embedded; hot-reload friendly.

**Fit for Vitrine:**
- *Pros:* powerful for batch/editing rules; mature; callable via subprocess (safe).
- *Cons:* heavy if embedded (C API); Vitrine prefers pure Lua/Rhai in-process for rules + WASM for heavy compute.
- **Recommendation:** `rhai` or `mlua` for lightweight rules / renames / predicates; offload heavy ImageMagick-style ops to a WASM plugin (or an optional Lua + ImageMagick subprocess extension). Strong case for Lua in v2 — familiar to users from ImageMagick workflows.

#### 10.3.1 Custom sort orders — first concrete use case (user request, 2026-07-15)

The v1 grid sort is a live `gtk::SortListModel` + `CustomSorter` over per-item
facts (Name / Size / Modified / Type; see `window.rs`). That comparator is a
clean, low-risk **first extension point** for the scripting tier: let users
register their own sort orders in Lua, which then appear as extra entries under
the header "Sort By" menu alongside the built-ins.

- **Model — key function, not comparator.** A script exposes
  `key(item) -> comparable` (number, string, or tuple/array), *not* a pairwise
  `compare(a, b)`. Rationale: the key is computed **once per item** and memoized,
  so an O(n) pass feeds GTK's O(n log n) sort — a pairwise Lua call in the hot
  comparison path would be too slow on 10k+ folders and risks non-transitive
  (unstable) orderings. Direction (asc/desc) and the case-folded name tiebreak
  stay native, reused from the built-in path.
- **Item context (read-only).** Expose the fields Vitrine already has in memory
  or in the index: `name`, `path`, `size`, `mtime`, and the enriched columns
  `width`, `height`, `date_taken`, `camera`, `orientation`, plus v1 Phase 3
  `rating` and `tags`. No I/O, no mutation — pure functions only, so the sort is
  stable and re-runnable.
- **Examples this unlocks:** natural/numeric filename order (`img_2` < `img_10`);
  aspect ratio (`width/height`); camera then date; rating then name; EXIF focal
  length or ISO (once indexed); folder depth; "unrated first". Natural-sort is the
  most-requested and could alternatively be promoted to a v1 built-in.
- **Delivery.** Pure in-process `mlua`/`rhai`, sandboxed (no filesystem/network),
  hot-reloaded from an extensions dir; each script declares a display name + the
  `key` function. Registered scripts add radio items to the sort menu; selecting
  one swaps the `CustomSorter`'s key source and calls `sorter.changed()` — the
  same live path the built-ins use. Recompute/memoize keys on metadata change.
- **Ties into:** the scripting tier above (shared Lua host + sandbox), §5 stored
  metadata (the key's input columns), and Phase 3 tags/ratings (richer keys).

#### 10.3.2 More Lua use cases (user, 2026-07-15)

Beyond custom sort keys (§10.3.1), the Lua tier should cover:

- **Batch operations via ImageMagick** — expose a scripting hook that runs the
  `magick` CLI as a *subprocess* (house rule 3: no C++ linking) over the current
  selection: convert format, resize, rotate, watermark, strip metadata, etc. A
  Lua script declares its name + params (glob of ImageMagick options) and a
  destination policy (in place / copy to a subfolder / new suffix). This is the
  gallery-dl-adjacent "process a batch" workflow. Sandbox: needs write access to
  the target folder — flows through the portal like everything else.
- **Rename patterns / predicate rules** — already sketched (smart-collection
  predicates, rename templates); Lua expressions over item metadata.

**Distinct from Lua: an in-app EDIT tier** (crop / rotate / resize / flip — the
Loupe/gThumb "Edit File" feel). This is *app* work, not a plugin, and can use the
**`image` crate we already depend on** (pure Rust, no C++) for the pixel ops +
re-encode — no glycin (decode-only) and no new deps. Design intent:
- **Non-destructive first**: record edits as an operations list keyed by
  `content_hash` (a new `edits` table / sidecar), applied on export — so the
  original is never clobbered. "Save a Copy" writes the baked result via `image`.
- A minimal editor page (reuse the viewer): crop handles, rotate 90°, resize
  dialog, flip. Loupe/gThumb are the UX references.
- Later: expose these ops to Lua for *batch* editing (crop-all, resize-all).
- Ordering: this is **v2/v3** and larger than the plugin tiers — keep it behind a
  clear "editing" milestone; §9 already defers "editing tools".

### 10.4 BLAKE3 hashing — implementation notes

This is **v1** (`hash.rs`, §5/Phase 2); notes for reference. Extremely fast
(SIMD), cryptographic, parallelizable, content-based identity that survives
renames. Crate: `blake3` (official, maintained, `no_std`-friendly).

```rust
// crates/vitrine-engine/src/hash.rs
pub fn compute_blake3(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string() // 64-char hex
}

// Streaming / large files (memory-efficient)
pub fn blake3_from_reader<R: std::io::Read>(mut reader: R) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}
```

- **Ingestion (one pass with the glycin decode):** raw bytes → BLAKE3; downscaled thumbnail pixels → `image_hasher`; store both (`content_hash` + `phash`) in `files`.
- **Perf:** rayon-parallelizable for batch ingestion; handles multi-GB libraries.
- **Testing:** fixtures (exact match, rename survival).

### 10.5 WASM plugin tier (v2/v3)

**Architecture:**
- Tier: **compute plugins only** (no UI injection).
- Runtime: `wasmtime` (mature, secure, Rust-native) or Extism (higher-level, marketplace-friendly).
- Distribution: separate Flatpaks via an `add-extension` point (same mechanism glycin loaders use).

**Plugin contract (declarative params + compute):**

```rust
// Host declares the schema
struct PluginManifest {
    name: String,
    version: String,
    parameters: Vec<Param>, // float slider, enum, color, …
    entrypoint: String,     // e.g. "process_image"
}

// Plugin (WASM) receives
struct PluginInput {
    image_data: Vec<u8>, // or a temp-file path
    params: serde_json::Value,
}

// …and returns
struct PluginOutput {
    result: Vec<u8>,                           // processed image or JSON
    metadata_delta: Option<serde_json::Value>,
}
```

- **Host responsibilities:** render UI controls from the manifest (sliders, etc.); sandbox execution (memory/time limits); run the plugin in a worker/isolated context; apply results (image edits or DB tag updates).
- **Use cases (prioritized):** custom duplicate-similarity metrics; advanced filters/effects; batch converters/watermarking; AI-based tagging (future).
- **Implementation (v2):** add `wasmtime` + `serde` to the engine (optional feature); define safe host functions (`host_log`, `host_decode_image`); plugin manager in the app (load from the extension point); strict capabilities (no filesystem access unless granted).
- **Complementary Lua tier:** `mlua`/`rhai` for rules, rename patterns, smart-collection predicates — hot-reloadable, embedded.

#### 10.5.1 WASM plugin ideas (user asked, 2026-07-15)

Concrete compute-heavy use cases that justify the sandboxed WASM tier (each takes
decoded pixels or a temp path in, returns tags/vectors/scores/bytes out — all
local, no network unless granted):

- **AI auto-tagging** — a bundled image-classification model (ONNX via `tract`, or
  the plugin ships weights) labels objects/scenes → writes tags. Local, private.
- **Semantic "find similar" / smarter dedup** — compute CLIP-style embeddings per
  image; near-duplicate + "more like this" by vector distance, going *beyond*
  pHash (the §10.1 forward note). Store the embedding as a blob keyed by
  `content_hash`.
- **Face detection + grouping** — detect faces, cluster by person (digiKam/Photos
  style). Strictly on-device; a natural "People" smart collection.
- **OCR** — pull text from screenshots/scans → searchable + auto-taggable.
- **Quality/aesthetic scoring** — blur/exposure/sharpness detection → auto-flag or
  auto-rate blurry shots; a "review the bad ones" workflow.
- **Dominant-colour / palette extraction** — filter or sort by colour.
- **Custom similarity metrics for the dedup finder** — SSIM, alternative
  perceptual hashes, plugged into the union-find clustering.
- **Heavy batch image ops** — filters/effects/watermark as sandboxed compute
  (the original §10.5 use case), complementing the Lua+ImageMagick subprocess path.
- **Exotic-format thumbnailers** — decode formats glycin doesn't, returning pixels.

Recommendation: prioritise **auto-tagging** and **embeddings-based similarity** —
they're the highest-leverage (they feed tags + smart collections + a better
duplicate finder, all already built) and clearly *compute*, not *rules* (so WASM,
not Lua). Capabilities per §10.5: memory/time limits, no FS unless granted.

### 10.6 Sidebar navigation — Tree + Bookmarks + view switcher (user request, 2026-07-15)

The sidebar is currently a placeholder (`window.blp`: an `AdwNavigationSplitView`
sidebar with a `places_list` ListBox and an "Open Folder…" row). Replace it with a
gThumb-style **switchable** left pane. Near-term (pairs with **Phase 3**, which also
wants a Collections sidebar — §10.2); slot as **Phase 3a**.

**Shape — one stack, an icon switcher.** An `AdwViewStack` holding the sidebar views,
with an **`AdwViewSwitcherBar` pinned to the bottom of the sidebar** (gThumb's bottom
icon row — click an icon to switch the pane). Design it as an **N-page** switcher from
the start so **Collections** (Phase 3) drops in as a third page for free — gThumb's
Folders / Catalogs split is exactly Tree / Bookmarks / Collections. All pages funnel
into the existing `open_location(gio::File)` → grid + background index.

**Tree view.** `GtkTreeListModel` + `GtkListView` with `GtkTreeExpander` (the modern,
non-deprecated tree), lazily enumerating each folder's *sub-directories* only when
expanded (directories filtered in). Click a node → load its images.
- ⚠ **Sandbox scoping (the one real constraint).** We keep **no** `--filesystem=home`
  (§4). A tree rooted at `/` or `$HOME` can only show what's granted: `xdg-pictures`
  plus folders opened/added via the portal. So **root the tree at the accessible
  locations** — Pictures, the library roots, and portal-opened folders — not `/`. A
  full host filesystem tree is out of scope without breaking the sandbox posture (and
  scoping to photo locations is the right behaviour anyway).

**Bookmarks (Nautilus-style, our own list).**
- **Persistence:** a `[Bookmarks]` list in the existing `settings.rs` KeyFile (numbered
  keys, like library roots). Our own list — the host's GTK bookmarks
  (`~/.config/gtk-3.0/bookmarks`) aren't readable in-sandbox.
- **Bookmarks ≠ Library Roots** (keep distinct): roots are *indexed in the background*;
  bookmarks are *quick-nav shortcuts*. They overlap in practice → offer an "Add to
  Library" affordance on a bookmark, but two lists.
- **Drag-to-add** (the explicit ask): a `GtkDropTarget` accepting `GdkFileList`. Sources:
  (1) external file-manager DnD (drag a folder from Nautilus — the real Nautilus
  gesture); (2) from the Tree view; (3) **plus** a reliable "Bookmark this folder"
  action (`Ctrl+D` / star button) — the grid holds *images*, not folders, so you can't
  drag out of it. Reorder via in-list DnD; remove via context menu / hover button.
- ⚠ A bookmark to an arbitrary host folder is only usable while its portal grant
  persists (same caveat as roots). Graceful path: on click, if access is denied,
  re-prompt via the portal.

**Recommendation:** build the N-page switcher (Tree + Bookmarks now, Collections at
Phase 3), tree rooted at accessible locations, bookmarks in the settings KeyFile with
drag-add + a `Ctrl+D` action. Self-contained navigation; doesn't disturb grid/index/sort.

> Note: the **filmstrip** (viewer bottom bar, Phase 1) is unrelated — it moves *within*
> the current folder image-to-image; the sidebar moves *between* folders.

#### 10.6.1 Follow-up notes (user, 2026-07-15) — for when feasible

- **Bookmarks = Nautilus model, not gThumb's.** Drag folders you use often into the
  sidebar; clicking the Bookmarks switcher **changes the left pane to present the
  bookmarked dirs** (à la Nautilus 50.x), one of the switcher's pages (§10.6). Give
  the bookmark entry a **tag/label-style icon like gThumb's** (bundle our own — see
  the tag-icon precedent: `data/icons/.../*-symbolic.svg` in the gresource,
  registered via `IconTheme.add_resource_path`).
- **Back-button navigation like Nautilus** — a header back button with folder-history
  (visited folders / collections), so you can step back through where you've been.
  Pairs with the sidebar navigation. (The viewer→browser back already exists via
  `AdwNavigationView`; this is *browser-level* folder history.)
- **Tag icon: DONE** — replaced the flat placeholder with an original tilted
  price-tag symbolic (the "looks like a real tag" shape), bundled + recolored.

---

## 11. Test suite & debug tooling (QA plan — internal memo, 2026-07-15)

Design intent for making Vitrine *testable* beyond the engine's headless unit
tests. Not built as one block — grow it alongside the phases.

### 11.1 What we have

- **Engine unit tests** (65 as of Phase 3): pure, headless, fast; cover index/
  scanner/hash/query/annotations/collections/backup. Enforced with the purity
  gate (`build-aux/checks.sh`) so the engine never grows a GTK dep.
- **Dev env hooks** (the seed of a UI harness — each sets up state, acts, and
  self-quits, often writing a PNG via GTK's own GSK renderer so no compositor is
  needed): `VITRINE_SHOT` (snapshot+quit), `VITRINE_OPEN` (auto-open viewer),
  `VITRINE_INFO` (reveal properties sidebar), `VITRINE_PREFS` (open Preferences),
  `VITRINE_SORT=<field>[:desc]`, `VITRINE_SCROLLTEST` (fling+time), `VITRINE_LOADTEST`
  (worst main-loop stall), `VITRINE_CYCLE` (cycle folders, leak hunt), `VITRINE_ICON`,
  `VITRINE_LOAD_LIMIT`, `VITRINE_DECODE_LIMIT`, `VITRINE_CACHE_CAP_MB`.

### 11.2 UI behaviour tests to build (screenshot / assertion harness)

Formalize the hooks into a `build-aux/uitest.sh` that runs the release binary
with a scripted hook against a fixture folder and asserts on the result
(stderr markers and/or PNG hashes). Categories:

- **Thumbnail rendering**
  - *Unordered* (enumerate order) vs *ordered* (each sort field/direction): assert
    the grid's item order via a `VITRINE_DUMPORDER` hook (print the first N display
    names) — extend the existing `VITRINE_SORT` dump.
  - No blank cells after scroll settles (extend `VITRINE_SCROLLTEST` to count
    still-pending cells at the end).
  - **Recycling correctness**: a fast scroll must never paint image A into a cell
    since rebound to B (the §8 guard) — assert via a fixture of distinctly-coloured
    images and a pixel check at known cells.
- **Filmstrip**
  - Sync: arrow-flipping the viewer moves the filmstrip highlight and auto-scrolls
    it into view; click-to-jump changes the main image. Hook: `VITRINE_FLIPTEST`
    (open viewer, step ±N, assert current position + filmstrip selected match).
  - Order follows the grid sort (shares the sorted model).
- **UI resize / adaptivity**
  - Launch size (default 1100×720), minimum size (360×240), **fullscreen** toggle,
    and the split-view collapse breakpoints (sidebar / properties overlay when
    narrow). Hook: `VITRINE_SIZE=<wxh>` sets the window size then snapshots; assert
    the sidebar collapses below the breakpoint.

### 11.3 Debug tooling

- **App**: a single `VITRINE_DEBUG=1` that (a) adds the `.devel` styling, (b)
  turns on `glib::g_debug` logging under the `vitrine` domain (gate verbose logs
  behind it), and (c) shows an optional on-screen **debug overlay** — live FD
  count (`/proc/self/fd`), RSS, RAM/disk thumbnail-cache stats, decode-gate
  inflight, main-loop stall high-water. Complements `GTK_DEBUG=interactive` (the
  GTK Inspector) which already works. Keep the ad-hoc `VITRINE_*` hooks; document
  them in `HACKING.md`.
- **Extensions system** (v2, §10.3 Lua + §10.5 WASM): needs its own harness from
  day one —
  - a **dry-run/REPL**: load a script (or `.wasm`), feed it a fixture item, print
    the result + any host-call log, without touching the real DB;
  - a **capabilities inspector**: show what each extension declared/was granted
    (Lua: none but read-only item context; WASM: memory/time limits, host fns);
  - **hot-reload** + clear **error surfacing** (script/trap errors as a toast +
    log, never a crash); resource-limit visibility (WASM fuel/time);
  - a bundled **"echo" test extension** fixture + assertions (custom sort key,
    predicate) so the tier has regression coverage before real plugins exist.

### 11.4 Flatpak sandbox testing

The sandbox changes behaviour (portal doc-paths, `xdg-pictures` only, thumbnail-
cache mapping, tighter FD limits), so tests must also run **inside** it:

- `flatpak run --env=VITRINE_*=… --command=… io.github.…Vitrine <fixture>` to run
  the same scripted hooks in-sandbox; snapshot to a path under the app's
  granted dirs.
- `flatpak-builder --run build-dir <manifest> <cmd>` to execute a command in the
  freshly-built sandbox (CI-friendly: build, then run the uitest hook, assert).
- Sandbox-specific assertions: FD stays flat cycling unique dirs (the dmabuf-leak
  regression — already manually verified, automate it); shared thumbnail cache is
  read-only-mapped and never polluted by portal-path writes; real-path vs doc-path
  cache keying; library-root portal grants persist across runs.
- Wire `checks.sh` (or a `checks-flatpak.sh`) to optionally run the in-sandbox
  suite when a flatpak build exists, so regressions in sandbox posture are caught
  before release.

---

## 12. Post-v1 navigation & multi-view (investigation — user, 2026-07-15)

Natural extensions to the sidebar/back-nav work. Ordered easiest → biggest.

### 12.1 Forward button

The obvious companion to Back. The window already has a `history` stack of
`Location`s; add a **forward** stack:
- Going **Back** pushes the current location onto `forward` (as well as popping
  `history`).
- Any *new* navigation (not back/forward) **clears `forward`** (standard browser
  semantics).
- Forward pops `forward`, navigates (with the `navigating_back`-style guard), and
  pushes onto `history`.
Add a `go-next-symbolic` button beside Back; sensitivity tracks the stacks. Small,
self-contained — the cheapest of these.

### 12.2 Tabs

A collection in one tab, a bookmarked dir in another, a filmstrip from a third —
Nautilus-style. Use **`AdwTabView` + `AdwTabBar`**. This is the **big
architectural change**: today the window owns *one* grid/store/sort/filter/history/
viewer. Tabs need **N independent browse states**.
- **Refactor:** extract a `BrowserView` widget that owns everything per-tab — the
  `store` + `SortListModel` + `FilterListModel` + selection + grid + its own
  `history`/`current_location` + its viewer/filmstrip. The window becomes a shell:
  the shared **sidebar** (bookmarks/tree/collections) + an `AdwTabView` of
  `BrowserView`s.
- **Shared vs per-tab:** the index (`Db`), the `Indexer`/`Annotator`, the RAM
  thumbnail cache, and settings stay **shared** (one process, one index). Sort,
  filter, selection, history, current location are **per-tab**.
- **Interactions:** middle-click a bookmark / tree node / collection → "open in
  new tab" (the Nautilus gesture); `Ctrl+T` new tab, `Ctrl+W` close tab.
- Sizeable but mechanical once `BrowserView` is extracted; do the extraction as
  its own commit first (single tab, no behaviour change), then add the tab bar.

### 12.3 Address bar

A Nautilus-style top path bar showing e.g. `/home/user/Pictures/`:
- **Breadcrumb mode** (default): clickable segment buttons; click a segment to
  navigate up to it. Built from the current `Location::Folder(path)` split on `/`.
- **Editable mode** (`Ctrl+L` / click the empty area): a `GtkEntry` to type or
  paste a path; Enter navigates. Completion over accessible dirs is a nice-to-have.
- For `Location::Collection`, show the collection name (not a path) — the bar is
  location-aware, not just a folder path.
- ⚠ **Sandbox:** typing a path outside the granted roots (Pictures / library
  roots / portal-opened) will fail — only accessible paths resolve. The breadcrumb
  is always safe (you're already there); the *editable* entry should surface a
  clear "not accessible — open via the folder chooser to grant it" message rather
  than silently failing.

**Suggested order:** 12.1 Forward (trivial) → 12.3 Address bar (medium, mostly
UI) → 12.2 Tabs (the `BrowserView` extraction is the real work). All three are
post-v1 polish; none block shipping v1.

## 13. Rendering & decode-scheduling performance (research backlog — user, 2026-07-16)

Perf is the app's north star. The decode/thumbnail pipeline is already
*viewport-aware* (virtualized grid, bind-driven loads, 90ms scroll-settle
debounce, directional prefetch `PREFETCH_AHEAD 64` / `PREFETCH_BEHIND 16`, a
concurrency gate `thumbnails::load_gate`), but it is **not yet viewport-
*ordered*** — when a settle fires, `flush_pending` (window.rs) spawns decodes in
bind/drain order through a flat semaphore, so completion order is "whatever
decodes fastest," and thumbnails pop in scattered rather than filling outward
from where the eye is. This section captures the specific fix plus a researched
catalog of scheduling/rendering ideas from browsers, virtualized-list libraries,
GNOME image apps, and GPU texture pipelines — each mapped to Vitrine and to a
concrete test. **None are needed to ship; all are perf polish to A/B on real
folders.**

Key fact grounding all of this: **the app already has the viewport info it
needs** — the GPU is downstream and knows nothing about scrolling; GTK's
virtualized `GridView` calls `connect_bind` only for on-screen cells (+ buffer)
and hands us `list_item.position()`, and `grid_scroller.vadjustment()` gives the
exact pixel viewport. So prioritization is purely an app-layer scheduling
decision, no engine change.

### 13.1 Viewport-ordered decode scheduler (the immediate optimization)

**Problem.** Loads for a settle batch complete in decode-time order, not visual
order; and if you scroll again before the queue drains, a now-stale queued load
still holds its turn at the gate ahead of newly-visible items.

**Design.**
- Replace the flat `pending` drain + plain semaphore with a **priority queue**
  keyed by *distance from the viewport* (top-visible index, or viewport centre
  for centre-out fill). Compute the reference index from
  `grid_scroller.vadjustment()` (value + page_size ÷ row height) at flush time.
- Feed the gate from that priority queue so the visible region resolves **first
  and in order**, then radiates outward, then the prefetch margins fill.
- **Re-prioritize on each scroll settle**: newly-visible items jump ahead;
  scrolled-past queued items are dropped or demoted (partially done today —
  `flush_pending` already skips cells whose item changed).
- Keep the existing debounce (don't decode mid-fling) and the directional
  prefetch (bias in the scroll direction — see 13.2/overscan).

**Test cases (extend VITRINE_SCROLLTEST / VITRINE_LOADTEST):**
1. *Fill-order correctness* — on a cold folder, assert the first N completed
   thumbnails are the top-of-viewport items (by position), not arbitrary.
2. *Time-to-first-visible-thumb* — measure ms from folder-open (or scroll-stop)
   to the moment every currently-visible cell has a real texture; compare
   scheduler on/off. This is the metric that maps to perceived snappiness (cf.
   browser LCP — see 13.2).
3. *Scroll-jump staleness* — scroll to A, immediately jump to B before A drains;
   assert B's visible cells decode before any remaining A-only cells.
4. *No regression on cached folders* — with everything cached, scheduler adds no
   measurable stall (priority queue overhead must be negligible).
5. *Gate saturation* — big icon size + cold 27k folder: main-loop stall stays at
   the current release floor (~9.6s scrolltest, ~52ms worst LOADTEST stall).

### 13.2 Concept catalog (from real systems — A/B candidates)

For each: *what it is · where it comes from · how it maps to Vitrine · how to
test.*

- **Priority hints / boost-visible-first.** Browsers start in-viewport images at
  Low, then boost once layout finds them visible — often "too late." `fetchpriority`
  lets the important image start High immediately (median 21ms vs 102ms to first
  byte). *Vitrine:* the 13.1 priority queue *is* our fetchpriority; additionally
  mark the item under the cursor / selected / just-activated as **High** so it
  jumps the queue. *Test:* case 13.1.2 above, plus "selected item decodes first."
- **Overscan / directional prefetch tuning.** Virtualized lists render a small
  buffer beyond the viewport; overscan is a direct trade (fewer blank flashes vs.
  more work), and good ones bias in the scroll direction. *Vitrine:* we have
  `PREFETCH_AHEAD/BEHIND`; make them **velocity-adaptive** (faster scroll → larger
  ahead margin, smaller behind) using vadjustment delta over time. *Test:* blank-
  cell count during a fixed fling at several speeds; RSS stays flat (don't let
  overscan defeat virtualization).
- **Cancel / deprioritize scrolled-past work.** Windowing libs decode far-from-
  viewport items at low priority "after running interactions." *Vitrine:* partly
  done (debounce + skip-changed-cell); extend to **cancel in-flight decodes** for
  items flung far off-screen (glycin decode is a subprocess — cancellation frees
  the gate slot sooner). *Test:* VITRINE_CYCLE across dirs; assert no wasted
  completed decodes for never-settled cells; FD/gate-slot flatness.
- **Progressive / coarse-first fill.** GPU texture pipelines draw a coarse mip
  when there isn't frame time to decode full detail, refining later; browsers
  show a placeholder then swap. *Vitrine:* we downscale already; consider a
  **two-pass fill** — blit the shared-cache 256px (already read-cheap) instantly,
  then swap in the sharp x-large/xx-large bucket for the current icon size. *Test:*
  time-to-*any*-pixels vs time-to-sharp; ensure no visible flicker on swap.
- **Relevance-aware cache eviction.** Texture caches evict by more than raw LRU —
  keep what's near the region of interest. *Vitrine:* `SizedLru` is pure LRU;
  consider biasing eviction to protect items near the current viewport/folder.
  *Test:* scroll away and back; assert near-viewport thumbnails survived. (Tie to
  [[vitrine-thumbnail-cache-strategy]].)
- **More parallel thumbnailers.** gThumb sped up thousand-image dirs by starting
  more thumbnailers in parallel. *Vitrine:* the gate limit (`VITRINE_LOAD_LIMIT`,
  default ~24) is our knob; glycin's pool is unbounded and must stay gated (see
  the deadlock gotcha in §8). *Test:* sweep the gate limit vs. main-loop stall +
  total fill time to find the real optimum per host (host glycin 2.1.5 vs //50).
- **⭐ Cost-aware / lane-separated scheduling (LIKELY ROOT CAUSE — user, 2026-07-16).**
  The gate is a flat count semaphore, so it treats a 24 MP AVIF and a 400 px JPEG
  as equal. In a **mixed-size folder** (many small + some very large, the user's
  real case), a burst of large decodes grabs the slots and each holds one for far
  longer — **head-of-line blocking**: the small, *visible* thumbnails queued
  behind them starve, so the grid fills erratically exactly where sizes vary.
  Large sources also spike RAM (glycin often returns full-res on host loaders —
  §8), so several concurrent big decodes = a transient memory bulge (cf. the 27k
  OOM history). *Fixes to A/B:* (a) **estimate decode cost** — file bytes
  (`item.size()`, known at enumerate) as the cold proxy, refined to pixel **area**
  once enriched (`width*height` already indexed; `texture_cost` already computes
  w·h·4); (b) run large decodes in a **separate low-concurrency lane** so they
  can't monopolise the gate and starve small visible items (analogous to a
  browser's per-host connection cap / HTTP-2 prioritisation); (c) **memory-budget
  the gate** — bound *in-flight decoded bytes* (a weighted semaphore), not just
  the count, so N big images don't decode at once. *Test:* the variable corpus in
  §13.4 — a large image sharing the viewport must not delay the small visible
  ones; RSS stays bounded with several large images visible at once.
- **Per-frame decode budget / time-slicing.** Progressive renderers cap work per
  frame to avoid dropped frames. *Vitrine:* if priority-queue draining ever
  competes with the main loop, drain **at most k results/main-loop-iteration**
  (idle callback) so applying textures never janks a scroll. *Test:* VITRINE_LOADTEST
  worst-stall must not regress while a large batch applies.
- **Decode-ahead in scroll direction (prefetch as prediction).** Covered by
  overscan above, but note the *prediction* angle: use vadjustment velocity to
  prefetch where the user is *going*, not a symmetric margin. Already partially
  reflected in AHEAD>BEHIND; make it dynamic.

### 13.3 Harness additions to make these measurable

The above all need two metrics the current hooks don't cleanly expose:
- **Fill-order log** — a VITRINE_DEBUG mode that records, per decode completion,
  `(position, was_visible_at_completion, ms_since_settle)` so we can assert
  order/latency instead of eyeballing.
- **Time-to-visible-complete** — instrument the settle→"all visible cells have
  real textures" interval (the perceptual analogue of browser LCP for our grid).

### 13.4 Test corpus: size & format variability (user, 2026-07-16)

The committed fixtures (`tests/fixtures/images/`) are all uniform ~100×50 — fine
for engine correctness, **useless for perf**, because the symptoms only appear
when image *cost* varies. Perf test cases must run on a corpus that deliberately
mixes:
- **Size:** many small (e.g. 256–512 px) interleaved with some very large
  (e.g. 6000×4000, ~24 MP) — and test several *arrangements*, since arrangement
  is what triggers head-of-line blocking: a large image at the **top of the
  viewport**, a **cluster** of large images, and large images **sprinkled** among
  small ones.
- **Format:** JPEG (fast) mixed with AVIF / JXL (much slower to decode) — decode
  cost varies by format, not just by pixels, so the mix must too.
- **Scale:** enough items (thousands) that virtualization + the gate actually
  engage.

**Do not commit this corpus** — it would bloat the repo (the fixture generator is
deliberately kept under a couple MB). Add a *separate* generator
(`tests/fixtures/perf_corpus.py` or a `VITRINE_*` dev mode) that synthesises a
configurable mixed corpus into a scratch/temp dir on demand, so CI and manual
perf runs build it fresh.

**Assertions this corpus unlocks (tie to §13.1–13.3 metrics):**
1. *No head-of-line starvation* — with a large image sharing the viewport, the
   small visible thumbnails still reach a real texture within the target latency
   (they must not wait on the big decode).
2. *Bounded memory* — several large images visible/decoding at once keeps RSS
   under budget (no full-res pile-up).
3. *Fill order holds under cost variance* — the viewport-ordered scheduler
   (§13.1) still fills top-down/centre-out even when decode times differ wildly.
4. *Format-mix latency* — a viewport of AVIF/JXL fills within a tolerable factor
   of an equivalent JPEG viewport (surfaces decoder-bound stalls).

Run all of §13.1's cases against **each arrangement**, not just a uniform folder —
uniform fixtures would hide the very bug the user is hitting.

**Real reference corpus (user's gallery-dl folders, measured 2026-07-16).** Six
Instagram folders, ~44k images / ~20 GB. Per folder: **7k–14k images**, byte size
**p50 ≈ 200–340 KB, p90 ≈ 0.8–1.3 MB, p99 ≈ 1.4–3.4 MB, max ≈ 3–8.7 MB** — a
~40× spread *within a single folder*. They also hold many **non-image files**
(JSON sidecars, videos: one folder is 7,699 files but 1,807 images), i.e. the
enumerate/scan walks far more entries than it displays. The synthetic corpus
should match this shape: thousands of files, ~40× size spread, JPEG-dominant with
some AVIF/JXL, and a chunk of non-image siblings.

### 13.5 CONFIRMED root cause — background enrichment starves the foreground (2026-07-16)

Diagnosed from code + the corpus above when the user reported "UI slow again,
images not loading" after adding the folders. **This is the primary "why Nautilus
beats us."**

- **Two decode gates, one real resource.** Interactive thumbnail loads use
  `thumbnails::load_gate` (~24); background enrichment uses `decode::decode_gate`
  (`min(cores, 8)`, floor 4). They're separate semaphores but both spawn glycin
  subprocess decodes that contend for the **same CPU cores / glycin pool**, which
  the decode-gate note itself says **plateaus at ~8 concurrent**.
- **Enrichment runs flat-out and never yields.** `run_enrichment` decodes whole
  `TakeBatch` batches (64) concurrently, gated only by `decode_gate`, in a tight
  loop until `paths_needing_enrichment` is empty. So a fresh backlog (here **~11k
  un-enriched** of 105k) keeps ~8 full-image decodes running continuously,
  **saturating decode throughput regardless of whether the user is actively
  browsing**. The visible thumbnails then queue behind / lose CPU to enrichment →
  slow, out-of-order fill. **Nautilus has no such decode-everything pass** — that
  is precisely the gap.
- **Mixed size compounds it** (§13.2 ⭐): the ~40× spread means big decodes hold
  their slot far longer, so both the enrichment lane and the interactive lane
  suffer head-of-line blocking.
- The scanner is NOT at fault: `classify` skips unchanged files by `(size,mtime)`
  (scanner.rs), so back-nav re-walks (stat only) but never re-hashes; its slowness
  is the same decode contention hitting thumbnail reloads.

**Measured evidence (2026-07-16, installed Flatpak, real `elizarosewatson`
folder, 13,235 images, VITRINE_LOADTEST = worst main-loop stall over 10s):**
- Baseline (enrichment competing): **723 ms**.
- Enrichment throttled to 1 (`VITRINE_DECODE_LIMIT=1`): **392 ms** (−46%).
- **After enrichment-yield fix** (commit — probe() awaits yield_to_foreground):
  **533 ms** (−26% from baseline), backlog still 10,797. The modest LOADTEST delta
  is a *measurement artifact*: worst-single-stall is dominated by the synchronous
  **populate spike** (below), not decode — the yield fixes the *sustained* decode
  starvation while browsing, which worst-stall under-measures. **Need the fill/
  time-to-visible metric (§13.3) to score it properly.**
- **Second cause found — synchronous populate spike.** `Window::populate`
  (window.rs) runs on one main-loop iteration for the whole folder:
  `stamp_annotations` (a `ratings_under` DB query + a loop over every item) then
  `store.extend_from_slice(13k items)` → the SortListModel re-sorts and
  FilterListModel re-filters all 13k at once. That is the folder-OPEN freeze (the
  worst stall), distinct from decode contention. Fix: chunk/defer the insert
  (extend in batches across iterations), and move stamping off the open path
  (stamp lazily on bind, or after first paint). This is "Nautilus beats us on
  folder open"; the enrichment-yield is "…and while browsing."
- Interpretation: ~330 ms of the stall is enrichment contention; the remaining
  ~392 ms is the interactive mixed-size path (big-image decode head-of-line +
  the GPU downscale/readback done on the main thread for large images). For
  reference, the post-Phase-1 floor on cold uniform folders was ~52 ms. Both
  causes share the fix below; the interactive residual also wants the §13.1
  viewport-ordered, cost-aware scheduler and moving large-image downscale
  off-main. **723 ms is the "before" number to beat.**

**Profiled 2026-07-16 (#1 populate fix landed).** Per-step timing on the 13k
folder settled what worst-stall couldn't: `collect_images` name-sort 20 ms
(redundant — removed), **`populate` synchronous 533 ms → 2 ms** (incremental
sort/filter + off-main stamping worked), and the real worst-stall spikes were
(a) a **~1.7 s GSK/Vulkan pipeline compile** on the first `render_texture` and
(b) genuine **per-large-image GPU downscale on the main thread**
(~75 ms for a 3024×4032 / 12 MP photo) — that's item #6 (off-main downscale), and
these folders really do carry 12 MP images. Net: the recurring folder-open freeze
is fixed; the remaining felt cost during browsing is per-large-image downscale +
cold-folder decode throughput.

**Shader pre-warm — tried and REJECTED (measured 2026-07-16).** Kicked a throwaway
`render_texture` at startup to move the compile off the first-thumbnail path.
Measurement killed it: the compile is **~1.7 s on *every* launch that downscales,
NOT cached across launches** (`radv`/GSK-Vulkan recompiles the pipeline per
process; the earlier "warm" run was fast only because it browsed *cached*
thumbnails and never downscaled). So an unconditional pre-warm *forces* 1.7 s onto
every launch — including cached-only browsing that would never have paid it — a
regression on the fast path. Reverted. **The right fix is #6, and it subsumes
this:** move the downscale to a **CPU worker thread** (the `image` crate) instead
of GSK `render_texture`. That eliminates the Vulkan pipeline compile entirely (no
1.7 s, ever) *and* takes the per-large-image downscale off the main thread — one
change kills both, and drops the dmabuf-FD/readback dance too.

**Fix (recommended, do next).** Make enrichment a true *background* task that
**yields to the interactive foreground**: pause/throttle enrichment while thumbnail
loads are pending or a scroll is in flight (a shared "UI busy" signal checked
between batches/items), resuming after a short quiet period; and/or fold both into
one **priority-aware decode admission** where visible loads preempt enrichment
(§13.1). Keep enrichment correctness (single-writer monotonic queue) intact —
throttle *admission*, not the write ordering. Opportunistic win: when a full image
is decoded for the viewer, capture its pHash then to avoid a separate enrichment
decode. *Test:* on the real corpus, time-to-visible-complete while a large
enrichment backlog is pending must approach the backlog-idle case.

**DB scale (user asked "how large can the DB be?").** Not a concern. Current:
105,207 files → 51 MB (~500 B/row incl. indexes on path/hash/phash/date). SQLite's
practical ceiling is terabytes; millions of rows stay fast on the B-tree indexes
(~500 MB at 1M images). The real cost of a big library is the **one-time
enrichment decode pass** (above) and the bounded RAM/disk caches — never the DB
size. Index freely.

References (for whoever builds this): Chrome fetchpriority / LCP request
discovery (web.dev/articles/fetch-priority, developer.chrome.com/docs/performance/insights/lcp-discovery),
react-window overscan (web.dev/articles/virtualize-long-lists-react-window),
Loupe/glycin sandboxed per-file decode (blogs.gnome.org/sophieh 2023-08-30),
gThumb parallel thumbnailers (gitlab.gnome.org/GNOME/gthumb), async multilevel
texture pipelines / progressive refinement (US6618053B1; image-caching-for-fast-
scroll US patent 9501415).

## 14. UX backlog: viewer pan + removable-media bookmarks (user, 2026-07-16)

Flagged during the GPU/UI "lunch-and-learn" discussions. **§14.1 (pan) is
promoted from nice-to-have to a committed near-term add** (user: "required action
IMO") — it's the next-up implementation task. §14.2 follows.

### 14.1 Click-drag pan in the viewer — ⭐ NEXT UP (committed)

**Current state.** The viewer *already* pans when zoomed past fit — the image
lives in `picture_scroller` (a `GtkScrolledWindow`), so scroll-wheel and
scrollbars move it (viewer.rs). What's missing is the **grab-hand click-drag
gesture** everyone expects from an image viewer (Loupe, browsers): press and drag
the image itself to move it. Useful exactly when a large image is zoomed in.

**Design.**
- Add a `gtk::GestureDrag` on the picture (or scroller viewport). On
  `drag-update`, pan by subtracting the delta from
  `picture_scroller.hadjustment()`/`vadjustment()` (clamped to content bounds).
- Cursor feedback: `grab` cursor when the content exceeds the viewport (pannable),
  `grabbing` while dragging; default arrow at fit (nothing to pan).
- Optional polish: kinetic/inertial fling after release (ScrolledWindow has
  kinetic scrolling, but a raw drag bypasses it — would need momentum); middle-
  button-drag pan; "zoom to point under cursor" so Ctrl+scroll zoom keeps the spot
  under the pointer fixed (pairs naturally with drag-pan).
- Two-finger touchpad/touch pan already works via scroll — this is specifically
  the mouse click-drag path.

**Test cases.** Zoom in on a large image → drag pans and clamps at edges; cursor
changes to grab/grabbing; at fit there's nothing to pan (no cursor change, no
drift); drag-pan + Ctrl+scroll-zoom compose sanely.

### 14.2 Removable-media (USB) bookmarks — offline / greyed state

**Problem.** Bookmarks persist as `{name, path}` (settings.ini). A folder on a USB
drive lives under `/run/media/<user>/<label>/…` (or `/media/…`); unplug the drive
and that path vanishes. Today `refresh_bookmarks` renders every bookmark
identically with a folder icon and **no availability check**, so an offline USB
bookmark looks normal and clicking it opens a broken/empty folder. We want it
**greyed = offline**, kept in the list, and auto-revived on reconnect.

**Logic to build.**
- **Detect availability without blocking.** `Path::exists()` is the naive check
  but can stall on stale network mounts and can't tell "drive unplugged" from
  "folder deleted." Use GIO **`gio::VolumeMonitor`**: enumerate current mounts and
  subscribe to `mount-added` / `mount-removed` (+ `volume-*`) to re-evaluate and
  refresh the greyed state **live, no polling**. Resolve a bookmark's enclosing
  mount via `gio::File::find_enclosing_mount` (or prefix-match against mounted
  roots).
- **Three states, not two:**
  - *Online* — enclosing mount present and path resolves → normal row.
  - *Offline* — the path's removable mount root isn't currently mounted → **grey /
    insensitive**, swap the icon to `drive-removable-media-symbolic` (or an offline
    badge), keep the bookmark.
  - *Missing* — mount present but the folder was deleted → distinct "missing"
    treatment (offer *Remove bookmark?*), NOT the same as offline.
- **Interaction.** Clicking an offline bookmark shouldn't open an empty grid —
  either attempt `gio::Volume::mount` if the volume is present-but-unmounted, or
  toast "Drive not connected." On reconnect, VolumeMonitor un-greys it
  automatically.
- **Robustness follow-up (fragile-path problem).** USB drives can remount at a
  *different* path (label collisions → `LABEL_1`, etc.), which silently breaks a
  path-only bookmark. Robust version: store the **volume UUID/label + relative
  path** and re-resolve the live mount point on access. Path-only + offline
  detection is fine for a first cut; UUID re-resolution is the hardening step.
- **⚠ Sandbox (ties to §4).** Under Flatpak, reaching `/run/media/…` needs the
  right filesystem grant (or portal); a USB bookmark only works if the sandbox can
  see the mount. Bookmarks added by drag/chooser outside the granted roots may be
  unreadable even when the drive *is* plugged in — the "offline vs. no-permission"
  distinction should surface a sensible message rather than a blank folder.

**Test cases.** Bookmark a USB folder → unplug → row greys out (offline), no crash,
no error spam; replug → auto-revives via VolumeMonitor; click while offline →
mount-attempt or clear toast, never a broken empty grid; delete the folder while
mounted → "missing" state distinct from offline; (stretch) drive remounts at a new
path → UUID re-resolution still finds it.
