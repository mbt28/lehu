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

roads.bin is the road network as junctions and the roads between them,
the motorways marked (so routes can avoid them), and the main roads again
as a network of their own (the layer, which long routes are found on);
cameras.bin the speed cameras on it, tied to that roads.bin by its build
ID (a hash of its contents), so they can be replaced alone; places.bin the
street names, each with a point on the street and the town or village it
is in, the places' own names, the house numbers on each street (with their
postcodes), and the filling stations, public car parks and charging
stations (found nearest, not by name). The files are little
endian and made of 32-bit words, so the board maps them and reads them in
place; the layout is written out in docs/map.md and in
crates/lehu-map/src/graph.rs, cameras.rs and search.rs. Luxembourg makes
about 7 MB and 0.5 MB in half a minute, Hungary 69 MB and 19 MB in about
five minutes and 3 GB of memory. It needs pyosmium (python3 -m venv v &&
v/bin/pip install osmium).

Map data (c) OpenStreetMap contributors, ODbL 1.0.
"""

import argparse
import hashlib
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
# A house further than this from the street of its name in its town (or,
# with none there, from the nearest street of its name) is left out,
# metres.
HOUSE_STREET_M = 3000

ROADS_MAGIC = b"LEHUROAD"
PLACES_MAGIC = b"LEHUFIND"
CAMERAS_MAGIC = b"LEHUCAMS"
ROADS_VERSION = 3
PLACES_VERSION = 3
CAMERAS_VERSION = 1
# The classes a route avoiding motorways keeps off.
MOTORWAY = ("motorway", "motorway_link")
# The main roads: far from both ends of a route, the board searches only
# these, over a smaller network of their own (the layer: the junctions
# where main roads meet, the roads between them joined into one).
MAIN = ("motorway", "motorway_link", "trunk", "trunk_link", "primary", "primary_link",
        "secondary", "secondary_link")
# A node's word of how many roads meet there holds this many bits; above
# them, its place in the layer (+ 1, 0 when it is not in it).
DEGREE_BITS = 8
# An arc's edge word: edge * 2 + 1 when it runs b to a, + this when the
# edge is a main road.
ARC_MAIN = 1 << 31
# A speed camera further than this from any road is left out, metres.
CAMERA_SNAP_M = 50
# The points of interest found nearest (Map > Go to): their categories in
# places.bin, by amenity.
POI_CATEGORY = {"fuel": 1, "parking": 2, "charging_station": 3}
POI_UNNAMED = {1: "Fuel station", 2: "Parking", 3: "Charging station"}
# Access that keeps a point of interest out: not for anyone driving by.
POI_CLOSED = {"private", "no", "permit", "residents", "employees", "delivery", "agricultural",
              "forestry"}
# A car park counts when anyone may use it (not only a shop's customers),
# it is not just the side of a street, and it has a name or room for at
# least PARKING_MIN cars.
PARKING_CLOSED = POI_CLOSED | {"customers"}
PARKING_MIN = 10
PARKING_STREET = {"street_side", "lane", "layby", "on_kerb", "half_on_kerb", "shoulder"}
PARKING_KIND = {"underground": "Underground", "multi-storey": "Multi-storey",
                "rooftop": "Rooftop"}
# Two of a category this close with the same name (or one with none) are
# one, mapped twice (a node and an area).
POI_SAME_M = 60


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
        self.cameras = []  # (node id, lat, lon, direction tag, speed limit or None)
        self.pois = []  # (category, name, detail, lat, lon, named)
        self.enforcements = []  # (device node id, from node id, to node id, limit)
        self.houses = []  # (street, house number, postcode, lat, lon)

    def house(self, tags, lat, lon):
        number, street = tags.get("addr:housenumber"), tags.get("addr:street")
        if number and street:
            self.houses.append((street, number, tags.get("addr:postcode", ""), lat, lon))

    def node(self, n):
        if "addr:housenumber" in n.tags and n.location.valid():
            self.house(n.tags, n.location.lat, n.location.lon)
        if n.tags.get("amenity") in POI_CATEGORY and n.location.valid():
            self.poi(n.tags, n.location.lat, n.location.lon)
        if n.tags.get("highway") == "speed_camera" and n.location.valid():
            self.cameras.append((n.id, n.location.lat, n.location.lon,
                                 n.tags.get("direction", ""), rg.limit(n.tags)))
        kind = n.tags.get("place")
        name = n.tags.get("name")
        if kind in PLACE_RANK and name and n.location.valid():
            self.places.append((name, kind, n.location.lat, n.location.lon))

    def poi(self, tags, lat, lon):
        p = poi(tags)
        if p is not None:
            self.pois.append((*p[:3], lat, lon, p[3]))

    def way(self, w):
        poi = w.tags.get("amenity") in POI_CATEGORY
        if poi or "addr:housenumber" in w.tags:
            # An area: at the middle of its outline.
            pts = [(n.lat, n.lon) for n in w.nodes if n.location.valid()]
            if pts and len(pts) == len(w.nodes):
                if len(pts) > 1 and pts[0] == pts[-1]:
                    pts = pts[:-1]
                mid = (sum(p[0] for p in pts) / len(pts), sum(p[1] for p in pts) / len(pts))
                if poi:
                    self.poi(w.tags, *mid)
                self.house(w.tags, *mid)
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

    def relation(self, r):
        # A speed camera's enforcement: the device, and the traffic it
        # watches, from one node to another.
        if r.tags.get("type") != "enforcement" or r.tags.get("enforcement") not in (
                "maxspeed", "average_speed"):
            return
        role = {}
        for m in r.members:
            if m.type == "n" and m.role in ("device", "from", "to"):
                role.setdefault(m.role, m.ref)
        if "device" in role:
            self.enforcements.append((role["device"], role.get("from"), role.get("to"),
                                      rg.limit(r.tags)))


def number(text):
    """The leading number of a tag's value, or None: "22 kW" is 22."""
    digits = ""
    for c in text.strip():
        if c.isdigit() or (c == "." and digits and "." not in digits):
            digits += c
        else:
            break
    try:
        return float(digits)
    except ValueError:
        return None


