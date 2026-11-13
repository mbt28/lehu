#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""A drive made up, as a timed GPX track, to test how the map turns:
straights, 90 degree corners, S-bends and a U-turn, one point a second
like a GPS, at the speeds a car takes them.

    scripts/rotation-gpx.py OUT.gpx [--at LAT,LON] [--laps N]

The course starts at --at (Budapest's Heroes' Square unless named) and
goes round it; it follows no road, so it needs no map data, only tiles
to look at. LEHU replays it as map.fake-track; with LEHU_MAP_TRACE=1 in
its environment it logs every frame it turns.
"""

import argparse
import math
import sys

# The course: (kind, length or angle, radius, speed km/h).
#   straight: metres;  arc: degrees (left negative), radius in metres.
COURSE = [
    ("straight", 300, 0, 50),
    ("arc", 90, 12, 18),       # a right corner
    ("straight", 150, 0, 40),
    ("arc", -90, 12, 18),      # a left corner
    ("straight", 100, 0, 40),
    ("arc", 60, 40, 35),       # an S-bend
    ("arc", -60, 40, 35),
    ("straight", 200, 0, 50),
    ("arc", 180, 15, 15),      # a U-turn
    ("straight", 200, 0, 50),
    ("arc", -45, 80, 50),      # a gentle bend
    ("arc", 45, 80, 50),
    ("straight", 100, 0, 30),
    ("arc", -90, 8, 12),       # a tight left
    ("straight", 150, 0, 40),
]


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("out")
    p.add_argument("--at", default="47.5150,19.0778", help="where it starts, LAT,LON")
    p.add_argument("--laps", type=int, default=1)
    a = p.parse_args()
    lat0, lon0 = (float(v) for v in a.at.split(","))
    m_lat = 111_320.0
    m_lon = 111_320.0 * math.cos(math.radians(lat0))
    x = y = 0.0           # metres east, north
    heading = 0.0         # radians, 0 north, clockwise
    t = 0.0
    points = []

    def emit(speed_kmh):
        points.append((x, y, t))

    emit(50)
    for _ in range(a.laps):
        for kind, amount, radius, kmh in COURSE:
            v = kmh / 3.6
            if kind == "straight":
                n = max(1, round(amount / v))
                for _ in range(n):
                    x += math.sin(heading) * v
                    y += math.cos(heading) * v
                    t += 1
                    emit(kmh)
            else:
                length = abs(math.radians(amount)) * radius
                n = max(1, round(length / v))
                turn = math.radians(amount) / n
                for _ in range(n):
                    heading += turn / 2
                    x += math.sin(heading) * (length / n)
                    y += math.cos(heading) * (length / n)
                    heading += turn / 2
                    t += 1
                    emit(kmh)
    lines = ['<?xml version="1.0" encoding="UTF-8"?>',
             '<gpx version="1.1" creator="LEHU scripts/rotation-gpx.py" '
             'xmlns="http://www.topografix.com/GPX/1/1">',
             "<trk><trkseg>"]
    for px, py, pt in points:
        lat = lat0 + py / m_lat
        lon = lon0 + px / m_lon
        s = int(pt)
        lines.append(f'<trkpt lat="{lat:.7f}" lon="{lon:.7f}">'
                     f'<time>2027-01-15T08:{s // 60:02d}:{s % 60:02d}Z</time></trkpt>')
    lines += ["</trkseg></trk>", "</gpx>"]
    with open(a.out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"{len(points)} points, {int(t)} s: {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
