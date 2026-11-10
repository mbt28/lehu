#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Drive a route on an OpenStreetMap extract, on a PC, as a timed GPX track.

    scripts/route-gpx.py EXTRACT.osm.pbf FROM TO OUT.gpx [--via LAT,LON ...]

FROM, TO and each --via are "latitude,longitude". The route is the fastest
by car over the extract's roads: one-way streets and roundabouts are kept
to, motorways are fast and residential streets slow (a road's maxspeed
where it has one). The track has a point every second of the drive, timed
at those speeds, slowing for sharp turns, so LEHU can replay it as a
drive (map.fake-track) until there is a real position. The turns come as
waypoints before the track, in order: each at its junction, its <type> the
manoeuvre (left, slight-right, u-turn, roundabout-2, arrive...) and its
<name> the road it leads onto. It needs pyosmium
(python3 -m venv v && v/bin/pip install osmium).

Map data (c) OpenStreetMap contributors, ODbL 1.0.
"""

import argparse
import heapq
import math
import os
import sys
import time

import osmium

# Speeds by road class, km/h, where the road has no maxspeed of its own.
SPEED = {
    "motorway": 110, "motorway_link": 60,
    "trunk": 90, "trunk_link": 50,
    "primary": 60, "primary_link": 40,
    "secondary": 50, "secondary_link": 40,
    "tertiary": 50, "tertiary_link": 30,
    "unclassified": 40, "residential": 30, "living_street": 10,
    "road": 30, "service": 15,
}
# A turn sharper than this many degrees is taken at TURN_SPEED.
SHARP_TURN = 60
TURN_SPEED = 20
EARTH = 6371008.8


def metres(a, b):
    """Great-circle distance between two (lat, lon) points."""
    la1, lo1, la2, lo2 = map(math.radians, (a[0], a[1], b[0], b[1]))
    h = math.sin((la2 - la1) / 2) ** 2 + math.cos(la1) * math.cos(la2) * math.sin((lo2 - lo1) / 2) ** 2
    return 2 * EARTH * math.asin(math.sqrt(h))


def bearing(a, b):
    la1, lo1, la2, lo2 = map(math.radians, (a[0], a[1], b[0], b[1]))
    y = math.sin(lo2 - lo1) * math.cos(la2)
    x = math.cos(la1) * math.sin(la2) - math.sin(la1) * math.cos(la2) * math.cos(lo2 - lo1)
    return math.degrees(math.atan2(y, x))


def maxspeed(tags, cls):
    v = tags.get("maxspeed", "")
    try:
        if v.endswith(" mph"):
            return float(v[:-4]) * 1.609
        return float(v)
    except ValueError:
        return SPEED[cls]


class Roads(osmium.SimpleHandler):
    def __init__(self):
        super().__init__()
        self.where = {}  # node id -> (lat, lon)
        self.edges = {}  # node id -> [(next node, seconds, metres, way)]
        self.ways = []  # (name, roundabout) by way index
        self.near = {}  # node id -> the nodes next to it on any road

    def way(self, w):
        tags = w.tags
        cls = tags.get("highway")
        if cls not in SPEED or tags.get("area") == "yes":
            return
        if tags.get("access") in ("no", "private") or tags.get("motor_vehicle") in ("no", "private"):
            return
        oneway = tags.get("oneway", "")
        forward = backward = True
        if oneway in ("yes", "1", "true") or tags.get("junction") in ("roundabout", "circular") \
                or (cls == "motorway" and oneway != "no"):
            backward = False
        elif oneway == "-1":
            forward = False
        kmh = max(5.0, maxspeed(tags, cls))
        way = len(self.ways)
        name = tags.get("name") or tags.get("ref") or ""
        self.ways.append((name, tags.get("junction") in ("roundabout", "circular")))
        nodes = []
        for n in w.nodes:
            if not n.location.valid():
                return
            nodes.append((n.ref, (n.lat, n.lon)))
        for (a, pa), (b, pb) in zip(nodes, nodes[1:]):
            self.where[a] = pa
            self.where[b] = pb
            d = metres(pa, pb)
            s = d / (kmh / 3.6)
            if forward:
                self.edges.setdefault(a, []).append((b, s, d, way))
            if backward:
                self.edges.setdefault(b, []).append((a, s, d, way))
            self.near.setdefault(a, set()).add(b)
            self.near.setdefault(b, set()).add(a)


def nearest(roads, p):
    """The road node nearest to p that has a way out."""
    best, dist = None, float("inf")
    for n, q in roads.where.items():
        if n not in roads.edges:
            continue
        d = (q[0] - p[0]) ** 2 + ((q[1] - p[1]) * math.cos(math.radians(p[0]))) ** 2
        if d < dist:
            best, dist = n, d
    return best


def fastest(roads, a, b):
    """Dijkstra by time from node a to node b: the nodes on the way."""
    done = set()
    came = {}
    cost = {a: 0.0}
    heap = [(0.0, a)]
    while heap:
        t, n = heapq.heappop(heap)
        if n == b:
            path = [b]
            while path[-1] != a:
                path.append(came[path[-1]])
            return path[::-1]
        if n in done:
            continue
        done.add(n)
        for m, s, _, _ in roads.edges.get(n, ()):
            u = t + s
            if u < cost.get(m, float("inf")):
                cost[m] = u
                came[m] = n
                heapq.heappush(heap, (u, m))
    return None


def speeds(roads, path):
    """Each step's speed, m/s: the road's, slowed for a sharp turn at
    either end of the step."""
    pts = [roads.where[n] for n in path]
    heads = [bearing(p, q) for p, q in zip(pts, pts[1:])]

    def sharp(i):
        """The turn from step i - 1 into step i is sharp."""
        if i <= 0 or i >= len(heads):
            return False
        return abs((heads[i] - heads[i - 1] + 180) % 360 - 180) > SHARP_TURN

    out = []
    for i, (a, b) in enumerate(zip(path, path[1:])):
        s = next(s for m, s, _, _ in roads.edges[a] if m == b)
        d = metres(pts[i], pts[i + 1])
        v = d / s if s > 0 else 1.0
        if sharp(i) or sharp(i + 1):
            v = min(v, TURN_SPEED / 3.6)
        out.append(max(v, 1.0))
    return out


def step_way(roads, a, b):
    """The fastest way from node a to the next node b."""
    return min((e for e in roads.edges[a] if e[0] == b), key=lambda e: e[1])[3]


def heading_near(roads, path, i, back):
    """The bearing at node path[i], over about 15 m of the path behind it
    (back) or ahead of it."""
    here = roads.where[path[i]]
    j, run = i, 0.0
    while run < 15:
        k = j - 1 if back else j + 1
        if k < 0 or k >= len(path):
            break
        run += metres(roads.where[path[j]], roads.where[path[k]])
        j = k
    if j == i:
        return None
    there = roads.where[path[j]]
    return bearing(there, here) if back else bearing(here, there)


def kind_of(turn):
    """A turn's manoeuvre, from its angle in degrees (right positive)."""
    a = abs(turn)
    if a < 20:
        return "straight"
    side = "right" if turn > 0 else "left"
    if a < 45:
        return "slight-" + side
    if a < 135:
        return side
    if a < 170:
        return "sharp-" + side
    return "u-turn"


