# ROADMAP.md — Vitrine

*The short, living answer to "what are we building, and what comes next?"
`PLAN.md` holds the designs and the rationale; this file holds the goals and
the order. Re-read this before picking the next piece of work. Update it when
a release ships or a decision changes. Last revised 2026-10-06, after v0.3.1 (tagging moved to a design track).*

---

## 1. What Vitrine is for

**Browse images that happen to be files.** Loupe's viewer, Nautilus's grid and
selection model, and a catalog/tag layer keyed by content so that nothing you
record about an image is lost when gallery-dl renames it, a drive is moved, or
a backup is restored.

**North Star (PLAN §Status):** fast, *smooth* browsing of large libraries. Where
Nautilus, Loupe and gThumb get sluggish at tens of thousands of images, Vitrine
stays fluid. "Smooth" is judged by hand, not by a metric.

**What that implies, in order of priority:**

1. **Never lose what the user recorded.** Tags, ratings, comments, collections,
   edits: keyed by content hash, written by one thread, backed up before any
   migration deletes. *Offline is not deleted.*
2. **Never get slower.** Every feature ships only if the grid still feels the
   same. The browse geometry (PLAN §13) is the most expensive code in the app
   to touch, and the most protected.
3. **Stay small in the core; grow at the seam.** Anything that isn't browsing,
   reviewing or organising belongs in an extension (PLAN §16): Lua for policy,
   helpers for heavy tools, WASM later for ML.
4. **Be a good GNOME citizen.** libadwaita, portals, the shared thumbnail
   cache (standard sizes only), no `--filesystem=host`, ever.

**Non-goals (PLAN §0, never build):** web albums, contact sheets, burn-to-CD,
import wizards, screensaver slideshows, hand-rolled Vulkan, in-process `.so`
plugins, Tracker/TinySPARQL, gdk-pixbuf decoding. And: **not Flathub.** Vitrine
ships from its own signed repo and GitHub Releases.

**Not trying to be:** darktable or digiKam. Vitrine is the fast front door to
a library; heavy editing and developing happen in tools it hands off to.

---

## 2. The rules every update obeys

These are the reasons the app still feels right after three months of work.
They don't change between releases.

- **One line of work per release, its own clean build.** A feature release
  carries one theme. Bug fixes go in point releases between them.
- **Small, reversible steps.** Invisible engine work lands first, the UI on
  top of it later, so each step can soak before the next.
- **Don't build on spec.** The Lua host API grows only from things the user
  actually did by hand more than once. Same for rules, kinds, hierarchies.
- **Measure before and after** on the real library (430k rows) with the
  existing harness (`debug-run.sh`, `VITRINE_SOAK`, the A/B scripts).
- **House rules in code:** engine crate UI-free; `forbid(unsafe_code)` there,
  one named exception in the app; one writer thread; `source` separates
  curated from automatic tags; no Claude attribution on commits.
- **Distribution:** signed commits and tags; `release.sh check` green; hosted
  repo first, bundle second; a release is done when the public URL verifies.

---

## 3. Where we are

| Release | Date | What it was |
|---|---|---|
| 0.1.0 | 2026-07-18 | First release: browse, view, index, review, collections, duplicates, non-destructive edits. |
| 0.2.0 | 2026-07-21 | Correctness and responsiveness; per-image tags; Files → viewport drag. |
| *soak* | 07-22 → 10-05 | Fuzzy find, histogram, removable + content-hash thumbnail tiers, Lua sorts (E1), bulk tag removal, V-25/26/28/29. |
| 0.3.0 | 2026-10-05 | All of the above shipped; the signed hosted repo; site refresh; CI fixed. |
| 0.3.1 | 2026-10-06 | Private thumbnail cache (V-30), cache rows purged from the index (V-31), Clean Up Thumbnails, V-05, remembered icon size, `release.sh`, `run-dev.sh`. |

**Known debt (code review, 2026-10-06; full list in the vault note):**
the engine can't express "unreachable", so 63k offline/portal files are stuck
at a 0×0 enrichment; three Save paths can quietly degrade data; thumbnail
invalidation is by mtime ordering rather than identity; several main-thread
stalls at library scale. See §5 for where each gets paid.

