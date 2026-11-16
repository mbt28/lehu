#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""A screenshot of the map, composed from what LEHU wrote out.

The map is on the display's video plane, which no screenshot tool reads;
LEHU writes what it shows instead. Run LEHU with LEHU_SHOT_DIR=/some/dir
in its environment, and `touch /some/dir/take`: the next picture shown
lands there as shot.txt, shot-canvas.bin, shot-sprite.bin and shot-ui.bin.
Copy the folder to a PC and run

    scripts/map-shot.py DIR OUT.png

which draws the canvas as the panel showed it (the part shown, scaled to
the panel), the sprite (the arrow) over it, and the UI plane over both.
"""

import sys

import numpy as np
from PIL import Image


def main():
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    d, out = sys.argv[1], sys.argv[2]
    head = {}
    for line in open(f"{d}/shot.txt"):
        k, *v = line.split()
        head[k] = v
    pw, ph = (int(v) for v in head["panel"])
    cw, ch, pitch, chroma, fmt, full = head["canvas"]
    cw, ch, pitch, chroma = int(cw), int(ch), int(pitch), int(chroma)
    raw = np.frombuffer(open(f"{d}/shot-canvas.bin", "rb").read(), np.uint8)
    if fmt == "Nv12":
        y = raw[: ch * pitch].reshape(ch, pitch)[:, :cw].astype(np.float32)
        c = raw[chroma : chroma + (ch // 2) * pitch].reshape(ch // 2, pitch)[:, :cw]
        cb = c[:, 0::2].astype(np.float32).repeat(2, 0).repeat(2, 1)[:ch, :cw] - 128
        cr = c[:, 1::2].astype(np.float32).repeat(2, 0).repeat(2, 1)[:ch, :cw] - 128
        if full == "false":
            y = (y - 16) * 255 / 219
            cb, cr = cb * 255 / 224, cr * 255 / 224
        rgb = np.stack(
            [y + 1.402 * cr, y - 0.344136 * cb - 0.714136 * cr, y + 1.772 * cb], -1
        )
        pic = Image.fromarray(np.clip(rgb, 0, 255).astype(np.uint8), "RGB")
    elif fmt == "Rgb565":
        pic = Image.frombytes("RGB", (cw, ch), raw[: ch * pitch].tobytes(), "raw", "BGR;16", pitch)
    else:
        pic = Image.frombytes("RGB", (cw, ch), raw[: ch * pitch].tobytes(), "raw", "BGRX", pitch)
    sx, sy, sw, sh = (int(v) / 65536 for v in head["src"])
    pic = pic.crop((round(sx), round(sy), round(sx + sw), round(sy + sh)))
    if pic.size != (pw, ph):
        pic = pic.resize((pw, ph), Image.BILINEAR)
    pic = pic.convert("RGBA")
    if "sprite" in head:
        f = head["sprite"]
        w, h, spitch, ssx, ssy, sw, sh, x, y = (int(v) for v in f[:9])
        # The cursor's pixels are premultiplied, the sprite plane's not.
        mode = "BGRa" if len(f) > 9 and f[9] == "premultiplied" else "BGRA"
        raw = open(f"{d}/shot-sprite.bin", "rb").read()
        spr = Image.frombytes("RGBA", (w, h), raw[: h * spitch], "raw", mode, spitch)
        # Cut to the panel: the cursor can stand partly off it.
        cx, cy = max(0, -x), max(0, -y)
        part = spr.crop((ssx + cx, ssy + cy, ssx + sw, ssy + sh))
        pic.alpha_composite(part, (x + cx, y + cy))
    if "ui" in head:
        x0, y0, x1, y1, upitch, ufmt, bpp = head["ui"]
        x0, y0, x1, y1, upitch = int(x0), int(y0), int(x1), int(y1), int(upitch)
        raw = open(f"{d}/shot-ui.bin", "rb").read()
        # The format as DRM names it: RG16, AR24 or XR24.
        mode = "BGR;16" if "RG16" in ufmt else ("BGRA" if "AR24" in ufmt else "BGRX")
        kind = "RGBA" if "AR24" in ufmt else "RGB"
        ui = Image.frombytes(kind, (pw, ph), raw[: ph * upitch], "raw", mode, upitch)
        part = ui.crop((x0, y0, x1, y1)).convert("RGBA")
        pic.alpha_composite(part, (x0, y0))
    pic.convert("RGB").save(out)
    print(out, file=sys.stderr)


if __name__ == "__main__":
    main()
