#!/usr/bin/env python3
"""Generate the Herse app icon (AppIcon.icns) with no external assets.

Design: macOS Big Sur grid. A 1024px canvas holding an 824px rounded square
(185px corner radius) with a soft drop shadow. The tile is warm paper; on it,
Herse's mark (the same drawing as the app's `icons/herse-*.svg`): a notebook
cell's brackets in ink holding a drop of dew in the accent teal. Herse is the
goddess of dew; the brackets are Jupyter's cell prompt.

Usage:
    python3 make_icon.py                 # writes AppIcon.icns + AppIcon-1024.png here
    python3 make_icon.py --out DIR       # writes into DIR instead

Needs Pillow (PIL) and, for the .icns step, macOS `iconutil`.
"""

from __future__ import annotations

import argparse
import math
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

CANVAS = 1024
BODY = 824                      # Big Sur icon body
BODY_INSET = (CANVAS - BODY) // 2
RADIUS = 185
SS = 4                          # supersampling factor for anti-aliasing

# Herse's palette (DESIGN.md): paper, ink, dew.
BG_TOP = (252, 251, 248)
BG_BOTTOM = (236, 233, 227)
INK = (28, 26, 23)
DEW_TOP = (22, 154, 146)
DEW_BOTTOM = (8, 104, 99)


def lerp(a: tuple[int, ...], b: tuple[int, ...], t: float) -> tuple[int, ...]:
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def body_mask() -> Image.Image:
    """Anti-aliased mask of the rounded-square body (supersampled, reduced to CANVAS)."""
    s = SS
    big = Image.new("L", (CANVAS * s, CANVAS * s), 0)
    ImageDraw.Draw(big).rounded_rectangle(
        [BODY_INSET * s, BODY_INSET * s, (BODY_INSET + BODY) * s - 1, (BODY_INSET + BODY) * s - 1],
        radius=RADIUS * s,
        fill=255,
    )
    return big.resize((CANVAS, CANVAS), Image.LANCZOS)


def gradient(top, bottom, y0, y1) -> Image.Image:
    col = Image.new("RGB", (1, CANVAS))
    for y in range(CANVAS):
        t = min(max((y - y0) / (y1 - y0), 0.0), 1.0)
        col.putpixel((0, y), lerp(top, bottom, t))
    return col.resize((CANVAS, CANVAS), Image.NEAREST)


def cubic(p0, p1, p2, p3, n=48):
    for i in range(n + 1):
        t = i / n
        u = 1 - t
        yield (u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0],
               u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1])


def drop_points():
    """The drop of `herse-drop.svg` in its 24-unit grid."""
    pts = list(cubic((12, 7.2), (12, 7.2), (8.7, 11), (8.7, 13.5)))
    # The bottom: a half circle of radius 3.3 about (12, 13.5).
    for i in range(1, 64):
        a = math.pi - math.pi * i / 64
        pts.append((12 + 3.3 * math.cos(a), 13.5 + 3.3 * math.sin(a)))
    pts += list(cubic((15.3, 13.5), (15.3, 11), (12, 7.2), (12, 7.2)))
    return pts