def poi(tags):
    """A point of interest as (category, name, detail, named), or None for
    one that is not for anyone driving by."""
    category = POI_CATEGORY[tags.get("amenity")]
    access = tags.get("access", "")
    name = tags.get("name", "").strip()
    brand = tags.get("brand", "").strip()
    operator = tags.get("operator", "").strip()
    named = bool(name)
    capacity = number(tags.get("capacity", "")) or 0
    if category == 1:
        if access in POI_CLOSED:
            return None
        name = name or brand or operator
        detail = next((b for b in (brand, operator) if b and b != name), "")
    elif category == 2:
        if access in PARKING_CLOSED or tags.get("parking") in PARKING_STREET:
            return None
        if not name and capacity < PARKING_MIN:
            return None
        parts = []
        if tags.get("parking") in PARKING_KIND:
            parts.append(PARKING_KIND[tags["parking"]])
        if tags.get("park_ride") not in (None, "no"):
            parts.append("Park and ride")
        if capacity > 0:
            parts.append(f"{capacity:.0f} spaces")
        detail = ", ".join(parts)
    else:
        if access in POI_CLOSED or tags.get("motorcar") == "no":
            return None
        name = name or operator or brand
        parts = []
        if capacity > 0:
            parts.append(f"{capacity:.0f} point{'s' if capacity != 1 else ''}")
        kw = 0.0
        for tag in tags:
            k, v = tag.k, tag.v
            if k.startswith("socket:") and k.endswith(":output"):
                for part in v.replace(",", ";").split(";"):
                    n = number(part)
                    if n is not None and "kw" in part.lower().replace(" ", ""):
                        kw = max(kw, n)
        if kw > 0:
            parts.append(f"{kw:.0f} kW")
        detail = ", ".join(parts)
    name = name or POI_UNNAMED[category]
    return category, name[:120], detail[:120], named


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
    main = []
    for e, (a, b, between, m, s, fwd, bwd, wi) in enumerate(edges):
        t = max(1, round(s * 1000))
        ms.append(t)
        is_main = ex.ways[wi][7] in MAIN
        main.append(is_main)
        flag = ARC_MAIN if is_main else 0
        ia, ib = number[a], number[b]
        if fwd:
            arcs[ia].append((ib, t, e * 2 | flag))
        if bwd:
            arcs[ib].append((ia, t, e * 2 + 1 | flag))
        degree[ia] += 1
        degree[ib] += 1
    assert len(edges) < 1 << 30

    ends = [(number[e[0]], number[e[1]], e[5], e[6]) for e in edges]
    dms = [max(1, round(e[3] * 10)) for e in edges]
    motorway = [ex.ways[e[7]][7] in MOTORWAY for e in edges]
    layer, layer_node_words, layer_arc_words, unpack_words = main_layer(
        len(order), ends, main, ms, dms, motorway, [world_of[n] for n in order])

    node_words = []
    first = 0
    for i, n in enumerate(order):
        x, y = world_of[n]
        node_words += [x, y, first, min(degree[i], (1 << DEGREE_BITS) - 1)
                       | (layer.get(i, -1) + 1) << DEGREE_BITS]
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

    camera_words = place_cameras(ex, edges, world_of, cells, (x0, y0, cols, rows))

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
        u32s(layer_node_words), u32s(layer_arc_words), u32s(unpack_words),
    ]
    # The build's ID, which cameras.bin names: a hash of what follows the
    # header, so the same network always has the same one.
    digest = hashlib.blake2b(digest_size=8)
    for s in sections:
        digest.update(s)
    build_id = int.from_bytes(digest.digest(), "little")
    layer_nodes = len(layer_node_words) // LAYER_NODE - 1
    layer_arcs = len(layer_arc_words) // LAYER_ARC - 1
    header = ROADS_MAGIC + u32s([
        ROADS_VERSION, len(order), len(arc_words) // 3, len(edges), len(shape_x), len(kinds),
        len(cell_edges), len(names.blob), x0, y0, GRID_SHIFT, cols, rows, h_q16, round(top),
        0, round(top_other), 0, build_id & 0xFFFFFFFF, build_id >> 32, layer_nodes, layer_arcs,
        len(unpack_words), 0,
    ])
    assert len(header) == 4 * ROADS_HEADER
    with open(path + ".tmp", "wb") as f:
        f.write(header)
        for s in sections:
            f.write(s)
    os.replace(path + ".tmp", path)

    cameras = os.path.join(os.path.dirname(path), "cameras.bin")
    with open(cameras + ".tmp", "wb") as f:
        f.write(CAMERAS_MAGIC + u32s([CAMERAS_VERSION, len(camera_words), build_id & 0xFFFFFFFF,
                                      build_id >> 32]))
        f.write(u32s([w for c in camera_words for w in c]))
    os.replace(cameras + ".tmp", cameras)
    return {"junctions": len(order), "roads": len(edges), "shape points": len(shape_x),
            "road names": len(kinds), "grid cells": cols * rows, "speed cameras": len(camera_words),
            "top km/h": top, "layer junctions": layer_nodes, "layer roads": layer_arcs,
            "layer steps": len(unpack_words), "build": f"{build_id:016x}"}


