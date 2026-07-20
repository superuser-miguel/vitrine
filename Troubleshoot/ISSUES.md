# Vitrine — consolidated issue register

Derived from `vitrine-debug.log` (run of 2026-07-20 01:36), `Observations.md`, and
direct code/DB verification on 2026-07-20.

**Evidence key**
- `MEASURED` — reproduced with numbers against the live 92MB index.
- `CONFIRMED` — read directly in the source; mechanism is unambiguous.
- `OBSERVED` — reported from use, not yet reproduced under instrumentation.
- `UNVERIFIED` — asserted in Observations.md, not yet checked.

---

## Tier 0 — Data loss. Fix before anything else.

### V-01 · Delete in a collection view trashes the real file · `CONFIRMED` · **FIXED**

> **Fixed 2026-07-20.** `Request::RemoveFromCatalog` → `Annotator::remove_from_catalog`
> → worker arm; `Key::Delete` now routes through `delete_or_remove_selection()`,
> which removes from the catalog when browsing one and trashes otherwise. Added a
> **Remove Selection** entry to the collection menu, and relabelled its `Delete` to
> **Delete Collection** (it always deleted the whole collection — the label was
> half the trap). Smart collections are excluded: membership is query-derived, so
> there is nothing to remove from. Regression test:
> `collections::tests::removing_from_a_catalog_keeps_the_file_and_its_annotations`.


`window.rs:463` binds `Key::Delete` → `trash_selected()` with no context check.
`trash_selected()` (`window.rs:2492`) calls `item.file().trash_async(...)` on every
selected item regardless of whether the grid is showing a folder or a collection.

The collection context menu (`show_collection_menu`) offers only **Add Selection**
and **Delete** — where "Delete" deletes the *whole collection*, not a member.

There is therefore **no gesture that removes one image from a collection**. A user
trying to curate a collection reaches for Delete and trashes originals.

The engine already solves this and is not plumbed in:

```
crates/vitrine-engine/src/collections.rs:162
    pub fn remove_from_catalog(&self, id: i64, hashes: &[String]) -> rusqlite::Result<()>
crates/vitrine-engine/src/collections.rs:281
    (unit test — passing)
```

Missing links: `Request::RemoveFromCatalog` variant → `Annotator::remove_from_catalog`
→ worker arm → UI (Delete key branches on collection context; context-menu entry).

Mitigating: `trash_async` is reversible (Trash, never unlink). Still data loss from
the user's point of view, and silent.

---

## Tier 1 — Silent failure. The app asserts things it cannot know.

### V-02 · Success toasts fire unconditionally · `CONFIRMED` · **PARTLY FIXED**

> **2026-07-20.** Tag apply, catalog drop, catalog add and catalog remove now toast
> based on whether the writer *accepted* the write, and say so plainly when it did
> not. Note the honest limit: accepted means **queued, not committed**. A write can
> still be accepted and then land minutes later behind a scan (V-04). Closing that
> gap needs a reply channel from the worker — deliberately not built yet.


Both write paths put the toast *outside* the guard:

```rust
if let Some(indexer) = self.imp().indexer.borrow().as_ref() {
    indexer.annotator().tag(name, &hashes, true);
}
self.toast("Tagged {n} images");        // ← always runs
```

Same shape in the catalog drop handler (`window.rs:2242+`). This is the direct
cause of *"claims to have added them."*

Note: `imp.indexer` is set once (`window.rs:2749`) and never cleared, so the
`if let` rarely fails. The layer that actually matters is V-03.

### V-03 · Every annotator write discards its result · `CONFIRMED` · **FIXED**

> **Fixed 2026-07-20.** All 12 sends now route through `Annotator::send()`, which
> returns acceptance, warns via `g_warning!` on failure, and emits `VDBG-WRITE`
> carrying `op`, `rows`, `queued` and `accepted`. `queued` is the head-of-line
> signal for V-04; `accepted=false` means the writer thread is gone. **This is the
> instrument that tells blocked and dead apart** — the next log will say which.


12 occurrences of `let _ = self.requests.try_send(...)` in `index.rs`. No write
can report failure to the UI.

