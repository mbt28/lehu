#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Write the routing graph and the search index of an OpenStreetMap extract.

    scripts/map-graph.py EXTRACT.osm.pbf OUT_DIR

LEHU finds its own routes on the board (Map > Go to): it searches a name in
OUT_DIR/places.bin and drives the fastest route over OUT_DIR/roads.bin. Both
are written here, on a PC, from the same extract as the tiles, and copied
to the head unit's map directory (map.route-dir, the map's own directory by
default). The roads are the ones scripts/route-gpx.py drives, timed the
same way (its road classes, maxspeed, one-way streets and roundabouts), so
both find the same routes.

roads.bin is the road network as junctions and the roads between them;
places.bin the street names, each with a point on the street and the town
or village it is in, and the places' own names. Both files are little
endian and made of 32-bit words, so the board maps them and reads them in
place; the layout is written out in docs/map.md and in
crates/lehu-map/src/graph.rs and search.rs. Luxembourg makes about 7 MB and
1 MB in a minute. It needs pyosmium (python3 -m venv v && v/bin/pip install
osmium).

Map data (c) OpenStreetMap contributors, ODbL 1.0.
"""

import argparse
import importlib.util
import math
import os
import struct
import sys
import time
import unicodedata
from pathlib import Path

import osmium

# The roads and their speeds are route-gpx.py's (loaded by its path: its
# name is not a module's, and no cache is written beside it).
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location("route_gpx", Path(__file__).with_name("route-gpx.py"))
rg = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rg)

WORLD_Z = 22
WORLD = 256 << WORLD_Z  # world pixels across, at zoom 22
EQUATOR = 40_075_016.686
# A road's speed is taken at most this fast for its time (a stray maxspeed
# tag would weaken the board's search for the fastest route).
TOP_KMH = 150
# The search grid's cells: 2^14 world pixels, about 400 m in Luxembourg.
GRID_SHIFT = 14
# The places a street can be in, by how much they count: a street belongs
# to the place nearest it, distances divided by this.
LOCALITY = {"city": 3.0, "town": 2.0, "village": 1.0}
# The places found by name, and their order among equal matches (lower is
# more important).
PLACE_RANK = {"city": 0, "town": 1, "suburb": 2, "village": 3, "quarter": 4,
              "hamlet": 5, "neighbourhood": 6, "isolated_dwelling": 7, "locality": 7}
# A street's order among equal matches, by its class.
STREET_RANK = {"motorway": 8, "trunk": 8, "primary": 9, "secondary": 10, "tertiary": 11}
STREET_RANK_OTHER = 12
KIND_STREET, KIND_PLACE = 0, 1

ROADS_MAGIC = b"LEHUROAD"
PLACES_MAGIC = b"LEHUFIND"
ROADS_VERSION = 2
PLACES_VERSION = 1
# The classes a route avoiding motorways keeps off.
MOTORWAY = ("motorway", "motorway_link")


def world(lat, lon):
    """World pixels at zoom 22, as crates/lehu-map/src/view.rs has them."""
    s = math.sin(math.radians(lat))
    x = (lon + 180.0) / 360.0 * WORLD
    y = (0.5 - math.log((1.0 + s) / (1.0 - s)) / (4.0 * math.pi)) * WORLD
    return int(x), int(y)


# Letters NFKD leaves alone (or splits oddly), as crates/lehu-map's fold
# has them.
SPECIAL = {"ß": "ss", "æ": "ae", "œ": "oe", "ø": "o", "ł": "l", "đ": "d", "ð": "d",
           "þ": "th", "ı": "i", "ħ": "h", "ŧ": "t", "ĸ": "k", "ŋ": "n", "ŀ": "l", "ŉ": "n"}


def fold(text):
    """A name as it is matched: lower case, without accents, words split by
    single spaces ("Rue d'Anvers" -> "rue d anvers")."""
    out = []
    for c in text.lower():
        if c in SPECIAL:
            out.append(SPECIAL[c])
            continue
        for d in unicodedata.normalize("NFKD", c):
            if not unicodedata.combining(d):
                out.append(d if d.isalnum() else " ")
    return " ".join("".join(out).split())


def morton(x, y):
    """The Z-order of a point, so nodes near on the map are near in the file."""
    def spread(v):
        v &= 0xFFFFFFFF
        v = (v | (v << 16)) & 0x0000FFFF0000FFFF
        v = (v | (v << 8)) & 0x00FF00FF00FF00FF
        v = (v | (v << 4)) & 0x0F0F0F0F0F0F0F0F
        v = (v | (v << 2)) & 0x3333333333333333
        v = (v | (v << 1)) & 0x5555555555555555
        return v
    return spread(x) | (spread(y) << 1)


class Extract(osmium.SimpleHandler):
    """The roads cars take, with their nodes, and the named places."""

    def __init__(self):
        super().__init__()
        self.ways = []  # (node ids, forward, backward, km/h, name, ring, limit, class)
        self.where = {}  # node id -> (lat, lon)
        self.uses = {}  # node id -> how many times a road passes it
        self.ends = set()  # the nodes a road starts or ends at
        self.places = []  # (name, kind, lat, lon)

    def node(self, n):
        kind = n.tags.get("place")
        name = n.tags.get("name")
        if kind in PLACE_RANK and name and n.location.valid():
            self.places.append((name, kind, n.location.lat, n.location.lon))

    def way(self, w):
        r = rg.road(w.tags)
        if r is None:
            return
        nodes = []
        for n in w.nodes:
            if not n.location.valid():
                return
            nodes.append(n.ref)
            self.where[n.ref] = (n.lat, n.lon)
        if len(nodes) < 2:
            return
        forward, backward, kmh, name, ring, lim = r
        for n in nodes:
            self.uses[n] = self.uses.get(n, 0) + 1
        self.ends.add(nodes[0])
        self.ends.add(nodes[-1])
        self.ways.append((nodes, forward, backward, kmh, name, ring, lim, w.tags.get("highway")))


def split(ex):
    """The ways cut at their junctions: edges (a, b, the node ids between,
    metres, seconds, forward, backward, way index)."""
    junction = {n for n, k in ex.uses.items() if k > 1} | ex.ends
    edges = []
    for wi, (nodes, fwd, bwd, kmh, _, _, _, _) in enumerate(ex.ways):
        speed = min(kmh, TOP_KMH) / 3.6
        start = 0
        run = 0.0
        for i in range(1, len(nodes)):
            run += rg.metres(ex.where[nodes[i - 1]], ex.where[nodes[i]])
            if nodes[i] in junction:
                edges.append((nodes[start], nodes[i], nodes[start + 1:i], run, run / speed, fwd, bwd, wi))
                start, run = i, 0.0
    return edges


def strongest(nodes, edges):
    """The nodes of the largest part of the network where every node can be
    reached from every other (Tarjan's algorithm, without recursion)."""
    out = {n: [] for n in nodes}
    for a, b, _, _, _, fwd, bwd, _ in edges:
        if fwd:
            out[a].append(b)
        if bwd:
            out[b].append(a)
    index, low, on, stack = {}, {}, set(), []
    best = []
    counter = 0
    for root in nodes:
        if root in index:
            continue
        work = [(root, 0)]
        while work:
            v, i = work.pop()
            if i == 0:
                index[v] = low[v] = counter
                counter += 1
                stack.append(v)
                on.add(v)
            recurse = False
            nexts = out[v]
            while i < len(nexts):
                w = nexts[i]
                i += 1
                if w not in index:
                    work.append((v, i))
                    work.append((w, 0))
                    recurse = True
                    break
                if w in on:
                    low[v] = min(low[v], index[w])
            if recurse:
                continue
            if low[v] == index[v]:
                part = []
                while True:
                    w = stack.pop()
                    on.discard(w)
                    part.append(w)
                    if w == v:
                        break
                if len(part) > len(best):
                    best = part
            if work:
                u = work[-1][0]
                low[u] = min(low[u], low[v])
    return set(best)


class Words:
    """A blob of short strings, each a length byte then UTF-8, by offset."""

    def __init__(self):
        self.blob = bytearray()
        self.seen = {}

    def add(self, text):
        data = text.encode()[:255]
        if data not in self.seen:
            self.seen[data] = len(self.blob)
            self.blob.append(len(data))
            self.blob += data
        return self.seen[data]

    def padded(self):
        return bytes(self.blob) + b"\0" * (-len(self.blob) % 4)


def u32s(values):
    return struct.pack(f"<{len(values)}I", *values)


def write_roads(path, ex, edges, keep):
    """roads.bin: see crates/lehu-map/src/graph.rs for the layout."""
    edges = [e for e in edges if e[0] in keep and e[1] in keep]
    world_of = {n: world(*ex.where[n]) for n in keep}
    order = sorted(keep, key=lambda n: morton(*world_of[n]))
    number = {n: i for i, n in enumerate(order)}
    # Edges in the order of their first node, so a node's roads are near.
    edges.sort(key=lambda e: (number[e[0]], number[e[1]]))

    # The roads' kinds: name, roundabout, limit, shared by every edge alike.
    names = Words()
    kinds, kind_of = [], {}
    infos = []
    for a, b, between, m, s, fwd, bwd, wi in edges:
        _, _, _, _, name, ring, lim, cls = ex.ways[wi]
        key = (name, ring, lim or 0)
        if key not in kind_of:
            kind_of[key] = len(kinds)
            kinds.append(key)
        infos.append(kind_of[key] | (fwd << 24) | (bwd << 25) | ((cls in MOTORWAY) << 26))

    # Each node's roads out: the arcs a car may take from it, as (the node
    # it leads to, milliseconds, edge * 2 + 1 when it runs the edge from b
    # to a); and how many roads meet there, whichever way they go.
    arcs = [[] for _ in order]
    degree = [0] * len(order)
    ms = []
    for e, (a, b, between, m, s, fwd, bwd, wi) in enumerate(edges):
        t = max(1, round(s * 1000))
        ms.append(t)
        ia, ib = number[a], number[b]
        if fwd:
            arcs[ia].append((ib, t, e * 2))
        if bwd:
            arcs[ib].append((ia, t, e * 2 + 1))
        degree[ia] += 1
        degree[ib] += 1
    node_words = []
    first = 0
    for i, n in enumerate(order):
        x, y = world_of[n]
        node_words += [x, y, first, degree[i]]
        first += len(arcs[i])
    node_words += [0, 0, first, 0]  # the end of the last node's arcs
    arc_words = [w for out in arcs for arc in out for w in arc]

    # The edges' shapes: the points between a and b, in order.
    shape_x, shape_y = [], []
    edge_words = []
    for e, (a, b, between, m, s, fwd, bwd, wi) in enumerate(edges):
        for n in between:
            x, y = world(*ex.where[n])
            shape_x.append(x)
            shape_y.append(y)
        edge_words += [number[a], number[b], max(1, round(m * 10)), infos[e], len(shape_x) - len(between)]
    edge_words += [0, 0, 0, 0, len(shape_x)]

    kind_words = []
    for name, ring, lim in kinds:
        kind_words += [names.add(name), lim | (ring << 16)]

    # The grid: for each cell, the edges with a piece in it.
    xs = [world_of[n][0] for n in order] + shape_x
    ys = [world_of[n][1] for n in order] + shape_y
    x0 = (min(xs) >> GRID_SHIFT) << GRID_SHIFT
    y0 = (min(ys) >> GRID_SHIFT) << GRID_SHIFT
    cols = ((max(xs) - x0) >> GRID_SHIFT) + 1
    rows = ((max(ys) - y0) >> GRID_SHIFT) + 1
    cells = [[] for _ in range(cols * rows)]
    for e, (a, b, between, *_rest) in enumerate(edges):
        pts = [world_of[a]] + [world(*ex.where[n]) for n in between] + [world_of[b]]
        mine = set()
        for (px, py), (qx, qy) in zip(pts, pts[1:]):
            for cy in range((min(py, qy) - y0) >> GRID_SHIFT, ((max(py, qy) - y0) >> GRID_SHIFT) + 1):
                for cx in range((min(px, qx) - x0) >> GRID_SHIFT, ((max(px, qx) - x0) >> GRID_SHIFT) + 1):
                    mine.add(cy * cols + cx)
        for c in mine:
            cells[c].append(e)
    cell_first, cell_edges = [], []
    for c in cells:
        cell_first.append(len(cell_edges))
        cell_edges += c
    cell_first.append(len(cell_edges))

    # The search's lower bound on the time from anywhere to the goal: the
    # straight line at the top speed, a world pixel counted as the fewest
    # metres it is anywhere on the network (at its northernmost point).
    north = min(ys)
    lat = math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * north / WORLD))))
    metres_per_px = EQUATOR / WORLD * math.cos(math.radians(max(abs(lat), 0.0)))
    south = max(ys)
    lat_s = math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * south / WORLD))))
    if abs(lat_s) > abs(lat):
        metres_per_px = EQUATOR / WORLD * math.cos(math.radians(abs(lat_s)))
    top = max((min(ex.ways[e[7]][3], TOP_KMH) for e in edges), default=TOP_KMH)
    # The fastest road that is not a motorway: the bound when motorways
    # are avoided.
    top_other = max((min(ex.ways[e[7]][3], TOP_KMH) for e in edges
                     if ex.ways[e[7]][7] not in MOTORWAY), default=top)
    h_q16 = int(metres_per_px / (top / 3.6) * 1000 * 65536 * 0.99)

    sections = [
        u32s(node_words), u32s(arc_words), u32s(edge_words), u32s(shape_x), u32s(shape_y),
        u32s(kind_words), u32s(cell_first), u32s(cell_edges), names.padded(),
    ]
    header = ROADS_MAGIC + u32s([
        ROADS_VERSION, len(order), len(arc_words) // 3, len(edges), len(shape_x), len(kinds),
        len(cell_edges), len(names.blob), x0, y0, GRID_SHIFT, cols, rows, h_q16, round(top),
        0, round(top_other), 0,
    ])
    assert len(header) == 80
    with open(path + ".tmp", "wb") as f:
        f.write(header)
        for s in sections:
            f.write(s)
    os.replace(path + ".tmp", path)
    return len(order), len(edges), len(shape_x), len(kinds), cols * rows, h_q16, top