# The layer's records, in words: a junction (its node, x, y, its first
# road), a road (the junction it leads to, milliseconds, decimetres, the
# milliseconds and decimetres of it that are motorway, its first step).
LAYER_NODE = 4
LAYER_ARC = 6
ROADS_HEADER = 26


def main_layer(nodes, ends, main, ms, dms, motorway, xy):
    """The main roads as a network of their own: its junctions are the
    nodes where main roads meet other than two in a row (or where the way
    they may be driven changes), and each of its roads the main roads from
    one junction to the next, with the edges they are made of (its steps,
    edge * 2 + 1 when it runs b to a). Returns the junctions' places in it
    by node, and its three sections' words."""
    touching = {}
    for e, on in enumerate(main):
        if on:
            a, b = ends[e][0], ends[e][1]
            touching.setdefault(a, []).append(e)
            touching.setdefault(b, []).append(e)

    def leaves(e, n):
        a, b, fwd, bwd = ends[e]
        return (fwd and a == n) or (bwd and b == n)

    def enters(e, n):
        a, b, fwd, bwd = ends[e]
        return (fwd and b == n) or (bwd and a == n)

    def other(e, n):
        a, b = ends[e][0], ends[e][1]
        return b if a == n else a

    def interior(n):
        es = touching[n]
        if len(es) != 2 or es[0] == es[1]:
            return False
        e1, e2 = es
        # In through e1 and out through e2 as the chain runs, and against.
        return (enters(e1, n), leaves(e1, n)) == (leaves(e2, n), enters(e2, n))

    junction = {n for n in touching if not interior(n)}
    seen = [False] * len(main)
    roads = []  # (from node, to node, steps)

    def walk(j, e):
        steps, n, along, against = [], j, True, True
        while True:
            seen[e] = True
            back = ends[e][1] == n and ends[e][0] != n
            along &= leaves(e, n)
            nxt = other(e, n)
            against &= enters(e, n)
            steps.append(e * 2 + back)
            n = nxt
            if n in junction:
                break
            es = touching[n]
            e = es[1] if es[0] == e else es[0]
        if n == j:
            return  # a loop back to where it began: no route needs it
        if along:
            roads.append((j, n, steps))
        if against:
            roads.append((n, j, [s ^ 1 for s in reversed(steps)]))

    for j in sorted(junction):
        for e in touching[j]:
            if not seen[e]:
                walk(j, e)
    # Rings of main roads with no junction on them: one made at a node.
    for e, on in enumerate(main):
        if on and not seen[e]:
            j = ends[e][0]
            junction.add(j)
            walk(j, e)

    order = sorted(junction)
    place = {n: i for i, n in enumerate(order)}
    out = [[] for _ in order]
    for f, t, steps in roads:
        out[place[f]].append((place[t], steps))
    node_words, arc_words, unpack = [], [], []
    first = 0
    for i, n in enumerate(order):
        node_words += [n, xy[n][0], xy[n][1], first]
        first += len(out[i])
    node_words += [0, 0, 0, first]
    for i in range(len(order)):
        for t, steps in out[i]:
            edges_ = [s >> 1 for s in steps]
            arc_words += [t, sum(ms[e] for e in edges_), sum(dms[e] for e in edges_),
                          sum(ms[e] for e in edges_ if motorway[e]),
                          sum(dms[e] for e in edges_ if motorway[e]), len(unpack)]
            unpack += steps
    arc_words += [0, 0, 0, 0, 0, len(unpack)]
    return place, node_words, arc_words, unpack


