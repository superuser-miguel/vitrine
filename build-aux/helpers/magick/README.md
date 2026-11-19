# ImageMagick helper (E2/E3 groundwork)

The colour tools' ImageMagick, packaged as a Flatpak app extension of Vitrine
and mounted at `/app/helpers/magick`. Not wired into the app yet.

- `io.github.superuser_miguel.Vitrine.Helper.magick.yml` — the extension
  manifest. One module; every image library comes from the GNOME 50 runtime.
- `policy.xml` — installed over ImageMagick's default. Allows image formats
  only (JPEG, PNG, TIFF, WebP, HEIC/AVIF, JXL, JPEG 2000, PNM, MIFF); denies
  every other coder, delegates and `@file` indirection; caps memory 1 GiB,
  pixel area 256 MP, 32k px per side, 120 s.
- `…metainfo.xml` — `type="addon"`, `<extends>` Vitrine, so GNOME Software
  can list it as an add-on (to verify, G3).

Verified 2026-11-19 (standalone build, run under `flatpak build`):

| Check | Result |
| --- | --- |
| Build time / installed size | under 1 min / ~6 MB |
| JPEG, AVIF, JXL in, PNG out | all succeed |
| MVG, TXT, HTTPS, SVG write, `@file` | all refused by policy |
| 49 KB PNG that expands to 400 MP | refused ("cache resources exhausted") |
| `flatpak-spawn --sandbox` from Vitrine | no unexposed files, read-only exposure holds, no network |

Open: building as an extension requires the extension point in an *installed*
Vitrine (G2) — dev builds are never installed into the user's Flatpak, so
helper builds need a throwaway installation (`FLATPAK_USER_DIR`).