def write_places(path, ex, keep_nodes):
    """places.bin: see crates/lehu-map/src/search.rs for the layout."""
    text = Words()
    towns = [(name, kind, world(lat, lon)) for name, kind, lat, lon in ex.places if kind in LOCALITY]
    if not towns:
        sys.exit("no city, town or village in the extract")
    # A coarse grid of the towns, so each street finds its own quickly.
    tshift = 17  # about 3 km
    tgrid = {}
    for i, (_, _, (x, y)) in enumerate(towns):
        tgrid.setdefault((x >> tshift, y >> tshift), []).append(i)

    def locality(x, y, skip=None):
        best, score = 0, float("inf")
        cx, cy = x >> tshift, y >> tshift
        for r in range(0, 64):
            for gx in range(cx - r, cx + r + 1):
                for gy in range(cy - r, cy + r + 1):
                    if max(abs(gx - cx), abs(gy - cy)) != r:
                        continue
                    for i in tgrid.get((gx, gy), ()):
                        if i == skip:
                            continue
                        name, kind, (tx, ty) = towns[i]
                        d = math.hypot(tx - x, ty - y) / LOCALITY[kind]
                        if d < score:
                            best, score = i, d
            # A town further out than this ring cannot be nearer, even a
            # city (three times the weight).
            if score < float("inf") and score * 3 < (r << tshift):
                break
        return best

    # The streets: their ways grouped by name and the place they are in,
    # each group at the middle of its longest way, on the road.
    streets = {}
    for nodes, _, _, _, name, ring, _, cls in ex.ways:
        if not name or ring or cls.endswith("_link") or cls == "service":
            continue
        inside = [n for n in nodes if n in keep_nodes]
        if not inside:
            continue
        mid = inside[len(inside) // 2]
        x, y = world(*ex.where[mid])
        town = locality(x, y)
        length = sum(rg.metres(ex.where[p], ex.where[q]) for p, q in zip(nodes, nodes[1:]))
        key = (name, town)
        rank = STREET_RANK.get(cls, STREET_RANK_OTHER)
        old = streets.get(key)
        if old is None:
            streets[key] = (length, (x, y), rank)
        else:
            longest = (length, (x, y)) if length > old[0] else old[:2]
            streets[key] = (*longest, min(rank, old[2]))
    entries = []
    for (name, town), (_, (x, y), rank) in streets.items():
        entries.append((x, y, name, town, KIND_STREET, rank))
    # The places themselves: a city or a town is its own, anything smaller
    # is in the place nearest it.
    for name, kind, lat, lon in ex.places:
        x, y = world(lat, lon)
        own = next((i for i, t in enumerate(towns) if t[0] == name and t[2] == (x, y)), None)
        town = own if kind in ("city", "town") and own is not None else locality(x, y, skip=own)
        entries.append((x, y, name, town, KIND_PLACE, PLACE_RANK[kind]))
    entries.sort(key=lambda e: (fold(e[2]), e[5], e[3]))

    town_words = []
    for name, _, _ in towns:
        town_words += [text.add(name), text.add(fold(name))]
    entry_words = []
    for x, y, name, town, kind, rank in entries:
        entry_words += [x, y, text.add(name), text.add(fold(name)), town | (kind << 16) | (rank << 24)]
    header = PLACES_MAGIC + u32s([PLACES_VERSION, len(entries), len(towns), len(text.blob)])
    assert len(header) == 24
    with open(path + ".tmp", "wb") as f:
        f.write(header)
        f.write(u32s(town_words))
        f.write(u32s(entry_words))
        f.write(text.padded())
    os.replace(path + ".tmp", path)
    streets_n = sum(1 for e in entries if e[4] == KIND_STREET)
    return streets_n, len(entries) - streets_n, len(towns)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("extract")
    ap.add_argument("out")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)

    t0 = time.time()
    ex = Extract()
    ex.apply_file(a.extract, locations=True, idx="flex_mem")
    print(f"{len(ex.ways)} roads, {len(ex.where)} road nodes, {len(ex.places)} places "
          f"in {time.time() - t0:.0f} s", flush=True)
    edges = split(ex)
    nodes = {e[0] for e in edges} | {e[1] for e in edges}
    keep = strongest(nodes, edges)
    print(f"{len(nodes)} junctions, {len(keep)} of them connected both ways", flush=True)
    roads = os.path.join(a.out, "roads.bin")
    n, e, s, k, c, h, top = write_roads(roads, ex, edges, keep)
    print(f"{roads}: {n} junctions, {e} roads, {s} shape points, {k} road names, "
          f"{c} grid cells, top {top:.0f} km/h; {os.path.getsize(roads) / 1e6:.1f} MB", flush=True)
    # A street is found on the part of the network a route can reach.
    on_network = set(keep)
    for a_, b_, between, *_r in edges:
        if a_ in keep and b_ in keep:
            on_network.update(between)
    places = os.path.join(a.out, "places.bin")
    st, pl, towns = write_places(places, ex, on_network)
    print(f"{places}: {st} streets, {pl} places, {towns} towns; "
          f"{os.path.getsize(places) / 1e6:.1f} MB in {time.time() - t0:.0f} s", flush=True)


if __name__ == "__main__":
    main()
