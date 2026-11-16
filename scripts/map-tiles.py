#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Render an OpenStreetMap extract into the map tiles LEHU shows, on a PC.

    scripts/map-tiles.py EXTRACT.osm.pbf OUT [--zoom 8-16] [--name NAME]
                         [--bbox W,S,E,N] [--area Z0-Z1:W,S,E,N ...]
                         [--jobs N] [--quality 80] [--font FONT.ttf]
                         [--labels draw|data|both|none] [--label-zoom 8-18]
                         [--no-tiles] [--night] [--pack [DEST]] [--pack-only]
                         [--progress]

OUT gets one 256 x 256 baseline JPEG per tile, OUT/<z>/<x>/<y>.jpg, and
OUT/map.toml, which tells LEHU the zoom levels, the area and where to open.
--pack then packs them into DEST/tiles.pmtiles (PMTiles v3) and the label
data into DEST/labels.pack, the files of a region folder; scripts/
map-region.py builds a whole region this way (see docs/map.md). The extract comes from a provider of OSM data such as
download.geofabrik.de; the tiles are drawn here, never fetched from
OpenStreetMap's tile servers, whose policy forbids offline use.

The drawing is deliberately plain: land use, water, buildings, roads and
railways, with the names of streets and places. Labels are placed once for
the whole map at each zoom, before any tile is drawn, so they never
collide and a label across a tile's edge continues on the next tile. The
font is LEHU's own (crates/lehu-ui/ui/fonts/font.ttf) unless --font names
another. It needs pyosmium and Pillow
(python3 -m venv v && v/bin/pip install osmium pillow).

With --labels data the names are not drawn into the tiles but written as
data, for LEHU to draw itself at the size the panel wants and upright
whichever way the map is turned: OUT/labels/<z>/<bx>/<by>.txt, one file a
block of 8 x 8 tiles (bx, by: the block's first tile's x and y, divided by
8), placed for every zoom from the first to two past the last (the zooms
LEHU enlarges tiles to), or those --label-zoom names. Each line is a
label, its fields apart by tabs:

    kind  x  y  angle  size  text