def mark_layers(stroke_units: float):
    """(brackets mask, drop mask) at CANVAS size, the mark centred on the tile."""
    s = SS
    unit = 30.0                  # px per grid unit: the brackets span 17 units, ~62% of the body
    ox = CANVAS / 2 - 12 * unit
    oy = CANVAS / 2 - 12 * unit

    def at(x, y):
        return ((ox + x * unit) * s, (oy + y * unit) * s)

    brackets = Image.new("L", (CANVAS * s, CANVAS * s), 0)
    d = ImageDraw.Draw(brackets)
    w = stroke_units * unit * s
    for path in ([(8, 3.5), (4.5, 3.5), (4.5, 20.5), (8, 20.5)],
                 [(16, 3.5), (19.5, 3.5), (19.5, 20.5), (16, 20.5)]):
        pts = [at(*p) for p in path]
        d.line(pts, fill=255, width=round(w), joint="curve")
        for x, y in (pts[0], pts[-1]):         # round caps
            d.ellipse([x - w / 2, y - w / 2, x + w / 2, y + w / 2], fill=255)

    drop = Image.new("L", (CANVAS * s, CANVAS * s), 0)
    ImageDraw.Draw(drop).polygon([at(*p) for p in drop_points()], fill=255)
    shine = Image.new("L", (CANVAS * s, CANVAS * s), 0)
    cx, cy = at(10.9, 13.1)
    rx, ry = 0.75 * unit * s, 1.05 * unit * s
    ImageDraw.Draw(shine).ellipse([cx - rx, cy - ry, cx + rx, cy + ry], fill=255)
    shine = ImageChops.multiply(shine.rotate(-25, center=(cx, cy)), drop)

    def down(m):
        return m.resize((CANVAS, CANVAS), Image.LANCZOS)

    return down(brackets), down(drop), down(shine)


def render(stroke: float = 1.6) -> Image.Image:
    """The full 1024x1024 icon (RGBA)."""
    mask = body_mask()

    # Drop shadow: soft, offset down, kept subtle.
    shadow_alpha = mask.filter(ImageFilter.GaussianBlur(22)).point(lambda v: int(v * 0.32))
    shadow_alpha = ImageChops.offset(shadow_alpha, 0, 12)
    out = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    out.paste(Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 255)), (0, 0), shadow_alpha)

    body = gradient(BG_TOP, BG_BOTTOM, BODY_INSET, BODY_INSET + BODY).convert("RGBA")
    # A faint rim so the paper tile holds its edge on a light Dock.
    rim = ImageChops.subtract(mask, mask.filter(ImageFilter.MinFilter(5))).point(lambda v: int(v * 0.10))
    body.paste(Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 255)), (0, 0), rim)

    brackets, drop, shine = mark_layers(stroke)
    body.paste(Image.new("RGBA", (CANVAS, CANVAS), INK + (255,)), (0, 0), brackets)
    # The drop: a soft contact shadow, a top-lit gradient, a highlight.
    d_shadow = ImageChops.offset(drop.filter(ImageFilter.GaussianBlur(8)).point(lambda v: int(v * 0.28)), 0, 6)
    body.paste(Image.new("RGBA", (CANVAS, CANVAS), (0, 40, 38, 255)), (0, 0), d_shadow)
    dew = gradient(DEW_TOP, DEW_BOTTOM, CANVAS / 2 - 5 * 30, CANVAS / 2 + 5 * 30).convert("RGBA")
    body.paste(dew, (0, 0), drop)
    body.paste(Image.new("RGBA", (CANVAS, CANVAS), (255, 255, 255, 255)), (0, 0), shine.point(lambda v: int(v * 0.42)))

    out.paste(body, (0, 0), mask)
    return out


ICONSET = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
]


def main() -> int:
    here = Path(__file__).resolve().parent
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, default=here, help="output directory (default: next to this script)")
    ap.add_argument("--keep-iconset", action="store_true", help="also leave AppIcon.iconset in the output dir")
    args = ap.parse_args()
    out_dir: Path = args.out
    out_dir.mkdir(parents=True, exist_ok=True)

    master = render(stroke=1.6)
    # Tiny sizes get bolder strokes so the mark survives at 16-64px.
    small = render(stroke=2.3)

    png_1024 = out_dir / "AppIcon-1024.png"
    master.save(png_1024, optimize=True)

    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for name, px in ICONSET:
            src = master if px >= 128 else small
            img = src if px == CANVAS else src.resize((px, px), Image.LANCZOS)
            img.save(iconset / name)
        if args.keep_iconset:
            keep = out_dir / "AppIcon.iconset"
            if keep.exists():
                shutil.rmtree(keep)
            shutil.copytree(iconset, keep)
        icns = out_dir / "AppIcon.icns"
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(icns)], check=True)

    print(f"wrote {png_1024}")
    print(f"wrote {icns}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