The channel is **unbounded**, so `try_send` can only fail when the receiver is
dropped — i.e. the worker thread is gone. That makes these discarded `Result`s an
exact, zero-cost liveness detector, currently thrown away.

### V-17 · Rubber-band selection steals the press from item drags · `CONFIRMED` · **FIXED — verified in use** (drag starts 2 → 18)

`window.rs:438` sets `set_enable_rubberband(true)`. The cell's `DragSource` was on
the default bubble phase, so the two gestures raced for the press and rubber-band
usually won — dragging a thumbnail drew a selection rectangle instead.

Probe evidence, session of 2026-07-20 12:37: across a whole session of the user
fighting with drag, `VDBG-DRAG` fired **twice**. Both of those had `hash=true` and
both completed (`VDBG-DROP items=22`, `items=6`). The drop path is healthy; drags
simply almost never *start*.

> **2026-07-20.** Drag source moved to `PropagationPhase::Capture` so the cell sees
> the press first. A drag gesture only claims once the drag threshold is crossed,
> so click-to-select still falls through.
>
> **This is a hypothesis, not a measurement** — GTK gesture arbitration can't be
> driven headlessly. Verify by the `VDBG-DRAG` count: many lines = fixed, still
> ~2 = not. Fallback if it fails: disable rubber-band, which makes drag reliable
> at the cost of rubber-band multi-select. That is a UX trade for the user to make.

### V-04 · Single writer thread; large scans block user writes · `CONFIRMED` mechanism — **NOT the reported symptom**

> **2026-07-20 logs rule this out as the cause of "tagging claims to have added
> them".** Every write across both runs: `accepted=true`, `queued=0`. The writer
> was alive and never backlogged, and the tags were confirmed present in the live
> DB (tag count 7 → 9, "Crowns"/"gold" both written). The real cause was V-06 —
> the menu froze for 3+ seconds around the action, which read as failure.
>
> The head-of-line mechanism is still real and still worth fixing eventually; it
> simply was not what the user was hitting. Left open, deprioritised.


`worker()` serializes `Scan`, `Enrich`, and all user writes through one
`recv_blocking` loop. `Request::Scan` covers a whole tree walk plus a blake3 hash
of every changed file — one request, processed atomically. On a 192,860-file index
that occupies the worker for a long time, and tags/drops queue behind it while the
UI toasts success instantly.

Ruled out: enrichment. Batches are capped at 64 (`ENRICH_BATCH`) and awaited before
the next `TakeBatch`, so that backlog stays bounded.

Not yet distinguished: **blocked** vs **dead** worker. Different fixes. V-03 settles it.

### V-05 · Drag silently refuses when `content_hash` is empty · `CONFIRMED`

`grid_cell.rs:156` returns `None` from `connect_prepare` when the hash is empty —
the drag simply never starts, with no feedback.

Hashes are stamped by `stamp_annotations`, which early-returns when `current_folder`
is `None` and only covers that folder's subtree. Items can legitimately carry no hash.

Interaction note: on this path *tagging* shows "Select one or more indexed images to
tag" — a different message than the reported symptom. So V-05 likely explains the
drag half of "stops working after a while" but **not** the tagging half. Those may
be two causes, not one.

---

## Tier 2 — Performance. Measured.

### V-06 · `all_tags()` full-scans the files table, once per tag · `MEASURED` · **FIXED**

> **Fixed 2026-07-20.** Migration **v5** adds `idx_file_tags_tag ON file_tags(tag_id)`.
> Applies to a live 92MB index in <0.01s, `integrity_check` clean, plan flips
> `SCAN f` → `SEARCH ft`.
>
> Linear scaling in tag count confirmed against the live DB — cost is tags × files,
> so **every tag the user added slowed the menu for the whole library**:
>
> | tags | query | in-app stall |
> |------|-------|--------------|
> | 7    | 0.41s | ~2.5s        |
> | 9    | 0.54s | ~3.4s        |
>
> (0.54s measured against 0.53s predicted from the 7-tag figure.) Tests
> `tags::tests::tag_counts_are_indexed_not_a_table_scan` asserts the query *plan*,
> so a regression fails the build rather than quietly returning.


