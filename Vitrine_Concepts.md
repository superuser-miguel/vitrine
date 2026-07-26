# Vitrine — Concepts

A living document for **design concepts and directions considered** — the
"why / what-if" behind bigger ideas, captured so the reasoning survives even
when the work isn't scheduled. Distinct from `PLAN.md` (the concrete roadmap and
its §-numbered seams) and from `COMPETITIVE-LANDSCAPE.md` (where Vitrine sits
among other tools). When a concept here graduates into planned work, it moves to
`PLAN.md`; this file keeps the thinking.

---

## Querying the index database

*Captured 2026-07-26, out of the tagging rethink — "can users query the DB?"*

### The store, in one paragraph

Everything Vitrine knows lives in **one SQLite file** — `index.sqlite`, WAL mode,
app-private under Flatpak at
`~/.var/app/io.github.superuser_miguel.Vitrine/data/vitrine/index.sqlite`. It
holds files, tags, ratings, comments, collections, orientations and crops. A
**single writer thread** (the annotator) owns all mutations; reads go through
separate WAL reader connections. Rows are keyed by **`content_hash` (BLAKE3)**,
not path — `files(path, content_hash)` reconciles the two, and one image can
have several `files` rows (e.g. a portal path and the real path, see V-19)
sharing one hash.

### Today: external read-only is already possible

The index is a plain SQLite file at a known path, so a power user can already
point `sqlite3` or DB Browser at it and query. Three caveats make that
"works, but unsupported":

- **WAL mode** — read with the app closed, or strictly read-only, or you race
  the writer.
- **`content_hash` keying** — to get filenames you join `file_tags` / ratings
  through `files`, and the portal-path duplication means the same image can
  surface under two `files` rows.
- **The schema is internal, not a contract** — it migrates version to version.
  Querying it directly couples you to internals that can change underneath you.

Fine for a curious power user poking around. **Never** a place to *write* — all
mutations must go through the app's single writer thread, which owns the WAL and
the data invariants.

### As a feature — the spectrum

Ordered roughly safest/most-modern → most-powerful/most-coupled:

1. **Structured search UI.** Expand the existing filter bar into rating + tags +
   date + camera + folder with boolean combinations. No SQL surface, stays in
   core, and the query machinery already exists internally (`Db::query` with a
   `Query` builder — used today by the filter bar and tag-browse). Mostly UI over
   what's there. *Most broadly useful.*
2. **Saved searches / smart albums.** A persisted query that behaves like a live
   collection (digiKam / Lightroom style). Builds directly on #1. Likely the
   highest-value "querying" feature for a large library, and squarely on-brand
   for the modernity North Star.
3. **Read-only query from the Lua scripting API (E2).** Expose something like
   `vitrine.query{ tags = {...}, min_rating = 4 }` returning matching images for
   batch actions — the *open-ended glue* home Lua actually belongs in (after the
   native fuzzy find obsoleted the sort/search script demo). Read-only queries
   are low-risk; the §16.2 host-API versioning discipline applies.
4. **Raw SQL console** for power users. Most powerful, least safe — it turns the
   internal schema into a public contract and freezes the ability to migrate it.
   **Not recommended:** #1–#3 deliver the value without the trap.

### Recommendation

- User-facing path: **structured search (#1) → saved searches (#2).**
- Power path: a **read-only** query call in the **Lua API (#3).**
- **Skip the raw SQL console (#4)** as a public surface.
- Writes **always** through the annotator — never user-supplied SQL.

### Principles that fall out

- **Reads are cheap and safe to expose; writes go through the one writer.**
  Query = read; mutation = the app's write path, always.
- **Never expose the internal schema as a stable contract.** Exposed querying is
  phrased in Vitrine's own terms (tags, ratings, dates), not table/column names,
  so the schema stays free to evolve.
- **`content_hash` is the identity, not the path.** Any query surface returns/acts
  on images, and hides the hash↔path reconciliation (and portal duplication) from
  the user.

*No build scheduled — pairs with the tagging rethink when that turns into search
work.*
