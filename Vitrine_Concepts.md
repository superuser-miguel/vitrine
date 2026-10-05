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

---

## Sidecars → gallery: how downloaded metadata hooks into presentation

*Captured 2026-08-04, out of the ArtistDownloadScripts hardening — "how does
this all hook in with how the data is presented?"*

### The capture contract (upstream, already live)

The per-artist download scripts now guarantee: **every media file lands with a
sidecar beside it** (gallery-dl `file.ext.json` for photos/tweets, yt-dlp
`name.info.json` for videos), carrying the durable identity — platform ID,
creator, **full caption**, source URL, post date. The filename is just an
address: truncated to 150 bytes, ID-suffixed for uniqueness, never trusted to
carry meaning. That split *is* the ingest contract, and it matches Vitrine's
own identity model: rows are keyed by `content_hash`, so metadata follows the
pixels through renames, USB migrations and reorganizations — the filename
fragility defended against at download time stops mattering entirely once
ingest happens. Sidecars persist on disk with zero loss, so ingest can wait
indefinitely.

### Ingest: one more enricher, two quarantined tables (the P2 plan)

The scanner already ignores sidecars (extension whitelist) — invisible by
design until P2. The real work is small: when indexing `photo.jpg`, also read
the adjacent `photo.jpg.json` as **one more enricher on the existing
background enrichment pass** (same machinery as the histogram; NULLS-LAST
tolerance for late data already exists — no new architecture). It fills two
hash-keyed tables:

- **`sidecar_meta`** — creator, site, source URL, posted-at, plus the
  *verbatim JSON* so nothing is ever lost to field-mapping choices.
- **`sidecar_tags`** — kept **separate from curated `file_tags`**, so a rescan
  can never fight hand edits and a booru-style 80-tag flood never pollutes the
  small curated vocabulary. Promotion into real tags is always explicit.

### Presentation: two tiers on surfaces that already exist

- **Tier A — Properties sidebar** (where the histogram just went): a "Source"
  section that only renders when data exists — creator, site, post date,
  click-through to the original URL — plus read-only "Site tags" chips with a
  per-chip **promote** button. Platform metadata is visible but quarantined
  until blessed.
- **Tier B — queries and smart collections**: creator/site/sidecar-tag
  predicates in the query layer (builds directly on the structured-search →
  saved-searches path above; the smart-collection engine already resolves
  live). Yields auto-organizing "Artist: X" collections — new downloads just
  *appear* in the right collection. The per-artist folder tree becomes
  optional, because artist-ness lives in the data.
- **Later, Lua (§16) as the policy layer**: auto-promotion rules writing
  `file_tags.source='rule'` (revocable in bulk), creator normalization across
  TikTok/X handles, sidecar-aware sort keys. Hand-promotion comes first;
  repeated promotions become the rule engine's spec (the E1 lesson).

### The same pattern, twice more

- **Video Gallery app** (separate, future): the "TikTok webpage meets
  Nautilus + gThumb" vision is *caption-forward* — feed shows video + full
  caption + creator + date. Only possible because `.info.json` preserves the
  full caption; the UI reads `title` from the sidecar, never the filename.
  Duration/resolution/view counts come free. The sidecar captures happening
  now are that app's entire data layer, banked years early — sources get
  deleted; the sidecar is the copy that survives.
- **Download ledger DB** (deferred, see Obsidian "Download Ledger DB - Future
  Plan"): if built, `downloads.sqlite3` joins by platform ID against sidecar
  IDs, adding *provenance* the sidecars don't have — when it was grabbed,
  what's still blocked. Gallery shows the art; sidecars carry the art's
  story; the ledger carries the story of collecting it.

### Principles that fall out

- **Filenames are addresses; sidecars are the record.** Anything that must
  survive belongs in the JSON (or the DB), never in the name.
- **Ingested platform data stays quarantined until promoted.** Curated tags
  remain the user's; promotion is explicit (or rule-driven and revocable).
- **Capture now, ingest whenever.** Sidecars on disk are the durable layer;
  every downstream stage (P2 tables, Tier A/B UI, Video Gallery, ledger
  joins) can land on its own schedule with zero data loss.

*Sequencing: P2 ingest ships as its own clean build after v0.2.0's soak;
Tier A before Tier B; Lua rules only after hand-promotion patterns emerge.*
