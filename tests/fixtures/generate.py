#!/usr/bin/env python3
"""Generate the tiny sample images used by vitrine-engine tests (PLAN §6).

Deterministic output, kept small (repo stays well under a couple MB). Rerun
after changing the fixture set. Requires Pillow.

Fixtures produced in tests/fixtures/images/:
  rgb_100x50.png          reference pattern (PNG)
  rgb_100x50.jpg          same pixels, different bytes  -> near-dup pair
  rgb_100x50_copy.jpg     byte-identical copy of the jpg -> exact-dup pair
  rgb_100x50_scaled.jpg   same image at 50x25            -> near-dup (resize)
  exif_dated.jpg          carries DateTimeOriginal + Orientation=6
  corrupt.jpg             truncated file                 -> decode-failure path
  fake.png                text content, .png extension   -> mime-sniff failure
  rgb_100x50.webp         (if the toolchain can encode WebP)
  rgb_100x50.avif         (if the toolchain can encode AVIF)
"""

import shutil
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent / "images"
OUT.mkdir(parents=True, exist_ok=True)


def pattern(w: int, h: int) -> Image.Image:
    """A deterministic, non-flat RGB pattern so perceptual hashing is meaningful."""
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            px[x, y] = ((x * 255) // w, (y * 255) // h, ((x + y) * 255) // (w + h))
    draw = ImageDraw.Draw(img)
    draw.rectangle([w // 5, h // 5, w // 2, h // 2], fill=(20, 200, 90))
    draw.ellipse([w // 2, h // 3, w - w // 6, h - h // 6], fill=(230, 60, 60))
    return img


def main() -> None:
    base = pattern(100, 50)

    png = OUT / "rgb_100x50.png"
    base.save(png, "PNG")

    jpg = OUT / "rgb_100x50.jpg"
    base.save(jpg, "JPEG", quality=90)

    # Exact-dup: byte-identical copy of the jpg.
    shutil.copyfile(jpg, OUT / "rgb_100x50_copy.jpg")

    # Near-dup: same image, resized to 50x25.
    base.resize((50, 25)).save(OUT / "rgb_100x50_scaled.jpg", "JPEG", quality=90)

    # EXIF: DateTimeOriginal + Orientation=6 (rotate 90 CW).
    exif = Image.Exif()
    exif[0x0112] = 6  # Orientation
    exif[0x9003] = "2019:07:04 13:37:00"  # DateTimeOriginal
    exif[0x0132] = "2019:07:04 13:37:00"  # DateTime
    exif[0x010F] = "Vitrine"  # Make
    exif[0x0110] = "Fixture Camera"  # Model
    base.save(OUT / "exif_dated.jpg", "JPEG", quality=90, exif=exif)

    # Corrupt: a valid JPEG header then truncation.
    data = jpg.read_bytes()
    (OUT / "corrupt.jpg").write_bytes(data[: len(data) // 3])

    # Mime-sniff failure: text content with a .png extension.
    (OUT / "fake.png").write_text("this is not an image\n")

    # Optional formats — skip cleanly if the Pillow build can't encode them.
    for fmt, ext in (("WEBP", "webp"), ("AVIF", "avif")):
        try:
            base.save(OUT / f"rgb_100x50.{ext}", fmt)
        except Exception as e:  # noqa: BLE001 - best-effort
            print(f"skipping {ext}: {e}")

    for f in sorted(OUT.iterdir()):
        print(f"{f.stat().st_size:>7}  {f.name}")


if __name__ == "__main__":
    main()
