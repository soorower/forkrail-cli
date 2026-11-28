#!/usr/bin/env python3
"""Forkrail.app's icon, AppIcon.icns: the logo as an app tile (white squares on black, the grid 60%
of the tile), on macOS's icon grid (an 824 px rounded square in 1024). Needs Pillow.

    python3 deploy/macos/icon.py deploy/macos/AppIcon.icns
"""

import io
import struct
import sys

from PIL import Image, ImageDraw

# The logo's 11 squares on its 4x4 grid (brand/forkrail-logo-*.svg in forkrail-web), by column and row.
SQUARES = [(0, 0), (1, 0), (2, 0), (3, 0), (0, 1), (1, 1), (3, 1), (0, 2), (2, 2), (0, 3), (3, 3)]
# In the logo's units: each square 20.76 wide, 25.38 apart, corners 4.04; the grid is 96.9 across.
SIDE, STEP, CORNER, GRID = 20.76, 25.38, 4.04, 96.9


def draw(size):
    scale = 4  # drawn large, then scaled down: smooth edges
    px = size * scale
    image = Image.new("RGBA", (px, px), (0, 0, 0, 0))
    d = ImageDraw.Draw(image)
    tile = px * 824 / 1024
    left = (px - tile) / 2
    d.rounded_rectangle([left, left, left + tile, left + tile], radius=tile * 0.225, fill=(10, 10, 10, 255))
    unit = tile * 0.6 / GRID
    start = left + (tile - GRID * unit) / 2
    for col, row in SQUARES:
        x, y = start + col * STEP * unit, start + row * STEP * unit
        d.rounded_rectangle([x, y, x + SIDE * unit, y + SIDE * unit], radius=CORNER * unit, fill=(255, 255, 255, 255))
    return image.resize((size, size), Image.LANCZOS)


def png(size):
    out = io.BytesIO()
    draw(size).save(out, format="PNG")
    return out.getvalue()


# icns entries that hold PNGs: type, pixels.
TYPES = [(b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024),
         (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512)]


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: icon.py OUT.icns")
    cache = {}
    body = b""
    for kind, size in TYPES:
        data = cache.setdefault(size, png(size))
        body += kind + struct.pack(">I", len(data) + 8) + data
    with open(sys.argv[1], "wb") as f:
        f.write(b"icns" + struct.pack(">I", len(body) + 8) + body)


if __name__ == "__main__":
    main()
