#!/usr/bin/env python3
"""Synthesize culler-core/tests/fixtures/orientation_6.jpg.

Why synthesized instead of a real camera photo: this build environment has no
camera source, no `exiftool`, and no ImageMagick (`magick`) binary to shoot or
verify a real portrait JPEG (see CONTROLLER ADDENDUM, Task 5 brief). PIL is
available (10.2.0) and piexif is a small pure-Python package installable from
PyPI, so the fixture is generated deterministically from this script instead.

Produces a JPEG whose:
  - Main image is stored LANDSCAPE, 120x80 (w > h), pixel content a
    non-uniform RGB gradient (not a solid color).
  - EXIF IFD0 carries Orientation = 6 (`Rotate 90 CW` display transform:
    displaying it correctly requires rotating the stored landscape pixels
    into a portrait h > w layout).
  - EXIF IFD1 carries a real embedded JPEG thumbnail, also stored LANDSCAPE,
    30x20, gradient content, wired via piexif's `thumbnail` key so that
    `JPEGInterchangeFormat`/`JPEGInterchangeFormatLength` point at genuine
    JPEG bytes inside the EXIF blob (culler-core Task 6 reads this thumbnail).
  - Total file size well under 50 KB.

Regenerate with:
    pip install --target /path/to/pylibs piexif   # or: python3 -m pip install piexif
    PYTHONPATH=/path/to/pylibs python3 culler-core/tests/fixtures/make_orientation_6.py
"""

import io
import os
import sys

from PIL import Image
import piexif


FIXTURE_PATH = os.path.join(os.path.dirname(__file__), "orientation_6.jpg")

MAIN_W, MAIN_H = 120, 80
THUMB_W, THUMB_H = 30, 20


def gradient_rgb(w: int, h: int) -> Image.Image:
    """Non-uniform RGB gradient: R varies by x, G varies by y, B fixed."""
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            r = int(255 * x / max(1, w - 1))
            g = int(255 * y / max(1, h - 1))
            px[x, y] = (r, g, 64)
    return img


def main() -> None:
    main_img = gradient_rgb(MAIN_W, MAIN_H)
    thumb_img = gradient_rgb(THUMB_W, THUMB_H)

    thumb_buf = io.BytesIO()
    thumb_img.save(thumb_buf, "JPEG", quality=85)
    thumb_bytes = thumb_buf.getvalue()

    exif_dict = {
        "0th": {piexif.ImageIFD.Orientation: 6},
        "Exif": {},
        "1st": {},
        "thumbnail": thumb_bytes,
    }
    exif_bytes = piexif.dump(exif_dict)

    main_img.save(FIXTURE_PATH, "JPEG", exif=exif_bytes, quality=85)

    size = os.path.getsize(FIXTURE_PATH)
    print(f"wrote {FIXTURE_PATH} ({size} bytes)")
    if size >= 50 * 1024:
        print("WARNING: fixture is >= 50 KB", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
