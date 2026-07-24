# Competitive landscape & the extension roadmap it implies

Internal design note (2026-07-24). Where Vitrine sits among the Linux photo
tools, and — the useful part — what that says we should build **as extensions**
vs. natively. Companion to `PLAN.md §16` (the extension seam).

The thesis in one line: **Vitrine should not try to *become* darktable or
digiKam. It should be the fast, modern-format front-of-house that culls and
organizes at GTK4 speed and *calls in* the heavy tools as opt-in extensions.**

## The field

| Tool | One-line role |
|---|---|
| **Vitrine** | Fast browser + organizer + light non-destructive edit; content-hash catalog; best-in-class AVIF/JXL/HEIF decode (glycin). **Not** a RAW developer. |
| **darktable** | FOSS RAW-developer gold standard — scene-referred pipeline, masks, XMP sidecars, Lua, `darktable-cli`. Weak-ish DAM. |
| **RawTherapee** | FOSS RAW developer obsessed with demosaic/detail; a file browser, **no DAM**; `rawtherapee-cli`. |
| **digiKam** | FOSS DAM heavyweight — face recognition, geo/maps, deep metadata engine, Batch Queue Manager, competent editor. Heavy/complex. |
| **AfterShot Pro** | Commercial (~$80 perpetual) RAW dev + light DAM; Linux-native but **effectively stalled/legacy** (descends from Bibble). |

Workflow stages, and who owns them:

```
BROWSE → ORGANIZE(DAM) → DEVELOP(RAW) → PIXEL EDIT → EXPORT
Vitrine  ██ ██████        ·· delegates    · light      · via ext
digiKam  ██  ████████████   ·· (libraw)    ███          ████ BQM
darktable·   ████           ████████████    ███ masks    ████ cli
RawThera ·   ·              ████████████    ██           ███ cli
AfterShot··  ██████         ██████████      ██ layers    ███ watermark
```

Vitrine + digiKam own the **left** (browse/organize); the RAW trio owns the
**middle** (develop). Nobody owns Vitrine's exact corner: **fast + modern-format
+ Flatpak-native browsing.** That's the wedge.

## The matrix

| Axis | Vitrine | darktable | RawTherapee | digiKam | AfterShot |
|---|---|---|---|---|---|
| Primary role | browser/organizer | RAW dev | RAW dev | DAM | RAW dev + DAM |
| License | GPL free | GPL free | GPL free | GPL free | $80 perpetual |
| Linux-native | ✅ Flatpak/GTK4 | ✅ | ✅ | ✅ | ✅ (rare, commercial) |
| RAW develop depth | ✗ (decode only) | ★★★★★ | ★★★★★ | ★★★ | ★★★★ |
| DAM / catalog | ★★★ hash-keyed | ★★ | ✗ | ★★★★★ | ★★★ |
| Catalog model | **folder-first, no import** | sidecar+db | folder | mandatory-ish db | no forced import |
| Face recognition | ✗ | ✗ | ✗ | ✅ standout | ✗ |
| Geo / maps | ✗ | basic | ✗ | ✅ strong | basic |
| Non-destructive | ✅ instructions | ✅ edit stack | ✅ .pp3 | partial | ✅ |
| Layers/masks/local | ✗ | ★★★★ masks | ★★ | ★★ | ★★ layers |
| Batch engine | planned (E2) | `darktable-cli` | `rawtherapee-cli` | ★★★★★ BQM | ★★★★ |
| Scripting | **Lua sorts (E1)** | Lua | none | DPlugins | none |
| Metadata sidecar | XMP export | XMP | pp3 | XMP/IPTC full | XMP |
| Modern fmts (AVIF/JXL/HEIF) | **★★★★★ glycin** | weak | weak | some | ✗ |
| Speed / footprint | **★★★★★** | ★★★ | ★★★ | ★★ heavy | ★★★★ |
| Active dev | ✅ | ✅✅ | ✅ | ✅✅ | ✗ stalled |

