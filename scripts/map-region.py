#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Build a whole map region for LEHU from one OpenStreetMap extract, on a PC.

    scripts/map-region.py EXTRACT.osm.pbf OUT --preset hungary
                          [--jobs N] [--work DIR] [--only tiles|graph|manifest]

OUT becomes a region folder, ready to copy to the card's LEHU-DATA
partition as /data/maps/<region>/ (see docs/map.md, "Data on the card"):

    map.toml        the region: its name, zooms and area, the data version,
                    the extract's date, the preset, and the size and sha256
                    of every other file
    tiles.pmtiles   the tiles (scripts/map-tiles.py --pack)
    labels.pack     the street and place names LEHU draws (the same)
    roads.bin, places.bin, and what else scripts/map-graph.py writes:
                    the roads to route over, the names to search

The tiles are drawn into a work directory first (OUT.work beside OUT unless
--work names another), as loose files, then packed into OUT; the work
directory can go once OUT is built. --only does one part (and the manifest
after it): tiles, graph, or only the manifest again.

A preset says which zooms are drawn where. A whole country at the deepest
zooms would be far too much for the card and days on a PC, so the deep
zooms are drawn only over towns:

    luxembourg   zoom 8-14 over the extract, 15-16 over Luxembourg City
    hungary      zoom 8-14 over the extract, 15-16 over Budapest and over
                 every city or town of 50,000 people or more (from the
                 extract's place nodes and their population tags)

Labels are always data (LEHU draws them), placed for the zooms the map
shows: to two past the deepest tiles.

It runs scripts/map-tiles.py and scripts/map-graph.py as they are, so it
needs what they need (python3 -m venv v && v/bin/pip install osmium
pillow). Map data (c) OpenStreetMap contributors, ODbL 1.0.
"""

import argparse
import datetime
import hashlib
import math
import os
import subprocess
import sys
import time
import tomllib

import osmium

HERE = os.path.dirname(os.path.abspath(__file__))

# The layout of a region folder LEHU reads: map.toml's data-version. A
# LEHU reads the versions up to its own (crates/lehu-map/src/region.rs) and
# says "Map needs a newer LEHU" for a later one.
DATA_VERSION = 1

# Cities drawn deeper in their own box (W, S, E, N), whatever their
# population tags say.
BUDAPEST = (18.925, 47.349, 19.335, 47.613)

PRESETS = {
    "luxembourg": {
        "name": "Luxembourg",
        "zoom": "8-14",
        "deep": "15-16",
        "boxes": [("Luxembourg", (6.02, 49.56, 6.22, 49.67))],
        "towns": None,
    },
    "hungary": {
        "name": "Hungary",
        "zoom": "8-14",
        "deep": "15-16",
        "boxes": [("Budapest", BUDAPEST)],
        "towns": 50_000,
    },
}

# A town's deep box: this far from its place node each way, kilometres,
# by its population.
TOWN_KM = ((100_000, 6.0), (0, 4.0))


def population(text):
    try:
        return int(text.replace(" ", "").replace(",", "").replace(".", ""))
    except (AttributeError, ValueError):
        return 0


def towns(extract, least, skip):
    """The cities and towns of `least` people or more, with the box drawn
    deeper round each: (name, (W, S, E, N)), the largest first; none inside
    the boxes of `skip`."""
    found = []
    fp = osmium.FileProcessor(extract, osmium.osm.NODE).with_filter(
        osmium.filter.KeyFilter("place"))
    for n in fp:
        if n.tags.get("place") not in ("city", "town") or not n.location.valid():
            continue
        pop = population(n.tags.get("population"))
        if pop < least:
            continue
        lon, lat = n.location.lon, n.location.lat
        if any(b[0] <= lon <= b[2] and b[1] <= lat <= b[3] for _, b in skip):
            continue
        km = next(k for p, k in TOWN_KM if pop >= p)
        dlat = km / 111.32
        dlon = km / (111.32 * math.cos(math.radians(lat)))
        found.append((pop, n.tags.get("name", "?"), (lon - dlon, lat - dlat, lon + dlon, lat + dlat)))
    found.sort(reverse=True)
    return [(name, box) for _, name, box in found]


def osm_date(extract):
    """The extract's date: its replication timestamp, else the file's."""
    try:
        stamp = osmium.io.Reader(extract, osmium.osm.osm_entity_bits.NOTHING).header().get(
            "osmosis_replication_timestamp")
    except Exception:  # noqa: BLE001 - a header it cannot read: the file's date
        stamp = ""
    if stamp:
        return stamp[:10]
    return datetime.date.fromtimestamp(os.path.getmtime(extract)).isoformat()


def run(args):
    print("+", " ".join(args), flush=True)
    subprocess.run(args, check=True)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def toml_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def manifest(out, work, preset, extract, name):
    """OUT/map.toml: the tile set's description (from the work directory's,
    else OUT's own) with the data version, the extract's date, the preset
    and every file's size and sha256."""
    source = os.path.join(work, "map.toml")
    if not os.path.exists(source):
        source = os.path.join(out, "map.toml")
    with open(source, "rb") as f:
        meta = tomllib.load(f)
    files = sorted(
        f for f in os.listdir(out)
        if f != "map.toml" and not f.startswith(".") and not f.endswith(".tmp")
        and os.path.isfile(os.path.join(out, f)))
    lines = [
        f"# Written by scripts/map-region.py from {os.path.basename(extract)}.",
        f"name = {toml_str(name or meta['name'])}",
        f"format = {toml_str(meta.get('format', 'jpg'))}",
        f"tile-size = {meta.get('tile-size', 256)}",
        f"min-zoom = {meta['min-zoom']}",
        f"max-zoom = {meta['max-zoom']}",
        "# World pixels at zoom 22: the area, and the view to open at.",
        "bounds = [" + ", ".join(str(v) for v in meta["bounds"]) + "]",
        "center = [" + ", ".join(str(v) for v in meta["center"]) + "]",
        f"start-zoom = {meta.get('start-zoom', meta['min-zoom'])}",
        f"attribution = {toml_str(meta.get('attribution', '© OpenStreetMap contributors'))}",
    ]
    if "land" in meta:
        lines.append("land = [" + ", ".join(str(v) for v in meta["land"]) + "]")
    lines += [
        "# The region folder's layout: LEHU refuses a later one than it reads.",
        f"data-version = {DATA_VERSION}",
        f"osm-date = {toml_str(osm_date(extract))}",
        f"preset = {toml_str(preset)}",
        "",
        "# Every other file: LEHU checks the sizes when it opens the map, the",
        "# sha256 once, in the background, for a region new to it.",
        "[files]",
    ]
    for f in files:
        path = os.path.join(out, f)
        lines.append(f"{toml_str(f)} = {{ size = {os.path.getsize(path)}, "
                     f"sha256 = {toml_str(sha256(path))} }}")
    text = "\n".join(lines) + "\n"
    tmp = os.path.join(out, "map.toml.tmp")
    with open(tmp, "w") as f:
        f.write(text)
    os.replace(tmp, os.path.join(out, "map.toml"))
    total = sum(os.path.getsize(os.path.join(out, f)) for f in files)
    print(f"map.toml: {len(files)} files, {total / 1e6:.1f} MB", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("extract")
    ap.add_argument("out")
    ap.add_argument("--preset", required=True, choices=sorted(PRESETS))
    ap.add_argument("--name", default=None, help="the region's name; the preset's if absent")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 1,
                    help="processes drawing tiles")
    ap.add_argument("--work", default=None, help="the loose tiles' directory (OUT.work)")
    ap.add_argument("--only", choices=("tiles", "graph", "manifest"), default=None)
    ap.add_argument("--python", default=sys.executable,
                    help="the Python with osmium and Pillow to run the other scripts")
    a = ap.parse_args()
    preset = PRESETS[a.preset]
    out = a.out.rstrip("/")
    work = a.work or out + ".work"
    os.makedirs(out, exist_ok=True)
    t = time.time()
    if a.only in (None, "tiles"):
        boxes = list(preset["boxes"])
        if preset["towns"]:
            more = towns(a.extract, preset["towns"], boxes)
            print(f"{len(more)} towns of {preset['towns']} people or more: "
                  + ", ".join(n for n, _ in more), flush=True)
            boxes += more
        z0, z1 = (int(v) for v in preset["zoom"].split("-"))
        d1 = int(preset["deep"].split("-")[1])
        args = [a.python, os.path.join(HERE, "map-tiles.py"), a.extract, work,
                "--zoom", preset["zoom"], "--labels", "data",
                "--label-zoom", f"{z0}-{max(z1, d1)}", "--jobs", str(a.jobs), "--pack", out]
        args += ["--name", a.name or preset["name"]]
        for _, (w, s, e, n) in boxes:
            args += ["--area", f"{preset['deep']}:{w:.5f},{s:.5f},{e:.5f},{n:.5f}"]
        run(args)
    if a.only in (None, "graph"):
        run([a.python, os.path.join(HERE, "map-graph.py"), a.extract, out])
    manifest(out, work, a.preset, a.extract, a.name or preset["name"])
    print(f"region {out} done in {time.time() - t:.0f} s", flush=True)


if __name__ == "__main__":
    main()