def place_cameras(ex, edges, world_of, cells, grid):
    """The speed cameras on the roads kept, each at the nearest point of
    the nearest road within CAMERA_SNAP_M: (x, y, edge, decimetres from
    its a, the ways it watches (1 a to b, 2 b to a), speed limit or 0), in
    the order of their edges and along each. The ways come from the
    camera's enforcement relations (from one node to another), else its
    direction=forward/backward when it is a node of that road, else both."""
    x0, y0, cols, rows = grid
    watched = {}
    limits = {}
    for device, frm, to, lim in ex.enforcements:
        if frm in ex.where and to in ex.where:
            watched.setdefault(device, []).append((world(*ex.where[frm]), world(*ex.where[to])))
        if lim:
            limits.setdefault(device, lim)
    out = []
    lost = 0
    for node, lat, lon, direction, lim in ex.cameras:
        cx, cy = world(lat, lon)
        m_per_px = EQUATOR / WORLD * math.cos(math.radians(lat))
        best = None
        gx, gy = (cx - x0) >> GRID_SHIFT, (cy - y0) >> GRID_SHIFT
        seen = set()
        for yy in range(gy - 1, gy + 2):
            for xx in range(gx - 1, gx + 2):
                if not (0 <= xx < cols and 0 <= yy < rows):
                    continue
                for e in cells[yy * cols + xx]:
                    if e in seen:
                        continue
                    seen.add(e)
                    a, b, between = edges[e][0], edges[e][1], edges[e][2]
                    pts = [world_of[a]] + [world(*ex.where[n]) for n in between] + [world_of[b]]
                    for k, ((px, py), (qx, qy)) in enumerate(zip(pts, pts[1:])):
                        dx, dy = qx - px, qy - py
                        l2 = dx * dx + dy * dy
                        t = 0.0 if l2 == 0 else max(0.0, min(1.0, ((cx - px) * dx + (cy - py) * dy) / l2))
                        sx, sy = px + dx * t, py + dy * t
                        d2 = (sx - cx) ** 2 + (sy - cy) ** 2
                        if best is None or d2 < best[0]:
                            best = (d2, e, k, t, pts)
        if best is None or math.sqrt(best[0]) * m_per_px > CAMERA_SNAP_M:
            lost += 1
            continue
        _, e, k, t, pts = best
        a, b, between, metres_, _, _, _, _ = edges[e]
        seg = [math.hypot(q[0] - p[0], q[1] - p[1]) for p, q in zip(pts, pts[1:])]
        total = sum(seg)
        before = sum(seg[:k]) + seg[k] * t
        dm = max(1, round(metres_ * 10))
        from_a = min(dm, round(before / total * dm)) if total > 0 else 0
        (px, py), (qx, qy) = pts[k], pts[k + 1]
        sx, sy = int(px + (qx - px) * t), int(py + (qy - py) * t)
        ways = 0
        for (fx, fy), (tx, ty) in watched.get(node, ()):
            ways |= 1 if (tx - fx) * (qx - px) + (ty - fy) * (qy - py) > 0 else 2
        # The road the camera is a node of: its own way, so forward is a to b.
        on_road = node in between
        if not ways and on_road and direction == "forward":
            ways = 1
        elif not ways and on_road and direction == "backward":
            ways = 2
        if not ways:
            ways = 3
        out.append([sx, sy, e, from_a, ways, lim or limits.get(node) or 0])
    out.sort(key=lambda c: (c[2], c[3]))
    if lost:
        print(f"{lost} speed cameras further than {CAMERA_SNAP_M} m from a road kept, left out",
              flush=True)
    return out