`tags.rs:31` uses a correlated subquery. Against the live index SQLite picks:

```
SCAN t USING COVERING INDEX sqlite_autoindex_tags_1
`--CORRELATED SCALAR SUBQUERY 1
   |--SCAN f                     ← full scan, 192,860 rows
   `--SEARCH ft USING ... (content_hash=? AND tag_id=?)
```

`file_tags` PK is `(content_hash, tag_id)`; filtering on `tag_id` alone can't use it
(second column), so SQLite inverts the loop. Cost is tags × files.

Runs **synchronously on the GTK main thread** — `window.rs:1087` (popover
`connect_show`) and `window.rs:1179` (`refresh_tag_filter`).

Log evidence: stalls of 2514 / 2541 / 2525 / 2543 ms in complete probe silence
(decode `+0/s`, `queued=0`, RSS flat), growing to ~4.1s later in the session.

Fix, verified on a copy of the live DB:

```sql
CREATE INDEX idx_file_tags_tag ON file_tags(tag_id);
```

→ plan flips `SCAN f` to `SEARCH ft`; **0.41s → under 10ms**; results byte-identical.
Also fixes `hashes_with_tag()` (the tag *filter* path), likewise a `SCAN`.

Caveat: 0.41s is a warm-page-cache CLI number — a lower bound. It does not by itself
account for the observed 2.5s. Remainder is likely cold pages plus the query firing
twice per interaction. Needs a probe to close.

### V-07 · `refresh_tag_filter` computes counts, then discards them · `CONFIRMED` · **FIXED**

> **Fixed 2026-07-20.** New `Annotator::tag_names()` (`SELECT name FROM tags`)
> replaces the `all_tags()` call in `refresh_tag_filter`.


`window.rs:1179` calls `all_tags()` — paying the full V-06 subquery — then maps to
`t.name` and drops every count. Wants a plain `SELECT name FROM tags ORDER BY name`.

---

## Tier 3 — UI/UX. Observed in use.

### V-18 · No gesture clears a selection · `CONFIRMED` · **FIXED (untested)**

`GtkGridView` only clears a selection when another item is picked, so a
multi-selection could only be undone by selecting a single image — there was no
"select nothing" gesture at all. Added two: a click on empty grid background
(hit-tested with `pick()` against `VitrineGridCell`, so item clicks are
unaffected) and Escape.

### V-19 · Portal document paths split the index in two · `MEASURED` · **DEFERRED (not critical)** — see the decision at the end of this file

The index holds the same content under two unrelated path families:

| path prefix | files |
|-------------|-------|
| `/home/definitive_group/Pictures/…` | 78,817 |
| `/run/user/1000/doc/…` (portal documents) | ~114,000 |

17,050 hashes appear under **both**. Folders opened through the file-chooser
portal are indexed under opaque per-session document paths; the same file's real
path is a different string, so anything matching on path misses.

This is what made V-08 return `items=0`. Content-hash resolution works around it
there, and hashing is what keeps tags stable across this split — but every future
path-matching feature will hit it.

**Why the content hash is not itself the fix.** Annotations key on
`content_hash` and are correct already — that layer has no bug. But `files.path`
is `UNIQUE`, so `files` is keyed by *path*: one image reached two ways is two
rows, and anything counting rows double-counts. The hash says *the bytes are
identical*; it cannot say whether that is one file seen twice or two real copies
— and telling those apart is exactly what the Duplicates feature exists for. So
`files` cannot simply be collapsed on `content_hash`. Two questions, two keys:
`content_hash` answers "same content?", a resolved host path answers "same file?".

**Partial fix 2026-07-20:** `all_tags()` now counts `DISTINCT f.content_hash`, so
tag counts stopped double-counting (Ashley Trevort 15 → 7). Free, and no schema
change. It does **not** touch the row duplication: 41,727 hashes still hold
multiple present rows, so the Duplicates feature still reports them.

