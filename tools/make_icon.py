#!/usr/bin/env python3
"""Draws the application icon used by the installer and the executable.

The icon is generated rather than checked in as binary artwork so it can be reviewed,
adjusted and reproduced: a dark tile with a cluster of translucent coloured Gaussians,
which is what the application actually shows.

Windows wants a multi-size `.ico`: the installer, the taskbar and the window title bar
each pick a different size, and a single 32x32 image looks blurred everywhere else.

Usage:

    .python-runtime\\Scripts\\python.exe tools\\make_icon.py
"""

from __future__ import annotations

import math
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "src-tauri" / "icons"

# Sizes Windows actually asks for, largest last so the 256x256 stays the primary image.
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# The PNG sizes Tauri's bundle configuration references, and the base image they are drawn
# from. The base is deliberately much larger than any single icon: every other size is a
# downscale of it, so the curves stay clean at 256 and readable at 16.
BASE_SIZE = 1024
PNG_SIZES = {"32x32.png": 32, "64x64.png": 64, "128x128.png": 128, "128x128@2x.png": 256}

# The palette the viewer produces: a cloud of coloured, semi-transparent Gaussians.
COLORS = [
    (255, 138, 92),
    (255, 205, 96),
    (126, 231, 135),
    (91, 200, 255),
    (170, 140, 255),
    (255, 120, 170),
]
BACKGROUND = (16, 19, 22, 255)
TILE = (26, 31, 36, 255)


def draw(size: int) -> Image.Image:
    """Renders the icon at one size."""
    # A 4x supersample keeps the rounded corners and the soft dots clean when downscaled.
    scale = 4
    canvas = size * scale
    image = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    draw_ctx = ImageDraw.Draw(image)

    margin = canvas * 0.04
    radius = canvas * 0.22
    draw_ctx.rounded_rectangle(
        [margin, margin, canvas - margin, canvas - margin],
        radius=radius,
        fill=TILE,
        outline=BACKGROUND,
        width=max(1, int(canvas * 0.02)),
    )

    # A dense spiral of small dots: the same icon every run, no random seed to keep, and no
    # clumping. Overlapping translucent dots read as a mass, the way a splat cluster does.
    center = canvas / 2
    spread = canvas * 0.30
    dots = 30
    for index in range(dots):
        angle = index * math.pi * (3 - math.sqrt(5))
        distance = spread * math.sqrt((index + 0.5) / dots)
        x = center + math.cos(angle) * distance
        y = center + math.sin(angle) * distance
        # Small, tapering dots: the visual language of a splat cloud.
        dot_radius = canvas * (0.105 - 0.03 * (index / dots))
        color = COLORS[index % len(COLORS)]
        # Fade the outer dots so the cluster reads as depth rather than a flat blob.
        alpha = int(230 * (1.0 - 0.5 * (index / dots)))
        draw_ctx.ellipse(
            [x - dot_radius, y - dot_radius, x + dot_radius, y + dot_radius],
            fill=(*color, alpha),
        )

    return image.resize((size, size), Image.LANCZOS)


def main() -> int:
    ICONS.mkdir(parents=True, exist_ok=True)
    base = draw(BASE_SIZE)

    # The installer and the executable icon: a multi-size .ico, so the installer window, the
    # taskbar, the desktop shortcut and the title bar each get an image drawn for them.
    # Windows never upscales one size for another.
    target = ICONS / "icon.ico"
    base.save(target, format="ICO", sizes=[(size, size) for size in ICO_SIZES])
    print(f"wrote {target.name} ({target.stat().st_size} bytes, sizes {ICO_SIZES})")

    # The PNG set the bundle configuration lists, plus the base image itself for review.
    for name, size in PNG_SIZES.items():
        path = ICONS / name
        base.resize((size, size), Image.LANCZOS).save(path, format="PNG")
        print(f"wrote {name} ({path.stat().st_size} bytes)")
    source = ICONS / "icon.png"
    base.save(source, format="PNG")
    print(f"wrote {source.name} ({source.stat().st_size} bytes, {BASE_SIZE}x{BASE_SIZE})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
