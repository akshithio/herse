#!/usr/bin/env python3
"""Generate the Herse app icon (AppIcon.icns) with no external assets.

Design: Herse's logo (`native/brand/herse-logo.svg`), a pale grey swash on
blue, set on the macOS Big Sur grid: a 1024px canvas holding an 824px
rounded square (185px corner radius) with a soft drop shadow. The swash is
centred on the tile, 60% of its height, as in the logo.

Usage:
    python3 make_icon.py                 # writes AppIcon.icns + AppIcon-1024.png here
    python3 make_icon.py --out DIR       # writes into DIR instead

Needs Pillow (PIL) and, for the .icns step, macOS `iconutil`.
"""

from __future__ import annotations

import argparse
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

# The logo's colours (brand/herse-logo.svg).
BLUE = (6, 130, 254)            # #0682FE
GREY = (238, 238, 238)          # #EEEEEE

# How tall the swash stands on the tile.
MARK = 0.60

# The logo's one path, as cubic segments (start, c1, c2, end):
# M65.5 52 C65.5 61.3888 57.8888 69 48.5 69 C39.1112 69 45.5 51 34 53
# C46.5 50.8261 39.1112 35 48.5 35 C30 78 65.5 42.6112 65.5 52 Z
SWASH = [
    ((65.5, 52), (65.5, 61.3888), (57.8888, 69), (48.5, 69)),
    ((48.5, 69), (39.1112, 69), (45.5, 51), (34, 53)),
    ((34, 53), (46.5, 50.8261), (39.1112, 35), (48.5, 35)),
    ((48.5, 35), (30, 78), (65.5, 42.6112), (65.5, 52)),
]


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


def cubic(p0, p1, p2, p3, n=200):
    for i in range(n + 1):
        t = i / n
        u = 1 - t
        yield (u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0],
               u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1])


def swash_mask() -> Image.Image:
    """The logo's swash at CANVAS size: MARK of the body's height, centred."""
    s = SS
    pts = []
    for seg in SWASH:
        pts += list(cubic(*seg))[1 if pts else 0:]
    xs, ys = [x for x, _ in pts], [y for _, y in pts]
    unit = MARK * BODY / (max(ys) - min(ys))
    mid_x, mid_y = (min(xs) + max(xs)) / 2, (min(ys) + max(ys)) / 2
    big = Image.new("L", (CANVAS * s, CANVAS * s), 0)
    ImageDraw.Draw(big).polygon(
        [((CANVAS / 2 + (x - mid_x) * unit) * s, (CANVAS / 2 + (y - mid_y) * unit) * s) for x, y in pts], fill=255
    )
    return big.resize((CANVAS, CANVAS), Image.LANCZOS)


def render() -> Image.Image:
    """The full 1024x1024 icon (RGBA)."""
    mask = body_mask()

    # Drop shadow: soft, offset down, kept subtle.
    shadow_alpha = mask.filter(ImageFilter.GaussianBlur(22)).point(lambda v: int(v * 0.32))
    shadow_alpha = ImageChops.offset(shadow_alpha, 0, 12)
    out = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    out.paste(Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 255)), (0, 0), shadow_alpha)

    body = Image.new("RGBA", (CANVAS, CANVAS), BLUE + (255,))
    body.paste(Image.new("RGBA", (CANVAS, CANVAS), GREY + (255,)), (0, 0), swash_mask())

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

    master = render()

    png_1024 = out_dir / "AppIcon-1024.png"
    master.save(png_1024, optimize=True)

    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for name, px in ICONSET:
            img = master if px == CANVAS else master.resize((px, px), Image.LANCZOS)
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