**Do not run dedup on this library yet.** The dedup card offers "Trash the other
copy". For a portal/real pair those may be the same bytes on disk, so trashing
the "copy" could delete the file the kept row points at. Unverified — the FUSE
mount is not statable from outside the sandbox — but it is the same shape as
V-01 and should be checked before that feature is used at scale.

**Open decision.** `--filesystem=home` was considered and **rejected**: portals-first
is how a Flatpak should behave, and it matters more once helper binaries
(ImageMagick, ffmpeg) ship as runtime extensions — a wide home grant would extend
to them. The remaining proposal is a `host_path` column resolved via
`org.freedesktop.portal.Documents.Info()` (`ashpd` is already earmarked in
`vitrine-app/Cargo.toml` for the extra-roots chooser), unique so upsert dedupes,
plus a migration collapsing the existing 17,050 pairs. `path` keeps its current
meaning — the path the sandbox can actually open — so nothing breaks. Bonus: the
Properties card could show a real folder instead of `/run/user/1000/doc/…`.

### V-21 · Grid selection is louder than the images · `CONFIRMED` · **FIXED (untested)**

There was no selection styling at all, so Adwaita's default applied: a selected
`gridview > child` gets the **solid** accent background. Across a multi-selection
adjacent cells abut and merge into one saturated slab with the thumbnails sitting
inside it.

> **Fixed 2026-07-20.** A file-manager-weight wash instead: `alpha(@accent_bg_color,
> 0.25)`, `border-radius: 9px` (clears the cell's own 4px margin so the tint hugs
> the thumbnail rather than the grid track — this is what stops cells merging),
> and `color: inherit` so filenames keep their normal label colour. Focus keeps a
> distinct ring, since it must stay findable inside a selection. Hover left to
> Adwaita.
>
> CSS cannot be verified headlessly — GTK parses it at runtime and skips
> malformed rules with a `Gtk-WARNING`. If the slab persists, check the log for one.

### V-20 · A collection view doesn't refresh when it gains members · `CONFIRMED` · **FIXED (untested)**

Dropping onto the catalog you are currently viewing wrote to the index but left
the grid alone — the images only appeared after navigating away and back. A
collection view is a snapshot built by `open_collection()`; `CollectionsChanged`
only refreshed the **sidebar list**, never the grid.

> **Fixed 2026-07-20.** `CollectionsChanged` now carries `gained: Option<i64>`,
> set only by `AddToCatalog`. If the catalog that gained members is the one on
> screen, the grid reloads.
>
> Removals deliberately do *not* set it: `remove_selection_from_catalog` already
> drops those rows from the grid, so reloading would re-query the whole collection
> to learn what the UI knows — and discard the scroll position doing it. Dropping
> into catalog B while viewing catalog A correctly leaves A alone.

### V-08 · Nautilus → Collection never wired · `CONFIRMED` · **FIXED**

> **2026-07-20, two rounds.** First fix declared both `FileList` and `String` on
> the catalog row, so external drops reached the handler. The probe then showed
> `VDBG-DROP payload=files items=0` — accepted, but nothing resolved, because
> resolution matched on **path** and the dropped real path does not match the
> portal path the file was indexed under (see V-19).
>
> Second fix resolves on **content hash**: path lookup first, and any miss falls
> back to hashing the file and checking whether the index holds that content under
> another path. Whole-file I/O, so it runs off the main thread via
> `spawn_blocking` — a large drop cannot freeze the UI.
>
> Verified against the live index on the exact failing case
> (`~/Inkscape_Projects/Screenshot from 2024-12-20 10-39-44.svg`, indexed only
> under `/run/user/1000/doc/…`): `path_hit=false`, hash `55ca8ba74b42…` matches
> the portal row, `present_rows=1`. Path-only resolution returned nothing;
> hash resolution finds it.


`window.rs:2242`: `gtk::DropTarget::new(String::static_type(), COPY)` — accepts
`String` only. Nautilus delivers `GdkFileList` / `text/uri-list`, so the type never
matches and the handler is never called.

The "+" cursor is not the collection accepting; it's almost certainly the
`places_scroller` FileList target (`window.rs:1323`) underneath, which accepts folder
drops for bookmarking.

