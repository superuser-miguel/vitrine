# Soak checklist — things only a person at the app can confirm

Shipped changes that tests, headless runs and DB checks couldn't fully cover.
Work through these during the next soak (daily use is fine — no need to do
them in one sitting). Tick an item, add the date and what you saw; move a
failure into `ISSUES.md` as a new V-entry.

Where an item touches real files, use a copy of an image you don't mind
changing.

---

## From 0.3.2 (2026-10-08)

**Save and Save As** (V-32, V-33, V-34 — these write your files)
- [ ] On an `.avif` / `.webp` / `.heic`, **Save** is greyed out, with a tooltip
      saying to use Save As.
- [ ] Rotate a `.jpg`, then **Save**: the confirm dialog mentions that embedded
      metadata won't be kept; after saving, the rotation is applied **once**
      (not twice) and the image's tags and rating are still there.
- [ ] Same as above on an image that **also exists as a duplicate** somewhere
      else in the library: after saving, the *other* copy still has its tags.
- [ ] **Save As**, choose the original file → the Save-in-place confirm dialog
      appears (it doesn't silently overwrite).
- [ ] The same, in a folder opened through the **file picker** (document portal
      path) — the one case no automated check could reach.
- [ ] **Save As** to a name ending `.webp` → a refusal toast, nothing written.
- [ ] A failed save (e.g. a read-only folder) shows a toast, not nothing.

**Viewer** (V-38)
- [ ] Edit an image, move to one you haven't edited → Undo/Redo turn off.
- [ ] A corrupt image next to the one you're viewing doesn't make stepping
      sluggish (it's tried once, then skipped).

**Library** (V-35, V-36, V-39)
- [ ] Drag a line of text from a browser onto a catalog → nothing is added.
- [ ] Collection counts in the sidebar look right (same numbers as before).
- [ ] Opening a tag or a big collection feels at least as quick as before.
- [ ] If a scan ever fails (rare: disk full, I/O error), the "Indexing…" banner
      still goes away.

## Carried over from 0.3.1 (2026-10-06)

- [ ] **Preferences → Clean Up Thumbnails…** opens, shows sizes per cache, the
      preview count updates, and a cleanup removes what it said it would.
- [ ] **Icon size is remembered**: change it, quit, reopen. (Not yet seen in
      settings as of 2026-10-08 — no `[Grid]` section — so either it hasn't been
      changed since 0.3.1, or it isn't saving.)
- [ ] **V-05**: open a folder that has never been indexed and drag an image to a
      catalog immediately → it either works or a toast says "Still indexing".
- [ ] **V-30**: at the two largest icon sizes, a `vitrine-thumbnails` folder
      appears in `~/.var/app/io.github.superuser_miguel.Vitrine/cache/`, and
      Files' thumbnails are unaffected.

## Older, still open

- [ ] **V-04 nested path**: open a never-indexed folder within the first few
      minutes after launch; `build-aux/debug-run.sh --interact` should show a
      `VDBG-SCANYIELD` line.

---

*How items get here:* every release lists what it couldn't verify by machine;
those lines land in this file the day it ships. *How they leave:* ticked with a
date, or turned into an ISSUES entry.
