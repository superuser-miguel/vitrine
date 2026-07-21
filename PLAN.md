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

## Status & North Star (@ `v1.0-stable`, 2026-07-16)

**North Star.** Fast, *smooth* browsing of large image libraries — where
Nautilus / Loupe / gThumb get sluggish on tens of thousands of images, Vitrine
stays fluid. The bar is **browser-level thumbnail scrolling**: a browser renders a
10k-image page in consistent visible order while scrolling; we want that. "Smooth"
is judged **by hand**, not by a stall metric.

**Anchor.** `v1.0-stable` (git tag) is the known-good, hand-verified baseline:
windows slide with no lag, hover tracks the cursor, launch settles after 2–3 runs.
`git checkout v1.0-stable` to return to it before any interaction-path experiment.

**Shipped (v1 feature-complete).** DONE — the phase/feature sections below (§7,
§10.6, §12.1) are kept for design rationale, not as a to-do list: browse
(virtualized grid, Nautilus-style sort, icon-size, trash) · viewer + filmstrip +
zoom · review (tags, 0–5 ratings, comments) · collections (catalogs + smart) ·
filter bar · find-duplicates (exact + scalable "Similar" BK-tree) · sidebar
(Places/Folders/Collections switcher, Nautilus bookmarks, Back/Forward) · XMP
sidecar export · background content-hash index with EXIF/pHash enrichment.