Not a bug — unimplemented. Needs: accept `FileList`, resolve path → `content_hash`
via the read DB (**no such query exists yet**), then `add_to_catalog`.

### V-09 · No remove-tag flow · `CONFIRMED` · **FIXED — verified in use** (`VDBG-TAG op=remove name="crystal"`)

> **2026-07-20.** Landed as a **Tags group in the viewer's properties card**
> rather than a modifier-click in the grid popover, which draws a cleaner split:
> the popover tags a **selection** (bulk), the card shows **one image** — the
> context where removal is the obvious affordance. An "Add a tag" entry row plus
> a chip cloud; each chip is a pill with an × that removes that tag.
>
> Cheap because the viewer already held `annotator` and `read_db` for the
> rating/comment rows. Chips update **optimistically** — writes queue on the
> writer thread, so re-reading the index immediately would show the pre-edit
> state and flicker. If the writer is gone the chip is not added at all.


`Annotator::tag(name, hashes, add)` already takes an `add: bool` and the worker
handles `false` → `db.remove_tag(...)`. The UI only ever passes `true`
(`apply_tag_to_selection` hardcodes it). Backend is done; only UI is missing.

### V-10 · Tagging / DnD "go stale after a folder change" · `OBSERVED`, cause not confirmed

May be fully explained by V-04 + V-05 rather than being a distinct defect. Do not
fix speculatively — re-test once V-03 instrumentation lands.

### V-11 · No global search · `OBSERVED`

Only rating + single-tag filter today. Wants filename / path / tag / comment / EXIF.
Engine has a `Query` struct to extend. FTS5 if it needs to scale.

### V-12 · Sort lacks Date Taken and Rating · `OBSERVED` · **Rating FIXED; Date Taken deferred**

> **2026-07-20.** Rating sort added — `rating()` was already on `ImageObject` and
> already stamped at folder-open, so it cost one match arm and one menu entry.
> Note it sorts **highest-first on the Ascending setting**: "sort by rating" means
> the best work at the top, and nobody wants unrated images first.
>
> **Date Taken deliberately not done.** It looks like a peer of Rating but isn't:
> `date_taken` is populated by background *enrichment*, so on a large library the
> column is mostly NULL until enrichment catches up and the sort would silently
> produce near-random order. The groundwork was anticipated —
> `Indexer::start_enrichment(on_done)` is documented as "used to refresh a
> metadata sort once the columns it reads are populated" — and both call sites
> currently pass `|| {}`. Doing it properly means stamping `date_taken` onto
> `ImageObject`, extending `ratings_under`, and wiring that callback to re-sort.

`date_taken` is already indexed (`idx_files_date`), so Date Taken is close to free.

### V-13 · No Clear / Home button · `OBSERVED` · **FIXED (untested)**

> **2026-07-20.** `home_button` (`go-home-symbolic`) beside Back/Forward,
> insensitive until something is open. There was no route back to "No Folder
> Open" short of restarting — every gesture in the app moves *between* locations.
> `clear_viewport()` also clears history: leaving Back armed to re-enter the
> folder you just closed makes the close read as not having worked.

No way back to the initial "No Folder Open" state without restarting.

### V-14 · Collection polish · `OBSERVED`

Manual reorder within a collection; bulk remove (blocked on V-01); collection
thumbnails in the sidebar.

---

## Tier 4 — Open questions.

### V-15 · `Adwaita-CRITICAL: Page 'Viewer' is not in the navigation stack` · `CONFIRMED` · **FIXED — verified in use** (0 CRITICAL across 4 runs)

> **Root cause found 2026-07-20.** `window.rs:2727` branched on
> `nav_view.find_page("viewer")`, but `find_page()` also resolves pages the view
> merely holds a reference to — not only those on the stack. After the viewer was
> popped (which `load_folder` does deliberately when a folder is opened from the
> viewer), `find_page` still found it, so the `else` branch fired `pop_to_tag()`
> for a page that was not there. That is why the CRITICAL clustered around
> folder switching.
>
> Replaced with `nav_stack_contains()`, which walks the actual navigation stack —
> the only thing `pop_to_tag()` consults. The Duplicates page carried the
> identical latent bug and was fixed with it.


