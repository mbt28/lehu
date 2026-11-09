#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Draws LEHU's faceplate logo: a single-DIN head unit of the early 2000s.

    python3 assets/logo/faceplate.py

writes lehu-faceplate.svg (amber) and lehu-faceplate-cyan.svg beside this
script. Units are millimetres: the front is 150 x 50, narrower than a
real single-DIN one (180 x 50).

The layout follows the radios of that time: a knob at each end (volume on
the left, select on the right), the display in the middle with two keys on
each side (seek down and up on the left, home and back on the right), and
the preset keys 1 to 6 under it. Legends are lit in the display's colour,
as at night.
"""
from pathlib import Path

W, H = 150.0, 50.0

# The display's picture: a play mark and a pixel penguin.
PICTURE = """
.......#.........###.........
.......##.......#####........
.......###......#.#.#........
.......####.....#####........
.......#####...##...##.......
.......####...###...###......
.......###....###...###......
.......##......##...##.......
.......#........##.##........
"""

# Digits 1-6 as strokes in a 1 x 1.5 box, y down.
DIGITS = {
    "1": "M0.2 0.35 L0.6 0 V1.5",
    "2": "M0 0.35 C0 -0.05 1 -0.1 1 0.4 C1 0.8 0 1 0 1.5 H1",
    "3": "M0 0.15 C0.35 -0.08 1 -0.05 1 0.38 C1 0.62 0.75 0.7 0.4 0.7 "
         "C0.8 0.7 1 0.85 1 1.1 C1 1.6 0.3 1.6 0 1.35",
    "4": "M0.75 1.5 V0 L0 1.05 H1",
    "5": "M0.95 0 H0.1 L0 0.68 C0.3 0.52 1 0.48 1 1.05 C1 1.62 0.25 1.6 0 1.35",
    "6": "M0.9 0.12 C0.55 -0.08 0 0.05 0 0.85 C0 1.62 1 1.62 1 1.02 "
         "C1 0.48 0.2 0.5 0 0.9",
}

PLATE = "#1a1e23"
PLATE_EDGE = "#56606b"
KEY = "#2a3139"
KEY_EDGE = "#11151a"
KEY_TOP = "#3a434d"
WELL = "#0b0e12"


def f(v):
    return f"{v:.2f}".rstrip("0").rstrip(".")


def scaled(path, x, y, s):
    """Scales and moves a path written in a unit box."""
    out = []
    for tok in path.replace(",", " ").split():
        if tok[0].isalpha():
            cmd, tok = tok[0], tok[1:]
            out.append(cmd)
            if not tok:
                continue
        out.append(tok)
    # Coordinates alternate x, y except after H (x only) and V (y only).
    res, axis, cmd = [], 0, ""
    for tok in out:
        if tok.isalpha():
            cmd = tok
            res.append(tok)
            axis = 1 if cmd == "V" else 0
            continue
        v = float(tok)
        if axis == 0:
            res.append(f(x + v * s))
        else:
            res.append(f(y + v * s))
        if cmd not in "HV":
            axis ^= 1
    return " ".join(res)


def key(out, x, y, w, h):
    out.append(f'<rect x="{f(x)}" y="{f(y)}" width="{f(w)}" height="{f(h)}" rx="1.2" '
               f'fill="url(#key)" stroke="{KEY_EDGE}" stroke-width="0.5"/>')
    out.append(f'<path d="M{f(x + 1.2)} {f(y + 0.55)}H{f(x + w - 1.2)}" '
               f'stroke="{KEY_TOP}" stroke-width="0.4" stroke-linecap="round"/>')


def knob(out, cx, cy):
    out.append(f'<circle cx="{f(cx)}" cy="{f(cy)}" r="10" fill="{WELL}"/>')
    # Knurled grip: short ribs all round the rim.
    out.append(f'<circle cx="{f(cx)}" cy="{f(cy)}" r="8.7" fill="#232931" '
               f'stroke="#4b5560" stroke-width="1.5" stroke-dasharray="0.6 0.76"/>')
    out.append(f'<circle cx="{f(cx)}" cy="{f(cy)}" r="6.7" fill="url(#cap)" '
               f'stroke="#0f1215" stroke-width="0.4"/>')


def svg(lit, unlit):
    o = []
    o.append(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="-1 -1 {f(W + 2)} {f(H + 2)}" '
             'role="img" aria-label="LEHU">')
    o.append("<defs>"
             '<linearGradient id="plate" x1="0" y1="0" x2="0" y2="1">'
             '<stop offset="0" stop-color="#22272d"/><stop offset="1" stop-color="#15181c"/>'
             "</linearGradient>"
             '<linearGradient id="key" x1="0" y1="0" x2="0" y2="1">'
             f'<stop offset="0" stop-color="#303841"/><stop offset="1" stop-color="{KEY}"/>'
             "</linearGradient>"
             '<radialGradient id="cap" cx="0.4" cy="0.35" r="0.75">'
             '<stop offset="0" stop-color="#3a424c"/><stop offset="1" stop-color="#1c2127"/>'
             "</radialGradient>"
             '<linearGradient id="glass" x1="0" y1="0" x2="0" y2="1">'
             '<stop offset="0" stop-color="#ffffff" stop-opacity="0.06"/>'
             '<stop offset="0.45" stop-color="#ffffff" stop-opacity="0"/>'
             "</linearGradient>"
             "</defs>")

    # Plate.
    o.append(f'<rect x="0" y="0" width="{f(W)}" height="{f(H)}" rx="3.5" fill="url(#plate)" '
             f'stroke="{PLATE_EDGE}" stroke-width="0.9"/>')
    o.append(f'<path d="M3.5 1.1H{f(W - 3.5)}" stroke="#39414a" stroke-width="0.5" '
             'stroke-linecap="round"/>')

    # Knobs: volume on the left, select on the right.
    knob(o, 15, 25)
    knob(o, W - 15, 25)

    # Display: the picture centred in a wider matrix.
    rows = [r for r in PICTURE.strip().splitlines()]
    cols, pad = 33, 2
    pitch, dot = 1.9, 1.45
    mw, mh = cols * pitch, len(rows) * pitch
    dx0, dy0 = (W - mw) / 2, 11.75
    wx, wy, ww, wh = dx0 - 2.6, dy0 - 2.6, mw + 5.2, mh + 5.2
    o.append(f'<rect x="{f(wx - 0.9)}" y="{f(wy - 0.9)}" width="{f(ww + 1.8)}" '
             f'height="{f(wh + 1.8)}" rx="2.2" fill="#101317"/>')
    o.append(f'<rect x="{f(wx)}" y="{f(wy)}" width="{f(ww)}" height="{f(wh)}" rx="1.4" '
             f'fill="{WELL}"/>')
    for j, row in enumerate(rows):
        for i in range(cols):
            k = i - pad
            on = 0 <= k < len(row) and row[k] == "#"
            x = dx0 + i * pitch + (pitch - dot) / 2
            y = dy0 + j * pitch + (pitch - dot) / 2
            o.append(f'<rect x="{f(x)}" y="{f(y)}" width="{f(dot)}" height="{f(dot)}" '
                     f'rx="0.35" fill="{lit if on else unlit}"/>')
    o.append(f'<rect x="{f(wx)}" y="{f(wy)}" width="{f(ww)}" height="{f(wh)}" rx="1.4" '
             'fill="url(#glass)"/>')

    # Keys beside the display.
    kw, kh = 9.0, (wh - 2) / 2
    lx, rx = wx - 2.6 - kw, wx + ww + 2.6
    stroke = f'fill="none" stroke="{lit}" stroke-width="0.55" stroke-linecap="round" stroke-linejoin="round"'
    for x in (lx, rx):
        key(o, x, wy, kw, kh)
        key(o, x, wy + kh + 2, kw, kh)
    # Home.
    cx, cy = rx + kw / 2, wy + kh / 2 + 0.2
    o.append(f'<path d="M{f(cx - 2)} {f(cy)}L{f(cx)} {f(cy - 1.8)}L{f(cx + 2)} {f(cy)} '
             f'M{f(cx - 1.4)} {f(cy - 0.5)}V{f(cy + 1.7)}H{f(cx + 1.4)}V{f(cy - 0.5)}" {stroke}/>')
    # Back.
    cy = wy + kh + 2 + kh / 2
    o.append(f'<path d="M{f(cx - 1)} {f(cy - 1.9)}L{f(cx - 2)} {f(cy - 0.9)}L{f(cx - 1)} {f(cy + 0.1)} '
             f'M{f(cx - 2)} {f(cy - 0.9)}H{f(cx + 0.6)}C{f(cx + 2.2)} {f(cy - 0.9)} {f(cx + 2.2)} '
             f'{f(cy + 1.9)} {f(cx + 0.6)} {f(cy + 1.9)}H{f(cx - 1)}" {stroke}/>')
    # Seek down and up.
    cx = lx + kw / 2
    for n, sign in ((0, -1), (1, 1)):
        cy = wy + n * (kh + 2) + kh / 2
        a, b = cx - 1.9 * sign, cx + 1.9 * sign
        m = cx
        o.append(f'<path d="M{f(a)} {f(cy - 1.3)}L{f(m)} {f(cy)}L{f(a)} {f(cy + 1.3)}Z '
                 f'M{f(m)} {f(cy - 1.3)}L{f(b)} {f(cy)}L{f(m)} {f(cy + 1.3)}Z" '
                 f'fill="{lit}" stroke="{lit}" stroke-width="0.25" stroke-linejoin="round"/>')
        o.append(f'<path d="M{f(b)} {f(cy - 1.3)}V{f(cy + 1.3)}" fill="none" stroke="{lit}" '
                 'stroke-width="0.5" stroke-linecap="round"/>')

    # Preset keys 1-6 under the display, spanning the side keys too.
    px0, px1 = lx, rx + kw
    gap, py, ph = 2.0, wy + wh + 3.2, 6.2
    pw = (px1 - px0 - 5 * gap) / 6
    for n in range(6):
        x = px0 + n * (pw + gap)
        key(o, x, py, pw, ph)
        s = 2.2
        o.append(f'<path d="{scaled(DIGITS[str(n + 1)], x + pw / 2 - s / 2, py + ph / 2 - 0.75 * s + 0.15, s)}" '
                 f'fill="none" stroke="{lit}" stroke-width="0.5" stroke-linecap="round" '
                 'stroke-linejoin="round"/>')

    o.append("</svg>")
    return "\n".join(o) + "\n"


here = Path(__file__).resolve().parent
(here / "lehu-faceplate.svg").write_text(svg("#f2a21c", "#1a2028"))
(here / "lehu-faceplate-cyan.svg").write_text(svg("#5fdcff", "#1a2028"))