**Landmark `v1.1-stable` (2026-07-18).** New known-good tag = §13 perf sprint
complete + Fable's fixes, with the edit-tier experiment reverted. `git checkout
v1.1-stable` to return here. (`v1.0-stable` still exists as the older baseline.)

### Edit tier — rotate / flip / crop (non-destructive) — SHIPPED (2026-07-18)

Arc: destructive first attempt (`2992695`) **reverted** (`4f1ab20`), then
**rebuilt non-destructively** (`1bbeed7`→`c1c7047`) as designed. Full history kept
below as a design record; see also memory `vitrine-edit-tier`. The reverted
in-place/glycin-`Complete` approach is documented so it is **not re-landed as-is**.

*What was built:* `crate::edit` wrapping glycin 3.1 `Editor`
(`Editor::new(file).edit().await` → `apply_sparse(&Operations)` →
`SparseEdit::Sparse` in-place byte patch | `Complete(BinaryData)` full rewrite).
`Operation::Rotate` is CCW (`gufo-common::orientation::Rotation`). Viewer: linked
header button group (rotate-l/r, flip-h/v) + `[`/`]` keys → `viewer::apply_transform`
→ edits the file **in place, immediately** → `window::on_image_edited`
(`thumbnails::invalidate_disk` + RAM-cache evict + `store.items_changed(idx,1,1)`
re-bind). `VITRINE_EDITTEST=<dir>` debug hook drove one RotateRight.

*Why reverted (real-use problems):*
1. **UX wrong.** Destructive edit on a single button click, no Save/Save-As, no
   mode boundary — the viewer silently becomes a destructive editor. Wanted:
   gThumb-style **edit mode/window** (enter-edit → preview transform → Save /
   Save As / Discard).
2. **glycin `Complete` re-encode is pathological for no-EXIF JPEGs** (the user's
   web/Instagram JPEGs are `orient=Undefined`, so no EXIF to patch → `Complete`
   path → physical re-encode). Observed **file grows ~2–3× and keeps ~doubling
   per successive edit** (277K→664K→1279K over rotations); result is slow to
   re-decode even in Loupe. A single rotation is directionally correct and
   ~46 dB (visually lossless) but the size explosion is unacceptable.
   **Root cause DIAGNOSED (2026-07-18, standalone host probe, glycin 3.1 crate /
   glycin-loaders 2.1.5):** two distinct defects in the `Complete` re-encode —
   (a) **chroma subsampling upgraded 4:2:0 → 4:4:4** (the bulk: +51% on a q85
   fixture; worse on heavily-optimized web JPEGs), and (b) **one duplicate JFIF
   APP0 segment appended per edit** (1→4 headers after 3 edits, never stripped)
   — unbounded per-edit growth; with fat APP segments (EXIF thumbnails/XMP) this
   is the plausible "keeps doubling" mechanism. Size stabilized (~549K) across
   3 successive edits on host and rounds 2–3 *worked*, so the second-edit
   failure may be flatpak/runtime-specific or file-specific. Both defects are
   upstream glycin/image-rs editor bugs (worth reporting); either alone
   disqualifies the `Complete` path for rotation.
3. **Second edit fails / file "stuck"; rotate button reported non-working while
   flip worked.** Not root-caused. The `EDITTEST` hook's single RotateRight *did*
   work on a fresh copy, so the failure is likely on an **already-re-encoded**
   file, or a decode/state race after the first in-place write. (Round-trip test
   also showed edits 3–4 not changing the file — consistent with either a real
   second-edit failure or the hook's fixed 3 s wait timing out on the now-larger
   file.)

*UX decision (user, 2026-07-18 — settled):* **Viewer first, not editor first.**
Edit entry is a **brush button** (gThumb/Loupe pattern) that opens a **card**
(same presentation as Image Properties) holding the basic tools only: crop /
rotate / flip, with save / save-as / undo / redo — "that's it." Never naked
edit buttons in the viewer chrome (the bad placement was the core mistake).
Anything deeper is extension territory (Lua/WASM, §10.3/§10.5).

*REBUILT (2026-07-18, Fable): the edit tier now ships as designed.* Rotate /
flip / **crop** are non-destructive instructions (migrations v3 `orientations` +
v4 `crops`, content-hash keyed, composed/applied on worker threads, `#o`/`#c`
RAM-key suffixes; disk caches always hold as-decoded pixels). Edit card =
Properties-style right slide-in with Transform, Crop (drag-select overlay with
dim + apply/reset), **undo/redo** (per-image session history), and **Save /
Save As**: bake at full resolution via engine `encode_baked` (image crate,
jpg q90 / png — NOT glycin's broken editor); Save = tmp+rename, re-hash,
`rekey_annotations` moves ratings/tags/comments/collections to the new
identity, instructions cleared (they're in the pixels), caches evicted.
Feel/UX verification is the user's (house rule 1).

*Redesign decisions — RESOLVED (shipped above):* chose **non-destructive**
(orientation/crop instructions keyed by content hash, file never rewritten —
matches the reviewer identity). glycin's `Editor` is abandoned for baking; Save /
Save As bake via the engine `encode_baked` (image crate) instead. Remaining
edit-tier ideas (resize/straighten, GPU-accelerated adjustments/filters) are
future work, extension territory (§10.3/§10.5).


**Hard-won process lessons (2026-07-16 — do not relearn).**
1. **A metric is never the acceptance test for *feel*.** LOADTEST worst-stall
   *improved* while the UI got choppier. Engine correctness / crashes / memory are
   headless-verifiable; scroll, hover, pan, fill smoothness are **only** verified by
   the user actually using it.
2. **Interaction-path changes land one at a time, user-verified.** Stacking four
   perf changes made the regression un-bisectable. One change → user feels it →
   keep or revert → next.
3. **Prefer boring over clever on the hot path.** `set_incremental(true)` on the
   sort model was clever and shuffled rows while settling → mid-scroll + hover lag.
   Off-main DB stamping was boring and fine. Pick boring.
4. **Tag a known-good state before experimenting.** `v1.0-stable` exists so a bad
   run costs minutes, not trust.

**Realigned perf priorities (honest, post-learnings).** The real felt costs, in
order — each attempted *one at a time, against the anchor, user-verified*; detail,
evidence, and rejected approaches in §13:
1. **Enrichment must yield to browsing** — background pHash-decode of the whole
   library shouldn't fight the visible thumbnails (the "why Nautilus beats us").
2. **CPU off-main downscale** — replace the GSK `render_texture` downscale with a
   CPU worker (`image` crate); kills the ~1.7 s per-launch Vulkan compile *and* the
   per-large-image main-thread cost in one move.
3. **Warm the cache during indexing** — enrichment already decodes every image;
   have it write the display thumbnail too, so an indexed folder never decodes on
   demand.
4. **Fill metric, then viewport-ordered decode** — make fill order/latency
   *measurable* (§13.3), then decode visible-first like a browser.

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

> **2026-07-21:** the scripting/plugin/helper items above are no longer merely
> deferred — the seam is **decided and phased in §16** (Lua via mlua; WASM
> later; helpers as Flatpak extensions; the Magick window).

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

## 13. Performance — findings & backlog (North Star detail)

Perf is the North Star (see *Status & North Star* at the top). This section holds
what we **proved** on real data and the honest backlog. Everything here is
attempted **one at a time, user-verified against `v1.0-stable`** — a stall metric
is not the acceptance test for feel (lesson 1). None of it blocks; the shipped app
is smooth.

### 13.1 What we proved (2026-07-16, on the user's real 13k-image folders)

- **Enrichment starves the foreground — the primary "why Nautilus beats us."**
  Interactive thumbnail decode and background pHash-enrichment contend for the
  same CPU / glycin pool (which plateaus ~8 concurrent). `run_enrichment` decodes
  flat-out until the backlog drains, never yielding — so a fresh backlog (~11k of
  105k here) saturates decode *while you browse*. Nautilus has no decode-everything
  pass; that's the gap. Measured (LOADTEST worst-stall, 13k folder): baseline
  **723 ms**; throttling enrichment to 1 → **392 ms** (−46%). ⚠ Worst-stall is a
  poor proxy for *fill smoothness* (lesson 1) — we need the fill metric (§13.3).
- **Mixed image size compounds it.** The decode gate is a flat count semaphore; a
  24 MP AVIF and a 400 px JPEG count the same. A burst of large decodes head-of-
  line-blocks the small *visible* thumbnails and spikes RAM. The user's folders
  span ~40× *within one folder* (p50 ≈ 250 KB, max ≈ 8.7 MB; some 12 MP).
- **Per-launch shader compile.** The first GSK `render_texture` (first thumbnail
  downscale) triggers a **~1.7 s Vulkan pipeline compile**, and it is **not cached
  across launches** (radv recompiles per process). Only paid when something
  actually downscales — all-cached browsing never pays it.
- **Folder-open populate** was a synchronous ~533 ms spike (sort + filter + DB
  stamp of 13k at once). NOT the scanner — `classify` skips unchanged files by
  `(size, mtime)`, so back-nav is stat-only and never re-hashes.
- **⭐ ROOT CAUSE of the large-folder freeze — unbounded decode-future spawn
  (proved 2026-07-16 via VITRINE_SOAK + VITRINE_NOCACHE + VITRINE_DEBUG).** The
  load path spawns **one decode future per cell-load / prefetch** with no bound on
  outstanding count; each awaits the 8-wide decode gate. Warm cache → they resolve
  instantly (hit) and never pile up (why cached browsing is smooth). But **cold /
  cache-miss-heavy folders → thousands pile up waiting on the gate**, and
  coordinating them on the single main thread saturates it. The HUD caught it dead
  to rights: `decode[live=3060]` (3k futures parked on an 8-slot gate), **`fps=0`,
  `frame_max=2910 ms`** (a 2.9 s frame), RSS 167→491 MB. Decodes finish ~150/s but
  spawn far faster → the backlog never drains. This is "still lagging on large
  directories." The `queued` (pending debounce) queue balloons the same way
  (→16k). **Fix = the bounded scheduler (§13.2 item 4, now #1): a capped,
  viewport-priority queue drained by a fixed worker pool — cap `live` at dozens,
  not thousands.** Re-run the same SOAK/NOCACHE capture to verify `live` bounded /
  `fps` up.
- **⭐ SOLVED (2026-07-17): #6's cold-RSS mystery was glibc malloc-arena
  retention — fixed with one `mallopt` at startup.** A categorized-smaps soak
  (mixed-size synthetic fixture, VITRINE_SOAK + NOCACHE) showed the retained
  memory living in ~60 **64 MB per-thread glibc arenas** (444 MB resident after
  decodes stopped) while the true full-res buffers were direct-mmap'd and freed
  within seconds. Mechanism: glibc *dynamically raises* its mmap threshold (to
  32 MB) after the first large frees, after which 15–17 MB image buffers land in
  arenas whose freed pages persist at high-water. The earlier mimalloc
  null-result was a **false negative**: `#[global_allocator]` swaps only *Rust*
  allocations — these buffers are `g_malloc`'d by GDK/GLib (C); `malloc_trim`'s
  partial reclaim (2000→1433 MB) was the true positive all along. Fix:
  `mallopt(M_MMAP_THRESHOLD, 1 MB)` first thing in `main()` — buffers ≥1 MB are
  always direct-mmap'd and returned to the OS on free. A/B on the same soak:
  **end RSS 675 → 256 MB, peak 714 → 409 MB, fps unchanged.** No heaptrack pass
  needed; the parked "#6 RSS" thread is closed.
  **Verified on real data (2026-07-18, 15 real dirs / ~840 cold decodes, caches
  wiped):** cold peak 747 MB → settles 380 MB; warm run peaks 388 MB; smaps shows
  **arena-resident RSS = 0** at settle in both runs. Two follow-on findings from
  the same soak pair:
  - **Scroll-time disk-cache warming works (correcting a 2026-07-18 report that
    it didn't):** 809 thumbnails landed on disk during the cold scroll (~96% of
    decodes — `store()`'s MAX_PENDING=16 cap dropped almost nothing at real
    decode rates), and a fresh-process re-run served **100% from disk, 0
    decodes**. The earlier "nothing durable landed" read came from
    `cache_hit=0%` on lap-2 revisits — but those are served from the RAM cache
    *upstream* of `load()`, so the hit counter never sees them; it says nothing
    about disk writes.
  - **The remaining ≥50 ms hitches are NOT cold decode.** The warm run (zero
    decodes) shows the *identical* stall profile to cold (47 samples ≥50 ms,
    30 of them within 2.5 s of a folder-open, worst ~150 ms). They're
    folder-open populate + view transitions — the §13.1 populate spike, much
    reduced but still the top smoothness item. Cold decode itself is smooth
    (that's #6 doing its job); its remaining cost is pop-in *latency*, which #3
    already removes for indexed folders.
    **→ SOLVED same day:** the per-open stall was `ratings_under`'s
    `LIKE 'folder/%'` — default case-insensitive LIKE can't use the BINARY
    `path` index, so every folder open full-scanned all 143k rows (+ LEFT JOIN)
    on the main thread (~36 ms in the CLI, 50–150 ms in-app). Replaced the
    LIKE-prefix with an index-friendly half-open path range
    (`query::subtree_range`: `[root/, root0)`) in `ratings_under`,
    `paths_under`, and the query builder's `under` scope (2 ms; no wildcard
    escaping needed — `escape_like` deleted). Verified on the same warm
    15-dir soak: **stalls ≥50 ms near folder-open 30 → 0** (total 47 → 3,
    smooth samples 176 → 204/234).

### 13.2 What's worth doing (priority order)

1. **Enrichment yields to browsing.** ✅ **SHIPPED (a4f5dba, re-added be9c80a).**
   `decode::yield_to_foreground()` parks the enrichment driver once per batch
   while interactive decodes are in flight (+150 ms grace); single-writer queue
   intact, admission throttled, not write order. Measured 723→533 ms worst-stall
   at the time. *Not built (low value now):* the opportunistic pHash capture from
   the viewer's full decode — enrichment already hashes at index time, so it
   would only help files viewed before enrichment reaches them.
2. **CPU off-main downscale** (#6). ✅ **SHIPPED (b4d4a06 + faca9e8 copy-free;
   RSS follow-up f67ee25).** `thumbnails::downscale_cpu`: worker-thread download
   + engine resize + small `MemoryTexture`; no GSK `render_texture`, so no
   ~1.7 s Vulkan pipeline compile ever, and the dmabuf-FD/readback dance is
   gone. Cold-browse fps 0–15 → median ~71. Its cold-RSS side effect was the
   §13.1 glibc-arena saga, closed by the mallopt pin.
3. **Warm the cache during indexing.** ✅ **SHIPPED (d8dea14).** Enrichment now
   decodes at the grid's 256px thumbnail size (not a 64px pHash-only frame),
   computes the pHash from that, *and* writes the display thumbnail to the same
   URI-keyed disk cache the grid reads (`thumbnails::warm_cache`, awaited +
   unbounded so no write is dropped by store()'s backlog cap). An indexed folder
   now browses with **zero on-demand decode**. Verified on a 200-image varied
   fixture: warmed browse = 0 decodes / 100% hit / 113fps vs 212 decodes cold;
   enrichment RSS 297MB idle / 557MB peak (no regression).
4. **Viewport-ordered decode** (the "browser trick"). ✅ **SHIPPED (e433c3b, as
   part of the bounded scheduler).** `pop_best_load` drains the capped queue
   nearest-`visible_center`-first, so visible thumbnails decode first and
   radiate outward, respecting the active sort. (§13.3's completion-order
   *metric* — start order vs completion order under cost variance — remains
   open; the ordering itself exists. Never touched the sort model — see the
   `incremental` footgun, §13.4.)
5. **Cost-aware / lane-separated gate.** ✅ **SHIPPED (2026-07-18).** Files ≥2 MiB
   (`VITRINE_HEAVY_BYTES`) must take a permit from a 2-wide heavy lane
   (`VITRINE_HEAVY_LIMIT`, 0 disables) *before* the shared decode gate (fixed
   acquire order — no deadlock), so large decodes can never occupy more than 2 of
   the gate's ~8 slots. A/B on an adversarial cold fixture (twenty 18 MB 24 MP
   files sorted to the top of the viewport + 142 small): small-file fill
   unchanged (viewport priority already had that), but **worst main-loop stall
   1957 ms → 72 ms** — the 8-wide burst of ~96 MB decode buffers was the freeze.
   Trade accepted: background *huge* thumbnails complete later (3.2 s → 6.1 s).
6. **Viewer-open polish.** ✅ **SHIPPED (2026-07-18).** (a) On a cache-miss open,
   the viewer instantly shows the grid's RAM-cached thumbnail (same aspect —
   image sharpens in place when the full decode lands) instead of a blank pane;
   (b) `decode_view` now *enforces* the 4096 `VIEW_MAX` cap via the worker-thread
   CPU downscale (glycin's scale request is best-effort), bounding the
   main-thread GPU upload. Verified (15-real-dir warm soak): placeholder shown
   on 30/30 opens, stalls ≥50 ms near open-viewer 2 → 0, whole-journey stalls
   47 → **1** (70 ms), smooth samples 210/233. Viewer texture LRU stays within
   its 256 MB budget (settle 456 MB total, arena retention still 0).

**§13.2 is complete — all six items shipped (2026-07-18).** What remains below
is deliberately parked: tunables that need *per-host measurement* first (lesson:
no blind changes on the hot path). None currently shows a felt problem:
velocity-adaptive prefetch margins (`PREFETCH_AHEAD/BEHIND`); gate size
(`VITRINE_LOAD_LIMIT`); relevance-aware LRU eviction (protect near-viewport
thumbs); cancel decodes for cells flung far past (admission already bails via
the cell re-check; only the ≤8 in-flight glycin decodes are uncancellable).

### 13.3 Measurement we still need

**User-reported test case (2026-07-17): thumbnail *render order* in the viewport.**
Thumbnails still "pop up randomly / take 3-5s" on a cold folder. Diagnosis: the
scheduler already *starts* them viewport-first (`pop_best_load` picks nearest
`visible_center`, respecting the active sort — e.g. newest-first shows/decodes the
newest visible items first). The randomness is **completion-order variance** (big
12MP images finish after small ones started later), and the 3-5s is **cold decode
latency of a screenful**, not wrong ordering. So the render-order test must
measure *completion* order + latency, not just start order:
- fill-order log `(position, visible_at_completion, ms_since_settle)`;
- assert the first N *completions* are top-of-viewport items within a target ms;
- flag decode-time variance (big-image completions lagging).
The real fix for "instant thumbnails" is §13.2 item 3 (warm cache during indexing)
— it removes the decode wait entirely for indexed folders; ordering only matters
*while* waiting.


The fill log now exists (2026-07-18): `VDBG-FILL ms= bytes=` (per decode),
`VDBG-GRIDFILL ms= pos= center= visible= hit=` (per grid completion),
`VDBG-FILM ms= pos= center=` + `VDBG-FILMBIND ms= pos= hit=` (filmstrip).
**Grid viewport-ordered decode verified with it**: cold 220-image open — first
40 completions all visible cells, median |pos−center| = 10, 219/219 consecutive
decoded completions monotone-outward. **Filmstrip scheduler rewritten the same
day** (was LIFO bind-order with oldest-dropped cap → backwards fill + starved
visible cells; now nearest-centre pop, farthest-dropped cap, dead-entry purge,
and a scroll_to centre hint until the async hadjustment catches up) — see
tests/thumbnail-popin-tests.md F1–F3 for the verified regression cases.

Worst-stall (LOADTEST) can't see fill *order/latency* — the thing most of §13.2
improves, and the reason a bad change once looked "good." Build a **fill-order log**
`(position, visible_at_completion, ms_since_settle)` and a **time-to-visible-
complete** metric (settle → every visible cell has a real texture; the grid
analogue of browser LCP). Run perf tests on a **variable corpus** (generated, *not*
committed — it would bloat the repo): thousands of files, ~40× size spread in
several arrangements (large at top-of-viewport / clustered / sprinkled), JPEG mixed
with slow AVIF/JXL, plus non-image siblings — uniform fixtures hide the bug. Assert:
no head-of-line starvation, bounded RSS, fill-order holds under cost variance.

### 13.4 Rejected / dead ends (do not retry)

- **`set_incremental(true)` on the sort/filter models.** Killed the open-freeze
  *metric* but shuffled rows while settling → mid-scroll + choppy hover. The
  cautionary tale behind lesson 3 (boring > clever on the hot path). Reverted.
- **Shader pre-warm at startup.** Moves the ~1.7 s compile to launch, but it's
  *per-launch* (not cached), so it *forces* the cost onto every launch — including
  all-cached browsing that would never pay it. A regression on the fast path.
  Reverted; superseded by CPU off-main downscale (§13.2 item 2), which removes the
  compile entirely.
- **First grab-hand pan attempt** — see §14.1 (the gesture fought the ScrolledWindow).

### 13.5 Notes

- **DB scale is a non-issue.** 105k files → 51 MB (~500 B/row, indexed
  path/hash/pHash/date); SQLite scales to millions of rows / hundreds of MB. The
  cost of a big library is the one-time enrichment decode pass, never the DB size.
  Index freely.
- **Splash / "building library" warm-up (idea, unbuilt).** A first-run splash
  (like Lightroom/digiKam, or GIMP/Krita covering init) can honestly hide the
  *bounded* launch work — shader compile, DB open, first screenful — but NOT the
  unbounded library hashing (that must background). On-brand for a catalog tool,
  and it *rescues* the pre-warm (a 1.7 s freeze behind an honest splash is fine,
  not a regression). A complement to §13.2, not a substitute. A feel change → verify
  by hand.

References: Chrome fetchpriority / LCP (web.dev/articles/fetch-priority),
react-window overscan, Loupe/glycin sandboxed decode (blogs.gnome.org/sophieh),
gThumb parallel thumbnailers.

## 14. UX backlog: viewer pan + removable-media bookmarks

### 14.1 Click-drag pan in the viewer — BUILT then REVERTED (fix the gesture conflict)

Built (68bf3de), then reverted: the grab-hand drag **jittered against the pan
boundaries**. The `GtkGestureDrag` on the picture *and* the enclosing
`GtkScrolledWindow` both tried to move the image, so at the clamp it stuttered —
the user's "hitting a ceiling and trying to push it through."

**Current state.** The viewer already pans when zoomed past fit via scroll-wheel /
scrollbars (`picture_scroller`). The missing piece is a *non-fighting* grab-hand
gesture.

**Redo — the actual fix.** The bug was the gesture and the scroller both driving
the image. Make exactly one of them move it: either claim the gesture / set
propagation so the ScrolledWindow ignores the drag while our handler drives its
adjustments, or disable the scroller's own kinetic panning during the drag. Then
the original design applies — drive `hadjustment`/`vadjustment` by the drag delta,
clamp to `[lower, upper - page_size]`, grab/grabbing cursor, engage only when
pannable. Optional later: kinetic fling, zoom-to-point-under-cursor. **This is a
feel change → verify the drag by hand, one change, against `v1.0-stable`.**

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

---

## 15. Portal/host path split + Find Duplicates (2026-07-20)

The index holds one file under two names — a directly-granted root path and an
opaque `/run/user/…/doc/…` document-portal path — so counts and duplicate
detection double-count. **Deferred, not critical**: tag counts now count distinct
hashes and drops resolve on content, so nothing daily bleeds from it; the
structural fix (a `host_path` column resolved via the Documents portal) waits.

**Find Duplicates is Experimental (UI/UX).** Its remaining value depends on
presenting each copy's *location* — a real duplicate means the same images stored
in different places, which is a user-storage question the portal doesn't fully
capture. Low priority for now. Full analysis: `Troubleshoot/ISSUES.md` V-19.

> **2026-07-21 update:** doc-ID reuse and portal-trash behaviour both measured
> (V-19 updates in the register); dedup is now scoped to the open folder and
> trash reports results (V-22). The deferral of `host_path` stands, comfortably.

---

## 16. The extension seam — decided (2026-07-21)

§10.3/§10.5 investigated the extension tiers; this section **decides** them, so
a build session starts from a contract instead of a debate. It exists because
the deferrals have been accumulating against it by name: advanced sorts
(§10.3.1), batch ops (§10.3.2), volume-aware dedup scope (ISSUES decision,
2026-07-20), custom similarity metrics (§10.5.1). House rules apply throughout —
especially 2 (engine stays UI-free), 3 (never link C++ — helpers are
subprocesses), 4 (portals-first) and 6 (nothing blocks the main loop).

### 16.1 Decisions (locked)

1. **Scripting engine: Lua via `mlua` (Lua 5.4, vendored).** Rhai is dropped:
   smaller user ecosystem, and its one advantage (pure Rust) is neutralised by
   mlua's vendored build (house rule 5 unaffected). Lua is what ImageMagick
   users already know, which matters because Magick recipes are a first-class
   use case (§16.4).
2. **Plugin engine: WASM via `wasmtime`, deferred** until a compute use case is
   scheduled (auto-tagging or embeddings, §10.5.1). Nothing in the seam may
   preclude it: the host API is defined engine-agnostically so a WASM plugin
   sees the same contract a Lua script does.
3. **Helper binaries ship as Flatpak extensions, never sandbox widenings.** An
   `add-extensions` point `io.github.superuser_miguel.Vitrine.Helper`
   (`subdirectories: true`, no-autodownload), mounted under `/app/helpers/<id>/`.
   ImageMagick is the first helper package (`…Vitrine.Helper.magick` — we build
   it; no freedesktop extension exists for it). ffmpeg later can follow the
   established `org.freedesktop.Platform.ffmpeg-full` pattern instead — its
   first concrete recipe is already known (user, 2026-07-21): **frame capture
   from shorts/video sections as reference images** — a §16.3 batch whose input
   is a video and whose output is stills Vitrine then indexes. Later down the
   line; the packaging work is also shared with the future Video Gallery
   sibling app. Core detects helper presence at startup and lights features up;
   absence degrades to the feature not appearing, never to an error dialog.
4. **Scripts are data, not packages.** Lua scripts live in the app's data dir
   (`~/.var/app/…/data/vitrine/scripts/`), hot-reloaded on change, no restart.
   A script is one file with a declared `manifest` table + functions. WASM
   plugins, when they come, are the opposite: packaged, versioned, capability-
   gated — that split is the whole point of having two tiers.

### 16.2 The host API contract (the actual seam)

One versioned table, `vitrine` (with `vitrine.api_version = 1`), passed into
every script. The sandbox is subtractive: scripts get a fresh environment with
`os`, `io`, `require`, `load`, `dofile` removed — the API table is the world.

**Read side (pure, memoized-friendly):**
- `item` facts as plain tables: `name`, `path`, `size`, `mtime`, `width`,
  `height`, `date_taken`, `camera`, `orientation`, `rating`, `tags`,
  `content_hash` — exactly the §10.3.1 list; nothing that requires I/O at call
  time. Paths are display-honest (`scope_display` semantics available).
- `vitrine.query{ under=, tag=, min_rating=, … }` — a thin veil over
  `vitrine_engine::Query`. **No raw SQL, ever** — the schema is not API.

**Write side (queued, honest):**
- `vitrine.tag(hashes, name, add)`, `vitrine.rate(hashes, n)` — routed through
  the existing `Annotator` queue. Scripts inherit the V-02/V-03 semantics:
  the call returns *accepted*, not *committed*, and the API docs say so.
- No direct file writes. File-producing operations go through §16.3 batch
  declarations, which the *host* executes (progress, cancellation, per-file
  results — the V-22 lesson is host code, not script code).

**Providers (registration, not execution):**
- `vitrine.register_sort{ name=, key=fn(item) }` — §10.3.1 verbatim: key
  function, computed once per item, memoized; comparator stays native.
- `vitrine.register_batch{ name=, params=, run=… }` — §16.3.
- `vitrine.register_filter{ name=, params=, args=fn(params) }` — §16.4.

**Never available:** filesystem, network, subprocess spawning, blocking the
main loop (scripts run on a worker; the key-function path memoizes so sort
stays off the hot comparator), or anything that can hold a DB handle.

### 16.3 Batch operations (mogrify-shaped)

A batch declaration names its params (typed: float/int/enum/bool → the host
renders controls), a **destination policy** (`in_place | suffix | subfolder` —
chosen by the *user* in the run dialog, defaulting to `subfolder`), and an
args-builder `fn(params, file) -> [argv]` for a helper binary. The host:

1. resolves the helper from the extension point (absent → feature hidden);
2. runs per-file subprocesses off-thread with progress + cancel;
3. for `in_place`, snapshots originals via `vitrine-engine::backup` first;
4. reports per-file results honestly (toast totals from *outcomes*, V-22);
5. re-indexes touched files (mtime change → rescan of the affected paths).

### 16.4 The Magick window (the payoff feature)

A parametric edit surface: pick images → open **Process** view → choose a
filter recipe → sliders/knobs appear from its declared params → live preview →
apply to the batch. The design constraints all come from lessons already paid
for:

- **Recipes are Lua filter registrations** (§16.2): a recipe maps params to
  `magick` argv (e.g. `modulate = { brightness=slider(0..200) } → ["-modulate",
  "%d"]`). The host owns the UI; scripts never touch widgets. Shipping a
  starter set (modulate, blur/sharpen, levels, grayscale, watermark, format
  convert) makes the window useful with zero user scripting.
- **Preview on a proxy, never the original.** The preview pipeline runs the
  recipe on a downscaled proxy (~1024px, cached per image) so a slider drag is
  a subsecond subprocess, debounced like the thumbnail scheduler (§13
  discipline: bounded in-flight, latest-wins per image).
- **The edit-tier lesson is law** (Status §, 2026-07-18): a slider changes the
  *preview only*. Committing is an explicit Apply with the §16.3 destination
  policy + backup path. No silent destructive writes, no exceptions.
- **Batch apply = the same recipe over the selection** through §16.3 — one
  progress surface, per-file outcomes, honest totals.

### 16.5 What moves out of core / what stays

**Out (extension territory, stop building in core):** sort orders beyond the
built-ins; rename rules; batch file ops; volume/intent-aware dedup scoping;
custom similarity metrics; aesthetic/quality scoring.

**Stays in core:** the query engine and **global search (V-11)** — search is
infrastructure extensions build *on*, FTS5 when it needs scale; Date Taken
sort (a built-in fact, blocked only on the enrichment callback, V-12);
Find Duplicates' current exact/pHash clustering; everything indexing.

### 16.6 Phases & acceptance

- **E0 — seam freeze.** This section reviewed + merged; `vitrine.api_version`
  semantics written into `docs/` for script authors. *Acceptance: none of
  E1–E3 requires a contract change (additions fine).*
- **E1 — Lua host + sort providers.** mlua embedded (engine-free: the host
  lives in `vitrine-app`, house rule 2); sandboxed env; hot reload;
  `register_sort` wired into the Sort By menu. *Acceptance: a natural-sort
  script orders `img_2 < img_10` on a 10k folder with no measurable stall
  regression (§13 numbers); editing the script re-sorts without restart;
  a script error surfaces as a toast naming the script, never a crash.*
- **E2 — helper extension point + batches.** Manifest `add-extensions`;
  `…Helper.magick` package builds offline (house rule 5); `register_batch` +
  run dialog with destination policy, progress, cancel; backup-first for
  in-place. *Acceptance: a format-convert batch over 100 files runs off-thread,
  reports per-file failures honestly, and a magick-less install simply doesn't
  show the menu item.*
- **E3 — the Magick window.** Process view, param-driven controls, proxy
  preview, Apply via E2 machinery; starter recipe set. *Acceptance: slider drag
  previews in <1s on the proxy; Apply never touches originals without the
  chosen policy + backup; cancelling mid-batch leaves a coherent, reported
  state.*
- **E4 — WASM tier.** Unchanged from §10.5, scheduled only when auto-tagging
  or embeddings is committed to.