---

## 4. The next ten feature updates

Each row is one theme, one clean build. Point releases (0.x.y) between them
carry bug fixes from the review and from use. Order is a proposal; the owner
decides when each starts. Nothing below is scheduled until it is.

| # | Version | Theme | What ships | Depends on | Goal it serves |
|---|---|---|---|---|---|
| 1 | **0.4** | **The index knows "offline"** | An explicit unreachable state instead of the 0×0 sentinel; `Reappeared` instead of re-hash; mount-point-aware reconcile; a one-time retry of the stuck files. The invalidation set (read `Thumb::MTime`; mtime in RAM/content keys). Navigation lifecycle fixes ride along. | — | 1. Structural debt that sidecars and rules would otherwise build on. |
| 2 | **0.5** | **Your downloads know who made them (read-only)** | gallery-dl sidecar ingest as one more enricher: `sidecar_meta` (creator, site, URL, posted) + `sidecar_tags` in their own tables; a read-only **Source** section in Properties; creator/site predicates for smart collections. *No promotion into curated tags yet: that waits for the tagging redesign.* | 0.4 (offline sidecars are safe); the tag-flood decision (Phase 1 data favours separate tables). | 1 and 3: metadata arrives without touching the curated tags. |
| 3 | **0.6** | **E2: helpers** | The add-extension point (PLAN §16.2/16.3): external tools as Flatpak extensions published through `vitrine-repo`; a run dialog with progress and cancel; `register_batch` / `register_filter`. First helper: ffmpeg frame capture. | The packaging decision (extension via the hosted repo vs. a host tool on `$PATH`). | 3. The seam's second tenant, and E3's prerequisite. |
| 4 | **0.7** | **E3: colour tools** | gThumb's set (Enhance, Adjust, Equalize, levels) as ImageMagick recipes on E2's machinery, in a Process view, non-destructive, with before/after (PLAN §16.4). Possibly `darktable-cli` develop batches. | 0.6. | The feature wanted since July, built the way it was decided: its own build, on top of E2. |
| 5 | **0.8** | **Find anything** | The other half of V-11: structured search over the `Query` the engine already has (tag, comment, rating, date, camera, creator), saved searches as smart collections, tags in fuzzy find. Built against the *current* tag model; adopts the redesigned one later. | 0.5 (creator predicate). | The index is the product; this is how it pays off at 288k files. |
| 6 | **0.9** | **Tagging, redesigned** (design track; slot provisional) | The outcome of the tagging UI/UX rethink (§6): how tags are applied during a migration, where they're visible, whether people/sites/descriptive are different kinds, how sidecar creators are promoted. Includes Manage Tags, the popover, the filter, and whatever the sidebar needs, designed together. **Design first, then one release.** | The design pass (owner-led); 0.5's sidecar data to design against. | 1, and the first organising tool at library scale. |
| 7 | **1.0** | **The 1.0** | A stability milestone, not a feature: every review finding closed; `window.rs` split (bookmarks, duplicates page, dev aids, a testable `LoadQueue`); CI with the Meson/metainfo job and the vendored-crate check; screenshots retaken; docs current; the two accepted `allow(dead_code)`s gone. **If the sidebar/folder-navigation rethink happens, it belongs here or with 0.9's tagging redesign, since both reshape the sidebar; one deliberate release, not two accidental ones.** | 1–6 shipped and soaked. | 2 (never slower) made auditable. |
| 8 | **1.1** | **Rules** | The Lua policy layer over the sidecar mirror: auto-promotion rules writing `source='rule'` tags, revocable in bulk by source; creator normalisation across sites; sidecar-aware sort keys. Specified by the hand-promotions actually made after 0.9, not on spec. | 0.9, and enough hand-promotions to know what the rules are. | 3. The E1 seam used for what it was designed for. |
| 9 | **1.2** | **Interop** | XMP both ways: write `lr:hierarchicalSubject` if a tag convention exists, read `.xmp` back as `source='xmp'` with a conflict rule; embedded metadata write via rexiv2 (activates `sync_state`, PLAN §9); the JSON backup wired into the UI. | 0.9's tag model; a conflict rule the owner chooses. | 1 and 4: your data is readable by darktable, digiKam and Files. |
| 10 | **1.3** | **Views** | Post-v1 navigation (PLAN §12): the address bar / breadcrumb first (small), then tabs (`AdwTabView`, the big architectural change: one window, several grid/store/filter/history sets). Viewer drag-pan redone (PLAN §14.1). | 1.0's `window.rs` split, so a second grid can exist. | 2, carefully: the only release in the ten that reshapes the browse view, and the last on purpose. |