def maneuvers(roads, path):
    """The turns along the path: (node index, kind, road onto), the
    arrival last."""
    ways = [step_way(roads, a, b) for a, b in zip(path, path[1:])]
    out = []
    i = 1
    while i < len(path) - 1:
        before, after = roads.ways[ways[i - 1]], roads.ways[ways[i]]
        if after[1] and not before[1]:
            # Into a roundabout: count the ways out of it up to the one we
            # take, and name the road after it.
            j = i
            exits = 0
            while j < len(ways) and roads.ways[ways[j]][1]:
                # path[j] is the next node on the ring, or where we leave it.
                j += 1
                leaving = j >= len(ways) or not roads.ways[ways[j]][1]
                ring = {path[j - 1], path[j + 1] if j + 1 < len(path) else None}
                others = [m for m, _, _, w in roads.edges.get(path[j], ())
                          if not roads.ways[w][1] and m not in ring]
                if leaving or others:
                    exits += 1
            onto = roads.ways[ways[j]][0] if j < len(ways) else ""
            out.append((i, f"roundabout-{max(1, exits)}", onto))
            i = j + 1
            continue
        turn_in = heading_near(roads, path, i, True)
        turn_out = heading_near(roads, path, i, False)
        if turn_in is None or turn_out is None:
            i += 1
            continue
        turn = (turn_out - turn_in + 180) % 360 - 180
        kind = kind_of(turn)
        junction = len(roads.near.get(path[i], ())) > 2
        renamed = after[0] != before[0]
        if (renamed and junction) or (junction and kind != "straight" and abs(turn) >= 45):
            if kind == "straight" and not after[0]:
                i += 1
                continue
            out.append((i, kind, after[0]))
        i += 1
    out.append((len(path) - 1, "arrive", ""))
    return out