Two rows drive strategy: **modern formats** (Vitrine wins outright — the RAW
tools are RAW-brained and weak on AVIF/JXL/HEIF) and **RAW develop** (Vitrine is
a zero there, and shouldn't fill it natively).

## The extension payload (maps onto the E-tier seam)

Each competitor donates a capability Vitrine can adopt **as a helper**, not a
rewrite.

### E2 — Develop RAWs without becoming a RAW developer (`darktable-cli` / `rawtherapee-cli`)
The reason the extension model exists. `darktable-cli` is headless: it **applies
an edit stack (a sidecar XMP, or a saved *style*) and exports** —
`darktable-cli in.raw [sidecar.xmp] out.jpg --style "Look"`. So Vitrine offers a
batch: *"develop these 40 RAWs with style X → JPEG"* via `register_batch` + the
run dialog (backup-first, progress, cancel). Same pattern wraps `rawtherapee-cli`
(`-p profile.pp3 -c`) as an alternate engine. **RAW development becomes an opt-in
helper package; we never write a demosaicer.** (This is digiKam-uses-external-
engines, but leaner.)

### E3 — The Magick window: colour, convert, enhance, watermark (already planned)
ImageMagick covers the "light pixel work" middle that AfterShot/digiKam bundle:
- gThumb-style **colour tools** (the stated E3 goal): levels/curves/saturation/
  auto-enhance → IM recipes.
- **Watermarking** — AfterShot's headline feature + digiKam BQM's; trivial in IM;
  a natural starter recipe.
- **Convert / resize / strip-metadata** batches — every DAM's export bread-and-butter.
- Highlight recovery / lens correction lean **darktable-cli**, not IM — route deliberately.

### Small, high-leverage — XMP round-trip (the interop *killer feature*)
Vitrine already **exports** XMP sidecars. Close the loop and **read** them too.
Then rate/tag in Vitrine → lands in the sidecar → **darktable and digiKam see
it**, and vice-versa. Vitrine becomes the fast front-of-house for a library also
developed in darktable — "cull at GTK4 speed here, develop keepers there, ratings
stay in sync." No competitor offers a *fast modern culler that speaks the same
metadata*; digiKam/darktable are too heavy to be that.

### Native (not extensions) — DAM features cherry-picked from digiKam
Core-viewer / index work, not subprocess helpers (same call as the histogram):
- **Map / GPS panel** — reads EXIF GPS; a live viewer-sidebar readout → **core**.
- **Better search** — extend the `Query` struct to a real builder (rating+tag+
  date+camera); continues V-11 fuzzy-find.
- **Similarity ("more like this")** — surface the existing pHash primitive as a
  feature, not just dedup cleanup.

### E4 — Face recognition (the one genuinely heavy thing), *someday*
digiKam's crown jewel; it's an ML model. Exactly the reserved **E4 WASM/ONNX**
tier: an optional model-backed extension (detect faces → suggest tags). Big,
opt-in, later.

## The rule this yields (add to the extension-seam thinking)

> **Interactive & about the image you're looking at** (histogram, map, crop,
> search, ratings) → **core**.
> **Batch & produces files** (develop, convert, watermark, resize) →
> **extension** (darktable-cli / ImageMagick, E2/E3).
> **ML / heavy** (faces, auto-tag) → **E4**.

It already falls out of the seam; the competitors just fill it with concrete,
battle-tested features.

## Positioning, stated

**Vitrine is the fast, modern-format front-of-house that culls and organizes at
GTK4 speed, keeps its metadata portable (content-hash + XMP), and calls in the
heavy tools — darktable, ImageMagick — as opt-in extensions when needed.**

- **vs digiKam** — ~10× lighter/faster, actually shows AVIF/JXL/HEIF; *delegates*
  develop/DAM depth. Different customer: Nautilus-speed browsing, not a DAM cockpit.
- **vs darktable** — the browser it lacks; it's the developer we won't build.
  Symbiotic via XMP, not competitive.
- **vs RawTherapee / AfterShot** — pure developers (AfterShot dying); Vitrine is
  the layer above that can *invoke* a developer.

Not a roadmap commitment — a map of where the opportunities are and which tier
each belongs to, so extension work stays disciplined instead of drifting into a
digiKam clone.
