#!/usr/bin/env python3
"""Writes the ladder site's favicon (service/assets/favicon.svg and favicon.ico).

    tools/favicon.py        # needs Pillow

The mark is the one in the site's header (service/templates/base.html): a banana on a
banana-yellow disc. One geometry, on a 28-unit square, makes both files: the SVG as text and
the ICO (16, 32 and 48 pixels) drawn by Pillow at eight times the size and scaled down. The
files are generated, never edited: change the numbers here, run this and commit what it
writes. The service embeds both and serves them at /favicon.svg and /favicon.ico.
"""
from pathlib import Path

from PIL import Image, ImageDraw

SIZE = 28
DISC = "#f4bb35"
BANANA = "#2b2103"
# The banana as cubic Béziers: a start point, then (control, control, end) triples, closed.
START = (8.4, 5.9)
CURVES = [
    ((5.2, 14.8), (11.4, 23.9), (21.2, 22.0)),
    ((22.9, 21.6), (23.3, 19.4), (21.7, 18.7)),
    ((16.8, 18.4), (13.2, 14.0), (12.8, 6.5)),
    ((12.7, 4.6), (8.8, 4.3), (8.4, 5.9)),
]


def num(value):
    return f"{value:.2f}".rstrip("0").rstrip(".")


def svg():
    path = f"M{num(START[0])} {num(START[1])}"
    for c1, c2, end in CURVES:
        path += "C" + " ".join(f"{num(x)} {num(y)}" for x, y in (c1, c2, end))
    half = num(SIZE / 2)
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}">'
        f'<circle cx="{half}" cy="{half}" r="{half}" fill="{DISC}"/>'
        f'<path d="{path}z" fill="{BANANA}"/></svg>\n'
    )


def outline(steps=24):
    points, at = [], START
    for c1, c2, end in CURVES:
        for i in range(1, steps + 1):
            t = i / steps
            u = 1 - t
            points.append(
                tuple(
                    u**3 * at[k] + 3 * u * u * t * c1[k] + 3 * u * t * t * c2[k] + t**3 * end[k]
                    for k in (0, 1)
                )
            )
        at = end
    return points


def raster(pixels, oversample=8):
    side = pixels * oversample
    scale = side / SIZE
    image = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.ellipse((0, 0, side - 1, side - 1), fill=DISC)
    draw.polygon([(x * scale, y * scale) for x, y in outline()], fill=BANANA)
    return image.resize((pixels, pixels), Image.LANCZOS)


def main():
    out = Path(__file__).resolve().parent.parent / "service" / "assets"
    out.mkdir(parents=True, exist_ok=True)
    (out / "favicon.svg").write_text(svg())
    sizes = [16, 32, 48]
    images = [raster(n) for n in sizes]
    images[-1].save(out / "favicon.ico", format="ICO", append_images=images[:-1], sizes=[(n, n) for n in sizes])
    print(f"wrote {out / 'favicon.svg'} and {out / 'favicon.ico'}")


if __name__ == "__main__":
    main()
