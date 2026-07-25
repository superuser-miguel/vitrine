# Thumbnail pop-in test cases (for a later session)

Goal: nail remaining cold pop-in. Tools that already exist: `VITRINE_SOAK`,
`VITRINE_NOCACHE`, `VITRINE_DEBUG` (`VDBG-FILL ms= bytes=` per decoded
completion), `VITRINE_HEAVY_LIMIT/BYTES`. Fixtures: generate under
`~/Pictures/_vitrine_*` (flatpak sees only xdg-pictures), then clean folder +
`files` DB rows + md5(uri) cache PNGs in both thumbnail caches (see PLAN §13.1
method note).

## Plumbing (built 2026-07-18)
1. ✅ `VDBG-GRIDFILL ms= pos= center= visible= hit=` — every grid completion,
   position vs viewport centre, visible-cell vs prefetch, cache hit.
   ✅ `VDBG-FILM ms= pos= center=` — every filmstrip completion.
   (`VDBG-FILL ms= bytes=` remains the size/latency line.)
2. ✅ `VDBG-LOAD pos= src= showing= hash=` (added 2026-07-24, diagnosed V-28) —
   per grid load: where the texture came from (`ram`/`content`/`disk`/`FAIL`),
   whether a cell was still showing the item at completion, and whether the
   content hash was enriched. `src=… showing=false` = decoded into RAM but
   painted to nobody — the V-28 smoking gun. Note: bind-time RAM hits never
   reach this line (they paint synchronously); `src=ram` here means a *queued*
   load found RAM warm, which is rare by design.

## Filmstrip (bugs found + fixed 2026-07-18 — regression cases, all verified)
F1. ✅ **Fill order**: cold 3k-item folder, open viewer mid-folder. VDBG-FILM
    completions radiate from `center` (verified: 1500, 1501, 1505, 1504, …@
    center=1500). Was: LIFO pop → strip filled backwards from overscan.
F2. ✅ **No starved cells**: SOAK strip sweep over 3k items, cold. Assert cells
    visible at every rest point paint within ~1 s (verified: open-screen 17/17
    by +0.8 s; sweep-end 20/20). Was two bugs: cap dropped *oldest* entries
    (could be on-screen) and dead/recycled entries counted against the cap.
F3. ✅ **Viewport signal**: `film_center()` = hadjustment-derived centre, with a
    `scroll_to`-target hint until the (async) hadjustment catches up — without
    the hint, viewer-open evicted the on-screen cells using center≈0 (verified:
    fills held center=1500 through the open; previously decayed to center=5).
Pitfall for future assertions: "last cells bound before a rest" ≈ on-screen
only at the strip's ends — elsewhere it's GTK's overscan tail. Assert on the
region around the selection / known scroll target instead.

## Queue invariant (2026-07-24, V-28 — supersedes the F2 cap mechanics)
The grid's bind journal and the strip's `film_queue` are now **deduped per
cell** (a rebind replaces that cell's entry) and their count caps are gone —
the bound-cell pool is the natural bound. Rationale: GTK binds ~200 positions
ahead of the viewport, so after a fling the on-screen rows are *never* the
newest binds; any newest-N cap evicts exactly them, and no path repaints a
bound-but-blank cell. F2's distance-trim had the same hole from the other side
(cap 96 < pool ~200: trimmed *live* overscan entries starved on a short drag).
F1/F2/F3 remain valid as regression cases; their old cap-policy rationale is
historical. Full postmortem: ISSUES.md V-28.
G1. ✅ **Post-fling rest paint (grid)**: warm-session fling deep into a 3k
    folder (SOAK scroll-grid is a valid synthetic — 24×60 ms steps stay under
    the 90 ms debounce), stop dead, hands off. The resting viewport's GRIDFILL
    completions must be `visible=true`, radiating from `center`; prefetch
    (`visible=false`) confined to the `PREFETCH_BEHIND/AHEAD` margins.
    (Verified 2026-07-24 on `_v28_fling`: center=2998 filled center-out
    `visible=true`, 16 trailing prefetches. Broken signature: viewport
    completes `visible=false`-only, then a dead window with `queued=0`.)
F4. **Strip slow-drag starvation guard** (the case F2's sweep missed): open
    viewer mid-folder, wait for the bind wave to drain, then drag the strip
    slowly by ~half a page and rest. Cells entering the viewport must paint
    (union of FILMBIND `hit=true` and VDBG-FILM completions covers the new
    visible range). Pre-fix, entries beyond the 96 nearest were trimmed live
    and those cells arrived bound-but-blank with no re-queue path.
    (Indirect verification 2026-07-24: single open-viewer bind wave drains
    **205** VDBG-FILM completions — impossible under the old 96 cap.)

## Test cases (each: cold open via SOAK+NOCACHE, assert on VDBG-FILL)
2. **Time-to-visible-complete** ("grid LCP"): settle → every visible cell has a
   real texture. Assert < 2 s on a 200-file mixed fixture; flag regressions.
3. **Fill order holds under cost variance**: large-at-top / clustered /
   sprinkled arrangements (~40× size spread, JPEG + AVIF/JXL, non-image
   siblings). Assert first N completions are visible items; no visible item
   completes after an invisible one started later (starvation).
4. **Heavy-lane tuning**: sweep `VITRINE_HEAVY_LIMIT` 1/2/3 and
   `HEAVY_BYTES` 1/2/4 MiB on case 3; pick per worst-stall + visible-complete.
   (Baseline shipped: limit 2 / 2 MiB; worst stall 1957→72 ms.)
5. **Byte-size proxy failure**: AVIF/JXL are small-bytes/big-pixels — a 0.5 MB
   24 MP AVIF dodges the heavy lane. Once enriched, dimensions are in the DB:
   route by known pixel area when available, bytes otherwise. Test both paths.
6. **Slow-media cold open**: user's parked HDD case — repeat case 2 from a
   spinning disk (or `mount -o sync` loop device as a stand-in).
7. **Placeholder feel guard**: viewer-open placeholder (30/30 in soak) — assert
   `VDBG-VIEWER placeholder=true` rate stays 100 % warm, and full texture
   replaces it < 1 s cold.