**Beyond ten:** E4, the WASM tier (PLAN §10.5/16.6), only when auto-tagging or
face grouping is actually wanted; the GPU edit tier; device import. The
**Video Gallery** is a sister app, not a Vitrine release; 0.7's ffmpeg helper
is the only overlap.

---

## 5. Where the debt gets paid

| Review finding (vault note, 2026-10-06) | Release |
|---|---|
| Save in place moves annotations; Save writes PNG under other extensions; Save As onto the source | **0.3.2** (point release, now) |
| Phantom hash from a text drop; stuck "Indexing…" banner; Lua guard held across a call; cheap perf wins (collection counts, N+1 ratings, partial index, `rating_min`) | **0.3.2** |
| Enrichment sentinel for unreachable files + retry of the 63k rows; `Reappeared`; mount-point reconcile | **0.4** |
| `Thumb::MTime` read back; mtime in RAM/content keys | **0.4** |
| Folder-load generation token; viewer pop on every navigation; tag-filter reset | **0.4** |
| Tag-operation journal + Undo on the toast (engine + one button; commits to no tagging UI) | **0.3.x, if the owner confirms** |
| `window.rs` extractions; CI jobs; test gaps; dead engine API | **1.0** (the engine tag calls with 0.9) |

---

## 6. Parked, and why

- **Tagging as it is.** Decided 2026-10-06: the tagging system as a whole
  (popover, sidebar chips, filter dropdown, discovery) needs a UI/UX rethink,
  not incremental fixes, so the incremental plan from the decision page is
  withdrawn. It becomes a design track with a provisional slot at 0.9. The
  decision page remains the inventory of surfaces, numbers and questions.
- **Folder navigation (the folders band) and a sidebar Tags section.** Deferred
  by the owner 2026-08-01 and again 2026-10-05: "it's huge, and right now I like
  what we have." Both reshape the browse view, as the tagging redesign will;
  revisit together, only when the owner raises it; home is 0.9 or 1.0.
- **Hierarchical tags / real tag kinds.** Only if the `person:` / `site:`
  prefix convention from 0.4 turns out too weak.
- **Sidecar tags into `file_tags`.** Rejected by the Phase 1 data: it would
  double the vocabulary with promo hashtags. Separate tables, promote by hand.
- **The download-ledger DB, the tagging of pre-sidecar TikToks.** gallery-dl
  side projects; not Vitrine releases.

---

## 7. Open decisions (the owner's)

1. **Does the tag-operation journal + Undo ship early (0.3.x)?** It's engine
   work plus one toast button and commits to no tagging UI; it's also the one
   thing that already cost an afternoon.
2. **The tagging design pass:** when, and what it must answer (see 0.9's row).
3. **E2 packaging (before 0.7):** helpers as extensions through `vitrine-repo`,
   or a host tool on `$PATH`.
4. **XMP conflict rule (before 1.2):** Vitrine wins, file wins, or newest wins.
5. **When, if ever, the browse view changes** (1.0 or 1.3).

---

## 8. How to use this file

- Starting a session: read §1, §3, then the first unshipped row of §4.
- Shipping a release: move its row into §3, bump the status note in `PLAN.md`,
  update the hub note in the vault.
- Changing a decision: edit §7 and the affected row, in the same commit as the
  change, with a one-line "why".
- Tempted by something not in §4: write it in `Vitrine_Concepts.md` first.
  If it survives a week, give it a row here.
