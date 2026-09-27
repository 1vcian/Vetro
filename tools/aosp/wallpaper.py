#!/usr/bin/env python3
"""Default wallpaper of the Vetro AOSP image (ADR 0030).

Writes a square PNG (1920x1920, center-cropped both vertically and
horizontally) with a midnight blue -> teal gradient and two lighter diagonal
panes, "glass" with no text or trademarks. Deterministic: same bytes on
every run with the same zlib. Standard library only.

Usage: tools/aosp/wallpaper.py [OUTPUT]
  (default: guest/aosp/device/vetro/vetro_arm64/branding/wallpaper.png)
"""
import os
import struct
import sys
import zlib

SIZE = 1920
TOP = (0x0B, 0x1F, 0x2A)     # midnight blue
BOTTOM = (0x1F, 0x6F, 0x78)  # teal
# Panes: (distance from the diagonal as a fraction of the side, thickness, white opacity).
PANES = ((-0.18, 0.22, 0.07), (0.20, 0.10, 0.05))


def pixel_row(y):
    t = y / (SIZE - 1)
    base = [TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3)]
    row = bytearray()
    for x in range(SIZE):
        # Signed distance from the bottom-left to top-right diagonal.
        d = (x + y - SIZE) / SIZE
        a = 0.0
        for c, w, op in PANES:
            e = abs(d - c) / (w / 2)
            if e < 1.0:
                # Soft edge: solid in the middle, fades over the last 15%.
                a += op * min(1.0, (1.0 - e) / 0.15)
        row += bytes(int(base[i] + (255 - base[i]) * a + 0.5) for i in range(3))
    return row


def png(path):
    raw = bytearray()
    prev = None
    for y in range(SIZE):
        row = pixel_row(y)
        if prev is None:
            raw += b"\x00" + row
        else:
            # Up filter: adjacent rows are nearly equal, compresses very well.
            raw += b"\x02" + bytes((row[i] - prev[i]) & 0xFF for i in range(len(row)))
        prev = row

    def chunk(kind, data):
        c = struct.pack(">I", len(data)) + kind + data
        return c + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 2, 0, 0, 0)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    print(f"{path}: {len(data)} bytes")


if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    default = os.path.join(here, "..", "..", "guest", "aosp", "device", "vetro", "vetro_arm64", "branding", "wallpaper.png")
    png(os.path.normpath(sys.argv[1] if len(sys.argv) > 1 else default))