def track(roads, path, start):
    """A point each second along the path, (lat, lon, unix seconds)."""
    pts = [roads.where[n] for n in path]
    v = speeds(roads, path)
    out = [(pts[0][0], pts[0][1], start)]
    t = 0.0
    next_t = 1.0
    for (a, b), speed in zip(zip(pts, pts[1:]), v):
        d = metres(a, b)
        dt = d / speed
        while next_t <= t + dt:
            f = (next_t - t) / dt if dt > 0 else 1.0
            out.append((a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, start + next_t))
            next_t += 1.0
        t += dt
    if out[-1][:2] != pts[-1]:
        out.append((pts[-1][0], pts[-1][1], start + t))
    return out, t, sum(metres(a, b) for a, b in zip(pts, pts[1:]))


def point(text):
    lat, lon = (float(v) for v in text.split(","))
    return lat, lon


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("extract")
    ap.add_argument("start", type=point, metavar="FROM")
    ap.add_argument("end", type=point, metavar="TO")
    ap.add_argument("out")
    ap.add_argument("--via", type=point, action="append", default=[])
    ap.add_argument("--name", default="Sample drive")
    a = ap.parse_args()

    t0 = time.time()
    roads = Roads()
    roads.apply_file(a.extract, locations=True, idx="flex_mem")
    print(f"{len(roads.where)} road nodes in {time.time() - t0:.0f} s", flush=True)
    stops = [a.start, *a.via, a.end]
    nodes = [nearest(roads, p) for p in stops]
    path = [nodes[0]]
    for x, y in zip(nodes, nodes[1:]):
        leg = fastest(roads, x, y)
        if leg is None:
            sys.exit(f"no road route from {roads.where[x]} to {roads.where[y]}")
        path += leg[1:]
    pts, secs, dist = track(roads, path, 1_800_000_000)
    turns = maneuvers(roads, path)
    with open(a.out + ".tmp", "w") as f:
        f.write('<?xml version="1.0" encoding="UTF-8"?>\n')
        f.write('<gpx version="1.1" creator="LEHU scripts/route-gpx.py" '
                'xmlns="http://www.topografix.com/GPX/1/1">\n')
        from xml.sax.saxutils import escape
        for i, kind, onto in turns:
            lat, lon = roads.where[path[i]]
            f.write(f'<wpt lat="{lat:.7f}" lon="{lon:.7f}"><name>{escape(onto)}</name>'
                    f"<type>{kind}</type></wpt>\n")
        f.write(f"<trk><name>{escape(a.name)}</name><trkseg>\n")
        for lat, lon, t in pts:
            stamp = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(int(t)))
            f.write(f'<trkpt lat="{lat:.7f}" lon="{lon:.7f}"><time>{stamp}Z</time></trkpt>\n')
        f.write("</trkseg></trk>\n</gpx>\n")
    os.replace(a.out + ".tmp", a.out)
    print(f"{dist / 1000:.1f} km in {secs / 60:.1f} min, {len(pts)} points, "
          f"{len(turns)} turns: {a.out}", flush=True)
    for i, kind, onto in turns:
        print(f"  {kind:14} {onto}", flush=True)


if __name__ == "__main__":
    main()
