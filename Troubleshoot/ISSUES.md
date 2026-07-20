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

### V-17 · Rubber-band selection steals the press from item drags · `CONFIRMED` · **FIX UNTESTED**

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

### V-08 · Nautilus → Collection never wired · `CONFIRMED`

`window.rs:2242`: `gtk::DropTarget::new(String::static_type(), COPY)` — accepts
`String` only. Nautilus delivers `GdkFileList` / `text/uri-list`, so the type never
matches and the handler is never called.

The "+" cursor is not the collection accepting; it's almost certainly the
`places_scroller` FileList target (`window.rs:1323`) underneath, which accepts folder
drops for bookmarking.

Not a bug — unimplemented. Needs: accept `FileList`, resolve path → `content_hash`
via the read DB (**no such query exists yet**), then `add_to_catalog`.

### V-09 · No remove-tag flow · `CONFIRMED`

`Annotator::tag(name, hashes, add)` already takes an `add: bool` and the worker
handles `false` → `db.remove_tag(...)`. The UI only ever passes `true`
(`apply_tag_to_selection` hardcodes it). Backend is done; only UI is missing.

### V-10 · Tagging / DnD "go stale after a folder change" · `OBSERVED`, cause not confirmed

May be fully explained by V-04 + V-05 rather than being a distinct defect. Do not
fix speculatively — re-test once V-03 instrumentation lands.

### V-11 · No global search · `OBSERVED`

Only rating + single-tag filter today. Wants filename / path / tag / comment / EXIF.
Engine has a `Query` struct to extend. FTS5 if it needs to scale.

### V-12 · Sort lacks Date Taken and Rating · `OBSERVED`

`date_taken` is already indexed (`idx_files_date`), so Date Taken is close to free.

### V-13 · No Clear / Home button · `OBSERVED`

No way back to the initial "No Folder Open" state without restarting.

### V-14 · Collection polish · `OBSERVED`

Manual reorder within a collection; bulk remove (blocked on V-01); collection
thumbnails in the sidebar.

---

## Tier 4 — Open questions.

### V-15 · `Adwaita-CRITICAL: Page 'Viewer' is not in the navigation stack` · `OBSERVED`

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