kind is s (a street) or p (a place); x and y the middle of the label's
box, in pixels of the zoom (whole, from the world's top left); angle the
street's way in degrees, clockwise from east, -89 to 90 (0 for a place);
size the text's size in pixels (its em); text the name, UTF-8. Lines
starting with # are comments. Each label lies in the block of its middle,
and none reaches more than LABEL_REACH pixels from it. A block without
labels has no file; OUT/labels/<z>/areas.txt lists the blocks each pass
covered, a line "bx0 by0 bx1 by1" (first to last, both included) a pass.
Where a zoom has neither a block's file nor the block in its areas, LEHU
draws the labels of a zoom or two out, so a pass over a smaller area at
deeper zooms (--bbox) can follow one over the whole extract, its blocks
replacing the first pass's. --no-tiles writes only the label data, for
tiles drawn before. The box is the
text's advance (no kerning) by its size, from 0.75 of the size above the
baseline to 0.25 below, with LABEL_HALO pixels of halo round it; labels of
a zoom keep LABEL_GAP pixels apart, and a place keeps clear the square
round its box's circle, so it can stay level on a turned map. The data is
the same for the day and the night tiles: LEHU picks the colours.

Map data (c) OpenStreetMap contributors, ODbL 1.0: LEHU shows the
attribution with the map.
"""

import argparse
import gc
import hashlib
import json
import math
import multiprocessing
import os
import sys
import struct
import time
from array import array

import osmium
from PIL import Image, ImageDraw, ImageFont

TILE = 256
SS = 2  # drawn at twice the size and reduced: smooth edges
WORLD_Z = 22  # map.toml's coordinates: world pixels at this zoom

LAND = (242, 239, 233)

# Areas: (colour, the zoom from which they are drawn).
AREAS = {
    "forest": ((173, 209, 158), 8),
    "grass": ((205, 235, 176), 10),
    "farmland": ((238, 240, 213), 10),
    "residential": ((224, 223, 223), 10),
    "industrial": ((235, 219, 232), 11),
    "commercial": ((242, 218, 217), 11),
    "park": ((200, 250, 204), 11),
    "cemetery": ((170, 203, 175), 13),
    "water": ((170, 211, 223), 8),
    "building": ((217, 208, 201), 14),
}
BUILDING_EDGE = (196, 182, 171)

LANDUSE = {
    "forest": "forest",
    "meadow": "grass",
    "grass": "grass",
    "village_green": "grass",
    "recreation_ground": "grass",
    "farmland": "farmland",
    "farmyard": "farmland",
    "orchard": "farmland",
    "vineyard": "farmland",
    "residential": "residential",
    "industrial": "industrial",
    "railway": "industrial",
    "commercial": "commercial",
    "retail": "commercial",
    "cemetery": "cemetery",
    "reservoir": "water",
    "basin": "water",
}
NATURAL = {
    "wood": "forest",
    "scrub": "grass",
    "heath": "grass",
    "grassland": "grass",
    "water": "water",
    "wetland": "grass",
}
LEISURE = {"park": "park", "garden": "park", "pitch": "park", "golf_course": "grass"}

# Roads: (fill, casing, width in pixels at zoom 8 and at 16, first zoom),
# drawn from the first entry to the last, so major roads end up on top.
ROADS = [
    ("path", (250, 128, 114), None, 0.6, 1.2, 15),
    ("service", (255, 255, 255), (190, 190, 190), 0.8, 3.0, 14),
    ("minor", (255, 255, 255), (190, 190, 190), 1.0, 6.0, 12),
    ("tertiary", (255, 255, 255), (170, 170, 170), 1.0, 8.0, 10),
    ("secondary", (247, 250, 191), (190, 180, 120), 1.2, 9.0, 9),
    ("primary", (252, 214, 164), (200, 150, 90), 1.4, 10.0, 8),
    ("trunk", (249, 178, 156), (200, 110, 90), 1.6, 11.0, 6),
    ("motorway", (232, 146, 162), (200, 90, 110), 1.8, 12.0, 6),
]
HIGHWAY = {
    "motorway": "motorway", "motorway_link": "motorway",
    "trunk": "trunk", "trunk_link": "trunk",
    "primary": "primary", "primary_link": "primary",
    "secondary": "secondary", "secondary_link": "secondary",
    "tertiary": "tertiary", "tertiary_link": "tertiary",
    "residential": "minor", "unclassified": "minor", "living_street": "minor",
    "road": "minor",
    "service": "service",
    "footway": "path", "path": "path", "cycleway": "path", "track": "path",
    "pedestrian": "path", "steps": "path", "bridleway": "path",
}
ROAD_ORDER = {r[0]: i for i, r in enumerate(ROADS)}
RAIL = ((150, 150, 150), 1.0, 3.0, 10)
WATERWAY = {"river": (1.0, 8.0, 9), "canal": (1.0, 6.0, 11), "stream": (0.6, 2.0, 13)}
WATER = AREAS["water"][0]

# Spatial buckets: features by the tiles they touch at these zooms; a tile
# reads the bucket of the deepest level at or above its own zoom.
LEVELS = (6, 10, 12, 14)

FEATURES = []  # (layer, kind, minzoom, bbox, rings or line), shared by fork
BUCKETS = {}

# Labels. Street names from these zooms, by road class; places by kind:
# (first zoom, last zoom, text size at 1x, priority: lower first).
STREET_NAMES = {
    "motorway": 13, "trunk": 13, "primary": 13, "secondary": 14,
    "tertiary": 14, "minor": 15, "service": 17, "path": 17,
}
PLACES = {
    "city": (8, 14, 15, 0),
    "town": (10, 15, 13, 1),
    "village": (12, 16, 12, 2),
    "suburb": (13, 15, 12, 3),
    "hamlet": (14, 16, 11, 4),
}
STREET_SIZE = 12  # pixels at 1x
STREET_TEXT = (60, 60, 60)
PLACE_TEXT = (40, 40, 40)
HALO = (255, 255, 255)
LABEL_GAP = 6  # pixels kept clear around every label, at 1x
REPEAT = 320  # pixels between two labels of the same street, at 1x
DEFAULT_FONT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "crates",
                            "lehu-ui", "ui", "fonts", "font.ttf")

STREETS = []  # (road class, name, line in zoom-0 pixels)
PLACE_NODES = []  # (kind, name, point in zoom-0 pixels)
LABELS = {}  # zoom -> [(text, size, colour, centre (2x zoom pixels), angle, bbox)]
LABEL_BUCKETS = {}  # zoom -> {(tile x, tile y): [label index]}
FONT_PATH = DEFAULT_FONT

# Names as data (--labels data), drawn by LEHU at these sizes in pixels on
# the panel, whatever the zoom.
LABEL_STREET_SIZE = 22
LABEL_PLACE_SIZE = {"city": 24, "town": 24, "village": 22, "suburb": 22, "hamlet": 20}
LABEL_HALO = 2  # LEHU's halo round the text
LABEL_GAP = 6  # pixels kept clear between two labels
LABEL_REPEAT = 400  # pixels between two labels of the same street
LABEL_REACH = 512  # no part of a label further than this from its middle
LABEL_BLOCK = 3  # a file holds 2**LABEL_BLOCK x 2**LABEL_BLOCK tiles
OVERZOOM = 2  # zooms LEHU goes past the last, the tiles enlarged
DATA_LABELS = {}  # zoom -> [(kind, text, size, centre (zoom pixels), angle)]

# --progress: machine-readable lines on stdout (@stage, @progress), the
# rest on stderr. scripts/map-region.py and the PC tool read them.
PROGRESS = False


def say(text):
    """A line for people: stdout, or stderr with --progress."""
    print(text, file=sys.stderr if PROGRESS else sys.stdout, flush=True)


def emit(line):
    """A line for a program (--progress only): "@stage tiles",
    "@progress 10 200"."""
    if PROGRESS:
        print(line, flush=True)


def stop_on_term():
    """SIGTERM (a cancel): the workers stopped too, and out at once; what
    was written stays, partial."""
    import signal

    def stop(signum, frame):
        for child in multiprocessing.active_children():
            child.terminate()
        os._exit(128 + signum)

    signal.signal(signal.SIGTERM, stop)


def night():
    """The night palette: dark land and areas, roads lighter than the land
    and keeping their colours (dimmed), light names on dark halos."""
    global LAND, BUILDING_EDGE, ROADS, RAIL, WATER, STREET_TEXT, PLACE_TEXT, HALO
    LAND = (30, 34, 40)
    dark = {
        "forest": (30, 48, 38),
        "grass": (34, 50, 40),
        "farmland": (36, 40, 40),
        "residential": (40, 43, 50),
        "industrial": (46, 42, 52),
        "commercial": (50, 42, 48),
        "park": (32, 54, 42),
        "cemetery": (34, 46, 40),
        "water": (20, 38, 60),
        "building": (54, 57, 66),
    }
    for kind, colour in dark.items():
        AREAS[kind] = (colour, AREAS[kind][1])
    BUILDING_EDGE = (68, 72, 82)
    colours = {
        "path": ((130, 80, 72), None),
        "service": ((78, 84, 94), (52, 56, 64)),
        "minor": ((88, 94, 104), (52, 56, 64)),
        "tertiary": ((100, 106, 116), (56, 60, 70)),
        "secondary": ((128, 124, 86), (70, 68, 50)),
        "primary": ((156, 122, 74), (96, 74, 44)),
        "trunk": ((166, 104, 84), (104, 64, 52)),
        "motorway": ((164, 92, 110), (104, 56, 70)),
    }
    ROADS = [(r[0], *colours[r[0]], *r[3:]) for r in ROADS]
    RAIL = ((90, 90, 96), *RAIL[1:])
    WATER = AREAS["water"][0]
    STREET_TEXT = (200, 204, 210)
    PLACE_TEXT = (226, 228, 232)
    HALO = (24, 26, 30)


def world(lon, lat):
    """Longitude and latitude to world pixels at zoom 0 (0..256)."""
    lat = max(-85.05112878, min(85.05112878, lat))
    s = math.sin(math.radians(lat))
    x = (lon + 180.0) / 360.0 * TILE
    y = (0.5 - math.log((1 + s) / (1 - s)) / (4 * math.pi)) * TILE
    return x, y


def ring(nodes):
    """A way's points in zoom-0 pixels, x and y by turns in one array of
    doubles: a tenth of the memory of a list of pairs, and one object, so
    the forked workers share it rather than copy it as they read it."""
    pts = array("d")
    for n in nodes:
        if n.location.valid():
            pts.extend(world(n.lon, n.lat))
    return pts


def pairs(pts):
    """The (x, y) pairs of a point array."""
    it = iter(pts)
    return list(zip(it, it))


def bbox(pts):
    xs = pts[0::2]
    ys = pts[1::2]
    return (min(xs), min(ys), max(xs), max(ys))


class Loader(osmium.SimpleHandler):
    def __init__(self, labels):
        super().__init__()
        self.labels = labels

    def node(self, n):
        if not self.labels:
            return
        kind = n.tags.get("place")
        name = n.tags.get("name")
        if kind in PLACES and name and n.location.valid():
            PLACE_NODES.append((kind, name, world(n.lon, n.lat)))

    def area(self, a):
        tags = a.tags
        kind = None
        if "building" in tags and tags["building"] != "no":
            kind = "building"
        elif tags.get("landuse") in LANDUSE:
            kind = LANDUSE[tags["landuse"]]
        elif tags.get("natural") in NATURAL:
            kind = NATURAL[tags["natural"]]
        elif tags.get("leisure") in LEISURE:
            kind = LEISURE[tags["leisure"]]
        elif tags.get("waterway") == "riverbank":
            kind = "water"
        if kind is None:
            return
        rings = []
        try:
            for outer in a.outer_rings():
                pts = ring(outer)
                if len(pts) < 6:
                    continue
                holes = [h for h in (ring(i) for i in a.inner_rings(outer)) if len(h) >= 6]
                rings.append((pts, holes))
        except osmium.InvalidLocationError:
            return
        if not rings:
            return
        boxes = [bbox(r[0]) for r in rings]
        box = (min(b[0] for b in boxes), min(b[1] for b in boxes),
               max(b[2] for b in boxes), max(b[3] for b in boxes))
        layer = 2 if kind == "building" else (1 if kind == "water" else 0)
        FEATURES.append((layer, kind, AREAS[kind][1], box, rings))

    def way(self, w):
        tags = w.tags
        hw = tags.get("highway")
        kind = None
        if hw in HIGHWAY and tags.get("area") != "yes":
            kind = ("road", HIGHWAY[hw])
            minzoom = ROADS[ROAD_ORDER[kind[1]]][5]
            layer = 3
        elif tags.get("railway") in ("rail", "light_rail", "narrow_gauge") and \
                tags.get("service") is None:
            kind = ("rail", None)
            minzoom = RAIL[3]
            layer = 4
        elif tags.get("waterway") in WATERWAY:
            kind = ("waterway", tags["waterway"])
            minzoom = WATERWAY[tags["waterway"]][2]
            layer = 1
        if kind is None:
            return
        try:
            pts = ring(w.nodes)
        except osmium.InvalidLocationError:
            return
        if len(pts) < 4:
            return
        FEATURES.append((layer, kind, minzoom, bbox(pts), pts))
        if self.labels and kind[0] == "road" and tags.get("name"):
            STREETS.append((kind[1], tags["name"], pts))


def tile_range(box, z):
    n = 2 ** z
    x0 = max(0, int(box[0] * n / TILE))
    y0 = max(0, int(box[1] * n / TILE))
    x1 = min(n - 1, int(box[2] * n / TILE))
    y1 = min(n - 1, int(box[3] * n / TILE))
    return x0, y0, x1, y1


def index():
    for level in LEVELS:
        b = {}
        for i, f in enumerate(FEATURES):
            # Only what some zoom this level serves draws.
            deeper = [l for l in LEVELS if l > level]
            if deeper and f[2] >= deeper[0]:
                continue
            x0, y0, x1, y1 = tile_range(f[3], level)
            for x in range(x0, x1 + 1):
                for y in range(y0, y1 + 1):
                    b.setdefault((x, y), []).append(i)
        BUCKETS[level] = b


def width(lo, hi, z):
    """A line's width at zoom z, from its widths at 8 and 16."""
    t = (z - 8) / 8.0
    if t <= 0:
        return lo * 2 ** (z - 8) if z < 8 else lo
    return lo * (hi / lo) ** min(t, 1.25)


class Grid:
    """Boxes placed so far, hashed by cells, for overlap tests."""

    CELL = 256

    def __init__(self):
        self.cells = {}

    def _keys(self, b):
        c = self.CELL
        for gx in range(int(b[0] // c), int(b[2] // c) + 1):
            for gy in range(int(b[1] // c), int(b[3] // c) + 1):
                yield gx, gy

    def free(self, b):
        for k in self._keys(b):
            for o in self.cells.get(k, ()):
                if b[0] < o[2] and o[0] < b[2] and b[1] < o[3] and o[1] < b[3]:
                    return False
        return True

    def add(self, b):
        for k in self._keys(b):
            self.cells.setdefault(k, []).append(b)


def rotated_box(cx, cy, w, h, angle):
    """The upright box around a w x h rectangle centred at (cx, cy), turned by
    `angle` degrees."""
    a = math.radians(angle)
    c, s = abs(math.cos(a)), abs(math.sin(a))
    hw = (w * c + h * s) / 2
    hh = (w * s + h * c) / 2
    return (cx - hw, cy - hh, cx + hw, cy + hh)


def straight_run(pts, length, max_turn=22):
    """Along a line (zoom pixels), the middle and the angle of a stretch at
    least `length` long whose direction stays within `max_turn` degrees of
    its chord, the stretch nearest the line's middle first; None if there
    is none."""
    if len(pts) < 2:
        return None
    seg = []
    total = 0.0
    for p, q in zip(pts, pts[1:]):
        d = math.hypot(q[0] - p[0], q[1] - p[1])
        seg.append(d)
        total += d
    if total < length:
        return None
    best = None
    for i in range(len(pts) - 1):
        run = 0.0
        for j in range(i, len(pts) - 1):
            run += seg[j]
            if run < length:
                continue
            a, b = pts[i], pts[j + 1]
            chord = math.atan2(b[1] - a[1], b[0] - a[0])
            ok = True
            for k in range(i, j + 1):
                p, q = pts[k], pts[k + 1]
                if seg[k] == 0:
                    continue
                t = math.atan2(q[1] - p[1], q[0] - p[0]) - chord
                t = (t + math.pi) % (2 * math.pi) - math.pi
                if abs(math.degrees(t)) > max_turn:
                    ok = False
                    break
            if ok:
                mid = ((a[0] + b[0]) / 2, (a[1] + b[1]) / 2)
                # Prefer the stretch nearest the middle of the line.
                at = sum(seg[:i]) + run / 2
                score = abs(at - total / 2)
                if best is None or score < best[0]:
                    best = (score, mid, chord)
            break
    if best is None:
        return None
    angle = math.degrees(best[2])
    # Upright: text reads from left to right.
    if angle > 90:
        angle -= 180
    elif angle <= -90:
        angle += 180
    return best[1], angle


def place_labels(z0, z1):
    """Every zoom's labels: places, then streets major first, none
    overlapping another and a street's name not repeated too close."""
    fonts = {}

    def font(size):
        if size not in fonts:
            fonts[size] = ImageFont.truetype(FONT_PATH, size * SS)
        return fonts[size]

    order = {r[0]: i for i, r in enumerate(reversed(ROADS))}
    streets = sorted(STREETS, key=lambda s: (order[s[0]], -len(s[2])))
    places = sorted(PLACE_NODES, key=lambda p: PLACES[p[0]][3])
    for z in range(z0, z1 + 1):
        scale = 2 ** z * SS
        grid = Grid()
        placed = []
        gap = LABEL_GAP * SS
        for kind, name, (x, y) in places:
            first, last, size, _ = PLACES[kind]
            if not first <= z <= last:
                continue
            f = font(size)
            l, t, r, b = f.getbbox(name, stroke_width=2 * SS)
            w, h = r - l, b - t
            cx, cy = x * scale, y * scale
            box = (cx - w / 2 - gap, cy - h / 2 - gap, cx + w / 2 + gap, cy + h / 2 + gap)
            if grid.free(box):
                grid.add(box)
                placed.append((name, size, PLACE_TEXT, (cx, cy), 0.0, box))
        last_at = {}
        f = font(STREET_SIZE)
        for cls, name, pts in streets:
            if z < STREET_NAMES[cls]:
                continue
            l, t, r, b = f.getbbox(name, stroke_width=2 * SS)
            w, h = r - l, b - t
            line = [(px_ * scale, py_ * scale) for px_, py_ in pairs(pts)]
            found = straight_run(line, w * 1.15)
            if found is None:
                continue
            (cx, cy), angle = found
            near = last_at.get(name, ())
            if any(math.hypot(cx - px, cy - py) < REPEAT * SS for px, py in near):
                continue
            box = rotated_box(cx, cy, w + 2 * gap, h + 2 * gap, angle)
            if not grid.free(box):
                continue
            grid.add(box)
            last_at.setdefault(name, []).append((cx, cy))
            placed.append((name, STREET_SIZE, STREET_TEXT, (cx, cy), angle, box))
        LABELS[z] = placed
        bucket = {}
        side = TILE * SS
        for i, lab in enumerate(placed):
            b = lab[5]
            for tx in range(int(b[0] // side), int(b[2] // side) + 1):
                for ty in range(int(b[1] // side), int(b[3] // side) + 1):
                    bucket.setdefault((tx, ty), []).append(i)
        LABEL_BUCKETS[z] = bucket
        say(f"  zoom {z}: {len(placed)} labels")


def place_label_data(zooms, area, tick=lambda: None):
    """Every zoom's labels as data, for LEHU: placed as `place_labels` does,
    at the sizes LEHU draws them, only those near `area` (zoom-0 pixels)."""
    fonts = {}

    def advance(text, size):
        # Glyph by glyph, no kerning: as LEHU lays text out.
        if size not in fonts:
            fonts[size] = ImageFont.truetype(FONT_PATH, size)
        f = fonts[size]
        return sum(f.getlength(ch) for ch in text)

    order = {r[0]: i for i, r in enumerate(reversed(ROADS))}
    streets = sorted(STREETS, key=lambda s: (order[s[0]], -len(s[2])))
    places = sorted(PLACE_NODES, key=lambda p: PLACES[p[0]][3])
    gap, halo = LABEL_GAP, LABEL_HALO
    for z in zooms:
        scale = 2 ** z
        # The blocks over the area, whole, and as far again as a label
        # reaches: every label of theirs placed as a pass over the whole
        # extract would.
        x0, y0, x1, y1 = tile_range(area, z)
        side = (TILE << LABEL_BLOCK) / scale
        b = 1 << LABEL_BLOCK
        m = LABEL_REACH / scale
        near = ((x0 // b) * side - m, (y0 // b) * side - m,
                (x1 // b + 1) * side + m, (y1 // b + 1) * side + m)
        grid = Grid()
        placed = []
        for kind, name, (x, y) in places:
            first, last, _, _ = PLACES[kind]
            if not first <= z <= last or not (near[0] <= x <= near[2] and near[1] <= y <= near[3]):
                continue
            size = LABEL_PLACE_SIZE[kind]
            w, h = advance(name, size) + 2 * halo, size + 2 * halo
            # Level on the panel however the map turns: it keeps clear the
            # square round the circle its box turns in.
            r = math.hypot(w, h) / 2 + gap
            if r > LABEL_REACH:
                continue
            cx, cy = x * scale, y * scale
            box = (cx - r, cy - r, cx + r, cy + r)
            if grid.free(box):
                grid.add(box)
                placed.append(("p", name, size, (cx, cy), 0))
        last_at = {}
        size = LABEL_STREET_SIZE
        for cls, name, pts in streets:
            if z < STREET_NAMES[cls]:
                continue
            b = bbox(pts)
            if b[2] < near[0] or b[0] > near[2] or b[3] < near[1] or b[1] > near[3]:
                continue
            w, h = advance(name, size) + 2 * halo, size + 2 * halo
            if math.hypot(w, h) / 2 > LABEL_REACH:
                continue
            line = [(px_ * scale, py_ * scale) for px_, py_ in pairs(pts)]
            found = straight_run(line, w * 1.15)
            if found is None:
                continue
            (cx, cy), angle = found
            angle = round(angle)
            if angle <= -90:
                angle += 180
            seen = last_at.get(name, ())
            if any(math.hypot(cx - px, cy - py) < LABEL_REPEAT for px, py in seen):
                continue
            box = rotated_box(cx, cy, w + 2 * gap, h + 2 * gap, angle)
            if not grid.free(box):
                continue
            grid.add(box)
            last_at.setdefault(name, []).append((cx, cy))
            placed.append(("s", name, size, (cx, cy), angle))
        DATA_LABELS[z] = placed
        say(f"  zoom {z}: {len(placed)} labels as data")
        tick()


def write_label_data(out, zooms, area):
    """The labels of `zooms` as files under OUT/labels, one a block of
    tiles, for the blocks over `area` (zoom-0 pixels) that have any,
    replacing those there; the blocks covered added to the zoom's
    areas.txt."""
    side = TILE << LABEL_BLOCK
    files = 0
    size = 0
    for z in zooms:
        root = os.path.join(out, "labels", str(z))
        x0, y0, x1, y1 = tile_range(area, z)
        bx0, by0, bx1, by1 = x0 >> LABEL_BLOCK, y0 >> LABEL_BLOCK, x1 >> LABEL_BLOCK, y1 >> LABEL_BLOCK
        blocks = {(bx, by): [] for bx in range(bx0, bx1 + 1) for by in range(by0, by1 + 1)}
        for kind, text, tsize, (cx, cy), angle in DATA_LABELS[z]:
            bx, by = int(cx // side), int(cy // side)
            if not (bx0 <= bx <= bx1 and by0 <= by <= by1):
                continue
            text = " ".join(text.split())
            blocks.setdefault((bx, by), []).append(
                f"{kind}\t{round(cx)}\t{round(cy)}\t{angle}\t{tsize}\t{text}\n")
        for (bx, by), lines in blocks.items():
            d = os.path.join(root, str(bx))
            path = os.path.join(d, f"{by}.txt")
            if not lines:
                # None here now: an earlier pass's go.
                if os.path.exists(path):
                    os.remove(path)
                continue
            os.makedirs(d, exist_ok=True)
            data = ("# lehu-labels 1\n" + "".join(lines)).encode()
            with open(path + ".tmp", "wb") as f:
                f.write(data)
            os.replace(path + ".tmp", path)
            files += 1
            size += len(data)
        os.makedirs(root, exist_ok=True)
        areas = os.path.join(root, "areas.txt")
        line = f"{bx0} {by0} {bx1} {by1}\n"
        old = open(areas).read().splitlines(keepends=True) if os.path.exists(areas) else []
        if line not in old:
            with open(areas + ".tmp", "w") as f:
                f.write("".join(old) + line)
            os.replace(areas + ".tmp", areas)
    say(f"label data: {files} files, {size // 1024} KiB")


_LABEL_IMAGES = {}


def label_image(z, i):
    """A label's text with its halo, turned, as an RGBA picture (2x)."""
    key = (z, i)
    if key not in _LABEL_IMAGES:
        name, size, colour, _, angle, _ = LABELS[z][i]
        f = ImageFont.truetype(FONT_PATH, size * SS)
        stroke = 2 * SS
        l, t, r, b = f.getbbox(name, stroke_width=stroke)
        im = Image.new("RGBA", (r - l, b - t), (0, 0, 0, 0))
        d = ImageDraw.Draw(im)
        d.text((-l, -t), name, font=f, fill=colour + (255,), stroke_width=stroke,
               stroke_fill=HALO + (255,))
        if angle:
            im = im.rotate(-angle, resample=Image.BICUBIC, expand=True)
        _LABEL_IMAGES[key] = im
    return _LABEL_IMAGES[key]


def draw_labels(img, z, x, y):
    side = TILE * SS
    for i in LABEL_BUCKETS.get(z, {}).get((x, y), ()):
        cx, cy = LABELS[z][i][3]
        im = label_image(z, i)
        at = (round(cx - x * side - im.width / 2), round(cy - y * side - im.height / 2))
        img.paste(im, at, im)


def render(job):
    z, x, y, out, quality = job
    level = max(l for l in LEVELS if l <= z) if z >= LEVELS[0] else None
    if level is None:
        ids = range(len(FEATURES))
    else:
        s = 2 ** (z - level)
        ids = BUCKETS[level].get((x // s, y // s), [])
    scale = 2 ** z * SS
    ox = x * TILE * SS
    oy = y * TILE * SS
    size = TILE * SS
    # The tile's box in zoom-0 pixels, a little wider for line widths.
    pad = 16 / 2 ** z
    tb = (x * TILE / 2 ** z - pad, y * TILE / 2 ** z - pad,
          (x + 1) * TILE / 2 ** z + pad, (y + 1) * TILE / 2 ** z + pad)
    feats = []
    for i in ids:
        f = FEATURES[i]
        if f[2] > z:
            continue
        b = f[3]
        if b[2] < tb[0] or b[0] > tb[2] or b[3] < tb[1] or b[1] > tb[3]:
            continue
        # Specks: an area smaller than a pixel at this zoom is left out.
        if f[0] in (0, 2) and (b[2] - b[0]) * 2 ** z < 0.6 and (b[3] - b[1]) * 2 ** z < 0.6:
            continue
        feats.append(f)

    def px(points):
        it = iter(points)
        return [(x_ * scale - ox, y_ * scale - oy) for x_, y_ in zip(it, it)]

    img = Image.new("RGB", (size, size), LAND)
    draw = ImageDraw.Draw(img)

    def area(rings, colour, edge=None):
        for outer, holes in rings:
            if not holes:
                draw.polygon(px(outer), fill=colour, outline=edge)
                continue
            mask = Image.new("L", (size, size), 0)
            m = ImageDraw.Draw(mask)
            m.polygon(px(outer), fill=255)
            for h in holes:
                m.polygon(px(h), fill=0)
            img.paste(colour, (0, 0), mask)

    # Land use, then water, then buildings.
    for layer in (0, 1, 2):
        for f in feats:
            if f[0] != layer:
                continue
            if layer == 1 and f[1][0] == "waterway":
                lo, hi, _ = WATERWAY[f[1][1]]
                draw.line(px(f[4]), fill=WATER, width=max(1, round(width(lo, hi, z) * SS)),
                          joint="curve")
                continue
            if layer == 1 and isinstance(f[1], tuple):
                continue
            colour = AREAS[f[1]][0]
            area(f[4], colour, BUILDING_EDGE if f[1] == "building" and z >= 16 else None)

    # Roads: every casing first, then the fills, minor to major.
    roads = sorted((f for f in feats if f[0] == 3), key=lambda f: ROAD_ORDER[f[1][1]])
    for f in roads:
        r = ROADS[ROAD_ORDER[f[1][1]]]
        if r[2] is None or z < 12:
            continue
        w = width(r[3], r[4], z) * SS
        draw.line(px(f[4]), fill=r[2], width=max(1, round(w + 2 * SS)), joint="curve")
    for f in roads:
        r = ROADS[ROAD_ORDER[f[1][1]]]
        w = width(r[3], r[4], z) * SS
        draw.line(px(f[4]), fill=r[1], width=max(1, round(w)), joint="curve")
    for f in feats:
        if f[0] == 4:
            w = width(RAIL[1], RAIL[2], z) * SS
            draw.line(px(f[4]), fill=RAIL[0], width=max(1, round(w)), joint="curve")

    draw_labels(img, z, x, y)
    img = img.resize((TILE, TILE), Image.LANCZOS)
    d = os.path.join(out, str(z), str(x))
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, f"{y}.jpg")
    img.save(path + ".tmp", "JPEG", quality=quality, subsampling=2, optimize=True,
             progressive=False)
    os.replace(path + ".tmp", path)
    return 1


# Packed: the tiles in one PMTiles v3 file, the label data in another,
# so a region is a few big files on the card rather than thousands of small
# ones (each at least a block of its own, looked up one directory at a
# time). See docs/map.md, "Data on the card".
PMTILES = "tiles.pmtiles"
LABELS_PACK = "labels.pack"
PM_HEADER = 127
PM_ROOT_MAX = 16384 - PM_HEADER  # header and root directory: the first 16 KiB
PM_LEAF = 1024  # entries a leaf directory, at least: small to read on the board


def tile_id(z, x, y):
    """PMTiles' tile id: the tiles of every zoom before, then (x, y)'s
    place along the zoom's Hilbert curve."""
    acc = ((1 << (2 * z)) - 1) // 3
    n = 1 << z
    s = n >> 1
    while s > 0:
        rx = 1 if x & s else 0
        ry = 1 if y & s else 0
        acc += s * s * ((3 * rx) ^ ry)
        if ry == 0:
            if rx == 1:
                x = n - 1 - x
                y = n - 1 - y
            x, y = y, x
        s >>= 1
    return acc


def varint(out, v):
    while v >= 0x80:
        out.append((v & 0x7F) | 0x80)
        v >>= 7
    out.append(v)


def pm_directory(entries):
    """A PMTiles directory, uncompressed: (tile id, offset, length, run)."""
    b = bytearray()
    varint(b, len(entries))
    last = 0
    for e in entries:
        varint(b, e[0] - last)
        last = e[0]
    for e in entries:
        varint(b, e[3])
    for e in entries:
        varint(b, e[2])
    for i, e in enumerate(entries):
        if i > 0 and e[1] == entries[i - 1][1] + entries[i - 1][2]:
            varint(b, 0)
        else:
            varint(b, e[1] + 1)
    return bytes(b)


def pm_directories(entries):
    """The root directory and the leaf directories after it, the root
    within the first 16 KiB with the header."""
    root = pm_directory(entries)
    if len(root) <= PM_ROOT_MAX:
        return root, b""
    size = PM_LEAF
    while True:
        leaves = bytearray()
        top = []
        for i in range(0, len(entries), size):
            leaf = pm_directory(entries[i:i + size])
            top.append((entries[i][0], len(leaves), len(leaf), 0))
            leaves += leaf
        root = pm_directory(top)
        if len(root) <= PM_ROOT_MAX:
            return root, bytes(leaves)
        size *= 2


def read_map_toml(out):
    import tomllib
    with open(os.path.join(out, "map.toml"), "rb") as f:
        return tomllib.load(f)


def degrees(wx, wy):
    """World pixels at WORLD_Z to longitude and latitude."""
    k = TILE * 2 ** WORLD_Z
    lon = wx / k * 360.0 - 180.0
    lat = math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * wy / k))))
    return lon, lat


def loose_tiles(out):
    """OUT's loose tiles: (tile id, path, zoom)."""
    tiles = []
    for zd in os.listdir(out):
        if not zd.isdigit() or not os.path.isdir(os.path.join(out, zd)):
            continue
        z = int(zd)
        for xd in os.listdir(os.path.join(out, zd)):
            if not xd.isdigit():
                continue
            for name in os.listdir(os.path.join(out, zd, xd)):
                stem, ext = os.path.splitext(name)
                if ext == ".jpg" and stem.isdigit():
                    tiles.append((tile_id(z, int(xd), int(stem)),
                                  os.path.join(out, zd, xd, name), z))
    return tiles


def write_pmtiles(out, dest, meta):
    """OUT's loose tiles (<z>/<x>/<y>.jpg) as DEST/tiles.pmtiles: PMTiles
    v3, JPEG tiles, nothing compressed (the board reads the directories
    as they lie), clustered (the tiles in tile id order), a tile the same
    as another (the sea, empty land) stored once."""
    tiles = loose_tiles(out)
    if not tiles:
        sys.exit(f"no tiles in {out} to pack")
    tiles.sort()
    path = os.path.join(dest, PMTILES)
    data_path = path + ".data.tmp"
    seen = {}
    entries = []
    size = 0
    with open(data_path, "wb") as data:
        for i, (tid, tpath, _) in enumerate(tiles):
            if i % 1000 == 0:
                emit(f"@progress {i} {len(tiles)}")
            with open(tpath, "rb") as f:
                body = f.read()
            h = hashlib.sha256(body).digest()
            if h not in seen:
                seen[h] = (size, len(body))
                data.write(body)
                size += len(body)
            off, length = seen[h]
            last = entries[-1] if entries else None
            if last and last[0] + last[3] == tid and last[1] == off:
                entries[-1] = (last[0], last[1], last[2], last[3] + 1)
            else:
                entries.append((tid, off, length, 1))
    root, leaves = pm_directories(entries)
    left, top, right, bottom = meta["bounds"]
    w, n = degrees(left, top)
    e, s = degrees(right, bottom)
    cx, cy = degrees(*meta["center"])
    info = json.dumps({
        "name": meta["name"],
        "format": "jpg",
        "type": "baselayer",
        "attribution": meta.get("attribution", ""),
        "description": "LEHU map tiles, drawn by scripts/map-tiles.py",
    }, ensure_ascii=False).encode()
    zooms = sorted({t[2] for t in tiles})
    md_at = PM_HEADER + len(root)
    leaf_at = md_at + len(info)
    data_at = leaf_at + len(leaves)

    def e7(v):
        return int(round(v * 1e7))

    header = b"PMTiles" + bytes([3]) + struct.pack(
        "<QQQQQQQQQQQBBBBBBiiiiBii",
        PM_HEADER, len(root), md_at, len(info), leaf_at, len(leaves), data_at, size,
        len(tiles), len(entries), len(seen),
        1,  # clustered
        1,  # internal compression: none
        1,  # tile compression: none
        3,  # tile type: JPEG
        zooms[0], zooms[-1],
        e7(w), e7(s), e7(e), e7(n),
        min(max(meta.get("start-zoom", zooms[0]), zooms[0]), zooms[-1]), e7(cx), e7(cy))
    assert len(header) == PM_HEADER
    with open(path + ".tmp", "wb") as f:
        f.write(header + root + info + leaves)
        with open(data_path, "rb") as data:
            while True:
                chunk = data.read(1 << 20)
                if not chunk:
                    break
                f.write(chunk)
    os.remove(data_path)
    os.replace(path + ".tmp", path)
    emit(f"@progress {len(tiles)} {len(tiles)}")
    say(f"{PMTILES}: {len(tiles)} tiles, {len(seen)} different, {len(entries)} entries, "
        f"{len(leaves) // 1024} KiB of leaf directories, "
        f"{os.path.getsize(path) / 1e6:.1f} MB")


def write_labels_pack(out, dest):
    """OUT's label data (labels/<z>/<bx>/<by>.txt and each zoom's
    areas.txt) as DEST/labels.pack, each file's text as it is, behind an
    index; false when OUT has none. Little-endian 32-bit words, so the
    board maps it and reads it where it lies:

        "LEHULABP", the version (1), the number of entries; then the
        entries, five words each, in order: z + 256 x kind (0 a block,
        1 the zoom's areas.txt), bx, by (0 for areas.txt), the text's byte
        offset from the start of the file and its length; then the text,
        the file padded to a whole word."""
    root = os.path.join(out, "labels")
    if not os.path.isdir(root):
        return False
    items = []
    for zd in os.listdir(root):
        if not zd.isdigit():
            continue
        z = int(zd)
        zp = os.path.join(root, zd)
        if os.path.exists(os.path.join(zp, "areas.txt")):
            items.append(((z | 256, 0, 0), os.path.join(zp, "areas.txt")))
        for bxd in os.listdir(zp):
            if not bxd.isdigit():
                continue
            for name in os.listdir(os.path.join(zp, bxd)):
                stem, ext = os.path.splitext(name)
                if ext == ".txt" and stem.isdigit():
                    items.append(((z, int(bxd), int(stem)), os.path.join(zp, bxd, name)))
    items.sort()
    at = 16 + 20 * len(items)
    index = bytearray()
    body = bytearray()
    for (k, bx, by), path in items:
        with open(path, "rb") as f:
            text = f.read()
        index += struct.pack("<5I", k, bx, by, at + len(body), len(text))
        body += text
    blob = b"LEHULABP" + struct.pack("<II", 1, len(items)) + index + body
    blob += b"\0" * (-len(blob) % 4)
    path = os.path.join(dest, LABELS_PACK)
    with open(path + ".tmp", "wb") as f:
        f.write(blob)
    os.replace(path + ".tmp", path)
    say(f"{LABELS_PACK}: {len(items)} entries, {len(blob) // 1024} KiB")
    return True


def pack(out, dest):
    """OUT's tiles and label data packed into DEST, map.toml beside them."""
    os.makedirs(dest, exist_ok=True)
    meta = read_map_toml(out)
    write_pmtiles(out, dest, meta)
    write_labels_pack(out, dest)
    if os.path.abspath(dest) != os.path.abspath(out):
        with open(os.path.join(out, "map.toml"), "rb") as f:
            text = f.read()
        with open(os.path.join(dest, "map.toml.tmp"), "wb") as f:
            f.write(text)
        os.replace(os.path.join(dest, "map.toml.tmp"), os.path.join(dest, "map.toml"))


def zoom_range(text, top, what):
    a, b = (int(v) for v in text.split("-"))
    if not 0 <= a <= b <= top:
        sys.exit(f"{what}: first-last, 0..{top}")
    return a, b


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("extract")
    ap.add_argument("out")
    ap.add_argument("--zoom", default="8-16", help="first-last zoom, e.g. 8-16")
    ap.add_argument("--name", default=None)
    ap.add_argument("--bbox", default=None, help="W,S,E,N in degrees; the extract's own if absent")
    ap.add_argument("--area", action="append", default=[], metavar="Z0-Z1:W,S,E,N",
                    help="more zooms over a smaller area (a city deeper), drawn in the same run "
                         "after the rest; again for more areas")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 1)
    ap.add_argument("--quality", type=int, default=80)
    ap.add_argument("--font", default=DEFAULT_FONT, help="the labels' TrueType font")
    ap.add_argument("--labels", choices=("draw", "data", "both", "none"), default="draw",
                    help="street and place names drawn into the tiles, written as data "
                         "for LEHU to draw (labels/), both, or none")
    ap.add_argument("--no-labels", action="store_true", help="the same as --labels none")
    ap.add_argument("--label-zoom", default=None,
                    help="first-last zoom of the label data; the tiles' first to two past "
                         "their last if absent")
    ap.add_argument("--no-tiles", action="store_true",
                    help="only the label data, for tiles drawn before (no tiles, no map.toml)")
    ap.add_argument("--night", action="store_true",
                    help="the night palette (LEHU shows the set in <map>/night at night)")
    ap.add_argument("--pack", nargs="?", const="", default=None, metavar="DEST",
                    help=f"then pack OUT's tiles and label data into DEST/{PMTILES} and "
                         f"DEST/{LABELS_PACK} (DEST: OUT when not given)")
    ap.add_argument("--pack-only", action="store_true",
                    help="only pack what OUT has, as --pack does (the extract is not read)")
    ap.add_argument("--progress", action="store_true",
                    help="machine-readable progress on stdout: @stage tiles, labels or pack, "
                         "@progress DONE TOTAL; the rest on stderr")
    a = ap.parse_args()
    global PROGRESS
    PROGRESS = a.progress
    stop_on_term()
    dest = a.pack or a.out
    if a.pack_only:
        emit("@stage pack")
        pack(a.out, dest)
        return
    if a.night:
        night()
    global FONT_PATH
    FONT_PATH = a.font
    mode = "none" if a.no_labels else a.labels
    labels = mode != "none"
    if labels and not os.path.exists(FONT_PATH):
        sys.exit(f"no font at {FONT_PATH}: give --font")
    z0, z1 = zoom_range(a.zoom, 19, "zoom")
    if a.label_zoom:
        l0, l1 = zoom_range(a.label_zoom, 22, "label-zoom")
    else:
        l0, l1 = z0, z1 + OVERZOOM

    t = time.time()
    if a.bbox:
        w, s, e, n = (float(v) for v in a.bbox.split(","))
    else:
        box = osmium.io.Reader(a.extract).header().box()
        if not box.valid():
            sys.exit("the extract has no bounding box: give --bbox")
        w, s, e, n = box.bottom_left.lon, box.bottom_left.lat, box.top_right.lon, box.top_right.lat
    left, top = world(w, n)
    right, bottom = world(e, s)
    # Each pass: its tiles' zooms, its labels' zooms and its area (zoom-0
    # pixels). The first is the whole map; each --area one more, its
    # labels placed again over it as for the first, replacing the first's.
    passes = [((z0, z1), (l0, l1), (left, top, right, bottom))]
    for spec in a.area:
        zs, _, bb = spec.partition(":")
        a0, a1 = zoom_range(zs, 19, "area zoom")
        aw, as_, ae, an = (float(v) for v in bb.split(","))
        al, at_ = world(aw, an)
        ar, ab = world(ae, as_)
        passes.append(((a0, a1), (a0, min(a1 + OVERZOOM, 22)), (al, at_, ar, ab)))
    top_zoom = max(p[0][1] for p in passes)
    emit("@stage tiles")
    emit("@progress 0 1")
    say(f"reading {a.extract}")
    # Nothing read is freed before the end: the collector would only walk
    # every feature (and in the workers, write to the pages it walks).
    gc.disable()
    Loader(labels).apply_file(a.extract, locations=True, idx="flex_mem")
    say(f"{len(FEATURES)} features, {len(STREETS)} named streets, "
        f"{len(PLACE_NODES)} places in {time.time() - t:.0f} s")
    index()
    if mode in ("draw", "both"):
        place_labels(z0, top_zoom)


    def label_data():
        # The names LEHU draws: placed after the tiles, which do not need
        # them.
        if mode not in ("data", "both"):
            return
        emit("@stage labels")
        total = sum(b1 - b0 + 1 for (_, (b0, b1), _) in passes)
        done = [0]

        def tick():
            done[0] += 1
            emit(f"@progress {done[0]} {total}")

        emit(f"@progress 0 {total}")
        for (_, (b0, b1), area) in passes:
            zooms = range(b0, b1 + 1)
            place_label_data(zooms, area, tick)
            write_label_data(a.out, zooms, area)

    if a.no_tiles:
        label_data()
        if a.pack is not None:
            emit("@stage pack")
            pack(a.out, dest)
        say(f"done in {time.time() - t:.0f} s")
        return
    jobs = []
    seen = set()
    for ((p0, p1), _, area) in passes:
        for z in range(p0, p1 + 1):
            x0, y0, x1, y1 = tile_range(area, z)
            for x in range(x0, x1 + 1):
                for y in range(y0, y1 + 1):
                    if (z, x, y) not in seen:
                        seen.add((z, x, y))
                        jobs.append((z, x, y, a.out, a.quality))
    del seen
    say(f"{len(jobs)} tiles, zoom {z0}-{top_zoom}, {a.jobs} jobs")
    os.makedirs(a.out, exist_ok=True)
    done = 0
    # What the workers read stays where the fork left it, shared.
    gc.freeze()
    emit(f"@progress 0 {len(jobs)}")
    with multiprocessing.get_context("fork").Pool(a.jobs) as pool:
        for n_ in pool.imap_unordered(render, jobs, chunksize=16):
            done += n_
            if done % 200 == 0 or done == len(jobs):
                emit(f"@progress {done} {len(jobs)}")
            if done % 2000 == 0:
                say(f"  {done}/{len(jobs)}  {time.time() - t:.0f} s")

    k = 2 ** WORLD_Z
    cx, cy = (left + right) / 2, (top + bottom) / 2
    name = a.name or os.path.basename(a.extract).split(".")[0]
    with open(os.path.join(a.out, "map.toml.tmp"), "w") as f:
        f.write(f'# Written by scripts/map-tiles.py from {os.path.basename(a.extract)}.\n')
        f.write(f'name = "{name}"\n')
        f.write('format = "jpg"\n')
        f.write(f"tile-size = {TILE}\n")
        f.write(f"min-zoom = {z0}\n")
        f.write(f"max-zoom = {top_zoom}\n")
        f.write(f"# World pixels at zoom {WORLD_Z}: the area, and the view to open at.\n")
        f.write(f"bounds = [{int(left * k)}, {int(top * k)}, {int(right * k)}, {int(bottom * k)}]\n")
        f.write(f"center = [{int(cx * k)}, {int(cy * k)}]\n")
        f.write(f"start-zoom = {min(max(z0, 12), top_zoom)}\n")
        f.write('attribution = "© OpenStreetMap contributors"\n')
        f.write(f"# Where there is no tile.\nland = [{LAND[0]}, {LAND[1]}, {LAND[2]}]\n")
    os.replace(os.path.join(a.out, "map.toml.tmp"), os.path.join(a.out, "map.toml"))
    label_data()
    if a.pack is not None:
        emit("@stage pack")
        pack(a.out, dest)
    say(f"done in {time.time() - t:.0f} s")


if __name__ == "__main__":
    main()
