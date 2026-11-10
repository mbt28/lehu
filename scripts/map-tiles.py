#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Render an OpenStreetMap extract into the map tiles LEHU shows, on a PC.

    scripts/map-tiles.py EXTRACT.osm.pbf OUT [--zoom 8-16] [--name NAME]
                         [--bbox W,S,E,N] [--jobs N] [--quality 80]

OUT gets one 256 x 256 baseline JPEG per tile, OUT/<z>/<x>/<y>.jpg, and
OUT/map.toml, which tells LEHU the zoom levels, the area and where to open.
Copy OUT to the head unit's map directory (map.dir, /var/lib/lehu/maps by
default). The extract comes from a provider of OSM data such as
download.geofabrik.de; the tiles are drawn here, never fetched from
OpenStreetMap's tile servers, whose policy forbids offline use.

The drawing is deliberately plain: land use, water, buildings, roads and
railways, no labels yet. It needs pyosmium and Pillow
(python3 -m venv v && v/bin/pip install osmium pillow).

Map data (c) OpenStreetMap contributors, ODbL 1.0: LEHU shows the
attribution with the map.
"""

import argparse
import math
import multiprocessing
import os
import sys
import time

import osmium
from PIL import Image, ImageDraw

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


def world(lon, lat):
    """Longitude and latitude to world pixels at zoom 0 (0..256)."""
    lat = max(-85.05112878, min(85.05112878, lat))
    s = math.sin(math.radians(lat))
    x = (lon + 180.0) / 360.0 * TILE
    y = (0.5 - math.log((1 + s) / (1 - s)) / (4 * math.pi)) * TILE
    return x, y


def ring(nodes):
    pts = []
    for n in nodes:
        if n.location.valid():
            pts.append(world(n.lon, n.lat))
    return pts


def bbox(points):
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    return (min(xs), min(ys), max(xs), max(ys))


class Loader(osmium.SimpleHandler):
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
                if len(pts) < 3:
                    continue
                holes = [h for h in (ring(i) for i in a.inner_rings(outer)) if len(h) >= 3]
                rings.append((pts, holes))
        except osmium.InvalidLocationError:
            return
        if not rings:
            return
        box = bbox([p for r in rings for p in r[0]])
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
        if len(pts) < 2:
            return
        FEATURES.append((layer, kind, minzoom, bbox(pts), pts))


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
        return [((p[0] * scale - ox), (p[1] * scale - oy)) for p in points]

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

    img = img.resize((TILE, TILE), Image.LANCZOS)
    d = os.path.join(out, str(z), str(x))
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, f"{y}.jpg")
    img.save(path + ".tmp", "JPEG", quality=quality, subsampling=2, optimize=True,
             progressive=False)
    os.replace(path + ".tmp", path)
    return 1


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("extract")
    ap.add_argument("out")
    ap.add_argument("--zoom", default="8-16", help="first-last zoom, e.g. 8-16")
    ap.add_argument("--name", default=None)
    ap.add_argument("--bbox", default=None, help="W,S,E,N in degrees; the extract's own if absent")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 1)
    ap.add_argument("--quality", type=int, default=80)
    a = ap.parse_args()
    z0, z1 = (int(v) for v in a.zoom.split("-"))
    if not 0 <= z0 <= z1 <= 19:
        sys.exit("zoom: first-last, 0..19")

    t = time.time()
    if a.bbox:
        w, s, e, n = (float(v) for v in a.bbox.split(","))
    else:
        box = osmium.io.Reader(a.extract).header().box()
        if not box.valid():
            sys.exit("the extract has no bounding box: give --bbox")
        w, s, e, n = box.bottom_left.lon, box.bottom_left.lat, box.top_right.lon, box.top_right.lat
    print(f"reading {a.extract}", flush=True)
    Loader().apply_file(a.extract, locations=True, idx="flex_mem")
    print(f"{len(FEATURES)} features in {time.time() - t:.0f} s", flush=True)
    index()

    left, top = world(w, n)
    right, bottom = world(e, s)
    jobs = []
    for z in range(z0, z1 + 1):
        x0, y0, x1, y1 = tile_range((left, top, right, bottom), z)
        for x in range(x0, x1 + 1):
            for y in range(y0, y1 + 1):
                jobs.append((z, x, y, a.out, a.quality))
    print(f"{len(jobs)} tiles, zoom {z0}-{z1}, {a.jobs} jobs", flush=True)
    os.makedirs(a.out, exist_ok=True)
    done = 0
    with multiprocessing.get_context("fork").Pool(a.jobs) as pool:
        for n_ in pool.imap_unordered(render, jobs, chunksize=16):
            done += n_
            if done % 2000 == 0:
                print(f"  {done}/{len(jobs)}", flush=True)

    k = 2 ** WORLD_Z
    cx, cy = (left + right) / 2, (top + bottom) / 2
    name = a.name or os.path.basename(a.extract).split(".")[0]
    with open(os.path.join(a.out, "map.toml.tmp"), "w") as f:
        f.write(f'# Written by scripts/map-tiles.py from {os.path.basename(a.extract)}.\n')
        f.write(f'name = "{name}"\n')
        f.write('format = "jpg"\n')
        f.write(f"tile-size = {TILE}\n")
        f.write(f"min-zoom = {z0}\n")
        f.write(f"max-zoom = {z1}\n")
        f.write(f"# World pixels at zoom {WORLD_Z}: the area, and the view to open at.\n")
        f.write(f"bounds = [{int(left * k)}, {int(top * k)}, {int(right * k)}, {int(bottom * k)}]\n")
        f.write(f"center = [{int(cx * k)}, {int(cy * k)}]\n")
        f.write(f"start-zoom = {min(max(z0, 12), z1)}\n")
        f.write('attribution = "© OpenStreetMap contributors"\n')
    os.replace(os.path.join(a.out, "map.toml.tmp"), os.path.join(a.out, "map.toml"))
    print(f"done in {time.time() - t:.0f} s", flush=True)


if __name__ == "__main__":
    main()