def same_pois(pois):
    """The points of interest without the ones mapped twice: of two of a
    category within POI_SAME_M with the same name (or one unnamed), the
    named one is kept."""
    kept = []
    grid = {}
    cell = 1 << 12  # world pixels, about 100 m
    for p in sorted(pois, key=lambda p: not p[5]):
        category, name, _, lat, lon, named = p
        x, y = world(lat, lon)
        m_per_px = EQUATOR / WORLD * math.cos(math.radians(lat))
        twin = False
        for gx in range(x // cell - 1, x // cell + 2):
            for gy in range(y // cell - 1, y // cell + 2):
                for q in grid.get((gx, gy), ()):
                    qx, qy = world(q[3], q[4])
                    if (q[0] == category and (q[1] == name or not named)
                            and math.hypot(qx - x, qy - y) * m_per_px < POI_SAME_M):
                        twin = True
        if not twin:
            kept.append(p)
            grid.setdefault((x // cell, y // cell), []).append(p)
    return kept


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
    # The words of the names, sorted, each with its name: a word typed is
    # looked up, not every name read.
    words = sorted({(w.encode(), i) for i, e in enumerate(entries) for w in fold(e[2]).split()})
    word_words = []
    for w, i in words:
        word_words += [text.add(w.decode()), i]
    # The names of each town, together.
    by_town = sorted(range(len(entries)), key=lambda i: (entries[i][3], i))
    town_first, at = [], 0
    for t in range(len(towns)):
        town_first.append(at)
        while at < len(by_town) and entries[by_town[at]][3] == t:
            at += 1
    town_first.append(at)
    # The houses of each street, at the street of their name in their town.
    houses, lost = place_houses(ex.houses, entries, locality)
    house_first, house_words, at = [], [], 0
    for i in range(len(entries)):
        house_first.append(at)
        while at < len(houses) and houses[at][0] == i:
            _, _, x, y, number, postcode = houses[at]
            house_words += [x, y, text.add(number), text.add(postcode)]
            at += 1
    house_first.append(at)
    if lost:
        print(f"{lost} house numbers away from any street of their name left out", flush=True)
    # The points of interest, by category, near ones near in the file.
    pois = []
    for category, name, detail, lat, lon, _ in same_pois(ex.pois):
        x, y = world(lat, lon)
        pois.append((category, morton(x, y), x, y, name, detail, locality(x, y)))
    pois.sort()
    poi_words = []
    for category, _, x, y, name, detail, town in pois:
        poi_words += [x, y, text.add(name), text.add(detail), town | (category << 16)]
    header = PLACES_MAGIC + u32s([PLACES_VERSION, len(entries), len(towns), len(text.blob),
                                  len(pois), len(houses), len(words)])
    assert len(header) == 36
    with open(path + ".tmp", "wb") as f:
        f.write(header)
        f.write(u32s(town_words))
        f.write(u32s(entry_words))
        f.write(u32s(poi_words))
        f.write(u32s(word_words))
        f.write(u32s(town_first))
        f.write(u32s(by_town))
        f.write(u32s(house_first))
        f.write(u32s(house_words))
        f.write(text.padded())
    os.replace(path + ".tmp", path)
    streets_n = sum(1 for e in entries if e[4] == KIND_STREET)
    counts = [sum(1 for p in pois if p[0] == c) for c in (1, 2, 3)]
    return streets_n, len(entries) - streets_n, len(towns), counts, len(houses)


def place_houses(found, entries, locality):
    """The house numbers by the name they are on: each at the street of its
    name in the town it is in, or, with none there, at the nearest street
    of its name within HOUSE_STREET_M. Returns (entry, the number's order,
    x, y, number, postcode) sorted, and how many were left out."""
    streets = {}
    for i, (x, y, name, town, kind, _) in enumerate(entries):
        if kind == KIND_STREET:
            streets.setdefault(fold(name), []).append((i, town, x, y))
    out, lost, seen = [], 0, set()
    for street, number, postcode, lat, lon in found:
        x, y = world(lat, lon)
        number, postcode = number.strip(), postcode.strip()
        # A building and a point inside it, both with the address: once.
        key = (street, number, x >> 10, y >> 10)
        if key in seen:
            continue
        seen.add(key)
        here = streets.get(fold(street))
        if not here or not number:
            lost += 1
            continue
        m_per_px = EQUATOR / WORLD * math.cos(math.radians(lat))
        town = locality(x, y)
        same = [s for s in here if s[1] == town]
        best = min(same or here, key=lambda s: math.hypot(s[2] - x, s[3] - y))
        if not same and math.hypot(best[2] - x, best[3] - y) * m_per_px > HOUSE_STREET_M:
            lost += 1
            continue
        lead = len(number) - len(number.lstrip("0123456789"))
        out.append((best[0], (int(number[:min(lead, 6)] or 0), number), x, y, number, postcode))
    # An address on two buildings (or a building and a point beside it):
    # once, the first.
    out.sort(key=lambda h: (h[0], h[1]))
    once = [h for k, h in enumerate(out)
            if k == 0 or (h[0], h[4], h[5]) != (out[k - 1][0], out[k - 1][4], out[k - 1][5])]
    return once, lost


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
    counts = write_roads(roads, ex, edges, keep)
    print(f"{roads}: " + ", ".join(f"{v} {k}" for k, v in counts.items())
          + f"; {os.path.getsize(roads) / 1e6:.1f} MB, cameras.bin "
          f"{os.path.getsize(os.path.join(a.out, 'cameras.bin')) / 1e3:.1f} kB", flush=True)
    # A street is found on the part of the network a route can reach.
    on_network = set(keep)
    for a_, b_, between, *_r in edges:
        if a_ in keep and b_ in keep:
            on_network.update(between)
    places = os.path.join(a.out, "places.bin")
    st, pl, towns, (fuel, parking, charging), houses = write_places(places, ex, on_network)
    print(f"{places}: {st} streets, {pl} places, {towns} towns, {houses} house numbers, {fuel} "
          f"fuel, {parking} parking, {charging} charging; {os.path.getsize(places) / 1e6:.1f} MB "
          f"in {time.time() - t0:.0f} s", flush=True)


if __name__ == "__main__":
    main()