Recurs across several runs (23:00–23:11 cluster). Navigation state is getting out of
sync with the nav view. Unrelated to the above as far as I can tell, but it is a
real state bug and worth its own look — nav-state corruption is a plausible
contributor to "after a while, things stop behaving."

### V-16 · The instrumentation cannot see any of Tier 0–1 · `CONFIRMED` · **FIXED**

> **Fixed 2026-07-20.** Four probes added — `VDBG-WRITE`, `VDBG-TAG`, `VDBG-DROP`,
> `VDBG-DRAG` — plus `build-aux/debug-run.sh --interact`, which suppresses the
> fill probes (63,065 `FILMBIND` lines last run) so interaction is readable. The
> run summary now reports dropped writes, peak writer queue, drags refused for a
> missing hash, and drops that resolved nothing, and warns explicitly on a dropped
> write or a queue over 100.


Probe inventory across all 92,375 log lines:

```
VDBG-FILMBIND  63065     VDBG           3552
VDBG-FILL      11767     VDBG-FILM      2296
VDBG-GRIDFILL   9648     VDBG-VIEWER    2038
```

All frame/decode shaped. Zero probes on drag, drop, tag, or DB. `grep -i
"tag\|drag\|drop"` returns nothing. These bugs are invisible by construction — no
amount of re-running the current build will surface them.

Two metric corrections for future reading:
- **`frame_max` is misleading.** The 22,490ms value at line 5142 has `stall=3ms` —
  that is idle gap between redraws, not lag. Read `stall`.
- **`cache_hit=0%` is not a bug.** Those runs used `--cold`, which sets
  `VITRINE_NOCACHE=1` and skips cache reads by design.

---

## Standing design constraint

Keep the Rust core lean; push custom logic (advanced sorts, batch ops) to an
extension layer rather than growing core. This argues against absorbing V-11/V-12
wholesale into `vitrine-engine` — worth deciding the seam before building them.

---

## Decision — 2026-07-20: V-19 deferred, Find Duplicates marked Experimental

**Status: V-19 is no longer critical. Relaxed by agreement.** Recorded verbatim
so the reasoning survives, including the parts still outstanding.

### Assistant position

I agree with you, with one split.

You're right that V-19's structural fix can wait. Nothing daily depends on it now
— tag counts are correct, drops resolve, browsing is fine. A day of ashpd +
host_path + migration buys correctness we're not currently bleeding from. Defer
it.

You're also right that Find Duplicates needs deep testing and that a lot of it is
user logic. Deciding what counts as a duplicate — byte-identical? visually
similar? keep which copy? — is a product judgement I shouldn't make for you, and
it needs your eyes on real results.

But I'd separate three things that are currently sitting in one bucket:

1. **What counts as a duplicate** — genuinely your call, genuinely needs testing.
   Defer, agreed.

2. **"This pair is the same file"** — not a judgement call. A pair where one path
   is `/run/user/*/doc/…` and the other isn't is one file listed against itself.
   That's wrong under any definition of duplicate you might land on. Excluding it
   is a path-prefix check — no host_path, no ashpd, no V-19. Maybe 30 minutes, and
   it cuts a large slice off the 41,727 before you ever start the deep testing.

3. **One click, no confirmation, no undo** — independent of both. A destructive
   action with no confirm step, fed by a list we know is noisy right now.

### User position

Find Duplicates is **Experimental (UI/UX)**. Its usefulness depends on the
*address/location* being part of the presentation — a genuine duplicate implies
the user is storing the same set of images in different places, which the portal
does not fully capture (the file chooser lets them traverse the filesystem to
reach images by other routes). The more it is examined, the less important it
looks for the app *presently*. Deprioritised.

### Still open

Items 2 and 3 above are **not done** and are not blocked by V-19. They stay on
the backlog: the same-file guard is a path-prefix check, and `trash_duplicate_others`
still trashes on click with no confirmation.

