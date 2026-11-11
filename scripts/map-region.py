#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Build a whole map region for LEHU from one OpenStreetMap extract, on a PC.

    scripts/map-region.py PBF OUTDIR --preset luxembourg|hungary|auto
                          [--workers N] [--progress] [--voice-dir DIR]
                          [--estimate] [--name NAME] [--work DIR] [--keep-work]
                          [--only graph|tiles|manifest]

OUTDIR becomes a region folder, ready to copy to the card's LEHU-MAPS
partition as /maps/<region>/ (see docs/map.md, "Data on the card"). The PC
tool (tools/lehu-maps) runs this script for the user; it can be run by
hand as well.

    map.toml        the region: its name, zooms and area, the data version,
                    the extract's date, the preset, and the size and sha256
                    of every other file; written last
    tiles.pmtiles   the tiles (scripts/map-tiles.py --pack)
    labels.pack     the street and place names LEHU draws (the same)
    roads.bin, places.bin, and what else scripts/map-graph.py writes:
                    the roads to route over, the names to search

The road data is written first, then the tiles are drawn as loose files
into a work directory (OUTDIR/.work unless --work names another), the
names placed, both packed into OUTDIR, and the work directory removed
(--keep-work keeps it). --only does one part, and the manifest after it.

A preset says which zooms are drawn where. A whole country at the deepest
zooms would be far too large for the card and take days on a PC, so the
deep zooms are drawn only over towns:

    luxembourg   zoom 8-14 over the extract, 15-16 over Luxembourg City
    hungary      zoom 8-14 over the extract, 15-16 over Budapest and over
                 every city or town of 50,000 people or more
    auto         zoom 8-14 over the extract, 15-16 over every place tagged
                 city or town with a population of 50,000 or more

(the towns from the extract's place nodes and their population tags).
The names are always data, which LEHU draws, placed for the zooms the map
shows: to two past the deepest tiles.

--voice-dir DIR names a set of spoken prompts' clips, as
scripts/voice-clips.py makes them; it is checked (every clip there) and
given its own map.toml (each clip's size and sha256), to be copied to the
card as /maps/voice/<lang>/ beside the region.

--progress is for a program running this one: stdout then carries only
lines for it, and everything else goes to stderr:

    @stage NAME          graph, tiles, labels, pack, manifest, in that order
    @progress DONE TOTAL within the stage
    @done                the region is complete
    @error MESSAGE       before a non-zero exit

--estimate prints "@estimate BYTES SECONDS", a rough size and time for the
extract and the preset (on a Raspberry Pi 5 class PC with --workers), and
exits. SIGTERM cancels: the script and the ones it runs stop, and OUTDIR is
left as far as it got.

It runs scripts/map-tiles.py and scripts/map-graph.py, so it needs what
they need (python3 -m venv v && v/bin/pip install osmium pillow). Map data
(c) OpenStreetMap contributors, ODbL 1.0.
"""

import argparse
import datetime
import hashlib
import math
import os
import shutil
import signal
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
    "auto": {
        "name": None,  # the extract's
        "zoom": "8-14",
        "deep": "15-16",
        "boxes": [],
        "towns": 50_000,
    },
}

# A town's deep box: this far from its place node each way, kilometres,
# by its population.
TOWN_KM = ((100_000, 6.0), (0, 4.0))

# --estimate: the region's bytes for an extract's, and seconds an extract
# MB on two workers, measured on a Raspberry Pi 5 (Luxembourg: 48 MB to
# 34 MB in 200 s; Hungary: see docs/map.md). The time of drawing tiles
# shrinks with more workers, reading and placing names does not.
ESTIMATE = {
    # preset: (region bytes an extract byte, seconds an MB read and placed,
    #          seconds an MB drawn on one worker)
    "luxembourg": (0.71, 2.0, 4.6),
    "hungary": (1.0, 6.5, 7.5),
    "auto": (1.0, 6.5, 7.5),
}

PROGRESS = False
CHILD = None


def say(text):
    """A line for people: stdout, or stderr with --progress."""
    print(text, file=sys.stderr if PROGRESS else sys.stdout, flush=True)


def emit(line):
    """A line for the program running this one (--progress only)."""
    if PROGRESS:
        print(line, flush=True)


class Failed(Exception):
    pass


def on_term(signum, frame):
    """A cancel: what runs now stopped too, then out; OUTDIR stays as it
    is, for the caller to remove."""
    if CHILD is not None and CHILD.poll() is None:
        CHILD.terminate()
        try:
            CHILD.wait(10)
        except subprocess.TimeoutExpired:
            CHILD.kill()
    os._exit(128 + signum)


def run(args, stdout=None):
    """Run a script, its @ lines passed on as they are; Failed when it
    fails."""
    global CHILD
    say("+ " + " ".join(args))
    CHILD = subprocess.Popen(args, stdout=stdout)
    code = CHILD.wait()
    CHILD = None
    if code != 0:
        raise Failed(f"{os.path.basename(args[1])} failed (exit {code})")


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


def extract_name(extract):
    """"hungary-latest.osm.pbf" -> "Hungary"."""
    base = os.path.basename(extract).split(".")[0]
    for tail in ("-latest", "-internal"):
        base = base.removesuffix(tail)
    return " ".join(w.capitalize() for w in base.replace("_", "-").split("-") if w) or "Map"


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


def file_lines(folder, files, tick):
    """`[files]`: each file's size and sha256."""
    lines = ["[files]"]
    for f in files:
        path = os.path.join(folder, f)
        lines.append(f"{toml_str(f)} = {{ size = {os.path.getsize(path)}, "
                     f"sha256 = {toml_str(sha256(path))} }}")
        tick()
    return lines


def write_toml(folder, lines):
    tmp = os.path.join(folder, "map.toml.tmp")
    with open(tmp, "w") as f:
        f.write("\n".join(lines) + "\n")
    os.replace(tmp, os.path.join(folder, "map.toml"))


def manifest(out, work, preset, extract, name, voice):
    """OUT/map.toml: the tile set's description (from the work directory's,
    else OUT's own) with the data version, the extract's date, the preset
    and every file's size and sha256; and the voice set's, when there is
    one."""
    source = os.path.join(work, "map.toml")
    if not os.path.exists(source):
        source = os.path.join(out, "map.toml")
    if not os.path.exists(source):
        raise Failed(f"no tiles in {out}: build them first (--only tiles)")
    with open(source, "rb") as f:
        meta = tomllib.load(f)
    files = sorted(
        f for f in os.listdir(out)
        if f != "map.toml" and not f.startswith(".") and not f.endswith(".tmp")
        and os.path.isfile(os.path.join(out, f)))
    clips = []
    if voice:
        clips = sorted(f for f in os.listdir(voice) if f.endswith(".wav"))
    total = len(files) + len(clips)
    done = [0]

    def tick():
        done[0] += 1
        emit(f"@progress {done[0]} {total}")

    emit(f"@progress 0 {total}")
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
    ]
    lines += file_lines(out, files, tick)
    write_toml(out, lines)
    size = sum(os.path.getsize(os.path.join(out, f)) for f in files)
    say(f"map.toml: {len(files)} files, {size / 1e6:.1f} MB")
    if voice:
        lang = os.path.basename(os.path.normpath(voice))
        vlines = [
            "# Written by scripts/map-region.py: a set of spoken prompts.",
            f"name = {toml_str(lang)}",
            f"data-version = {DATA_VERSION}",
            "",
        ]
        vlines += file_lines(voice, clips, tick)
        write_toml(voice, vlines)
        say(f"{voice}/map.toml: {len(clips)} clips")


# The clips LEHU's prompts are made of (scripts/voice-clips.py writes
# them): a few that every set has, to tell a voice set from another folder.
VOICE_SAMPLE = ("turn-left", "turn-right", "then", "arrived")


def check_voice(voice):
    if not os.path.isdir(voice):
        raise Failed(f"--voice-dir {voice}: no such folder")
    missing = [c for c in VOICE_SAMPLE if not os.path.exists(os.path.join(voice, c + ".wav"))]
    if missing:
        raise Failed(f"--voice-dir {voice}: no {', '.join(missing)} clips "
                     "(scripts/voice-clips.py makes them)")


def estimate(extract, preset, workers):
    """Rough bytes and seconds for the region."""
    mb = os.path.getsize(extract) / 1e6
    ratio, fixed, drawn = ESTIMATE[preset]
    seconds = mb * (fixed + drawn * 2 / max(1, workers))
    return int(mb * 1e6 * ratio), int(seconds)


def build(a):
    preset = PRESETS[a.preset]
    name = a.name or preset["name"] or extract_name(a.extract)
    out = a.out.rstrip("/") or "."
    work = a.work or os.path.join(out, ".work")
    os.makedirs(out, exist_ok=True)
    t = time.time()
    if a.voice_dir:
        check_voice(a.voice_dir)
    if a.only in (None, "graph"):
        emit("@stage graph")
        emit("@progress 0 1")
        # Its words are for people: on stderr while stdout is the caller's.
        run([a.python, os.path.join(HERE, "map-graph.py"), a.extract, out],
            stdout=sys.stderr if PROGRESS else None)
        emit("@progress 1 1")
    if a.only in (None, "tiles"):
        boxes = list(preset["boxes"])
        if preset["towns"]:
            more = towns(a.extract, preset["towns"], boxes)
            say(f"{len(more)} towns of {preset['towns']} people or more: "
                + ", ".join(n for n, _ in more))
            boxes += more
        z0, z1 = (int(v) for v in preset["zoom"].split("-"))
        d1 = int(preset["deep"].split("-")[1])
        # The tiles, then the names, then both packed: map-tiles.py says
        # its own stages.
        args = [a.python, os.path.join(HERE, "map-tiles.py"), a.extract, work,
                "--zoom", preset["zoom"], "--labels", "data",
                "--label-zoom", f"{z0}-{max(z1, d1)}", "--jobs", str(a.workers),
                "--pack", out, "--name", name]
        for _, (w, s, e, n) in boxes:
            args += ["--area", f"{preset['deep']}:{w:.5f},{s:.5f},{e:.5f},{n:.5f}"]
        if PROGRESS:
            args.append("--progress")
        run(args)
    emit("@stage manifest")
    manifest(out, work, a.preset, a.extract, name, a.voice_dir)
    if a.only is None and not a.keep_work and not a.work:
        shutil.rmtree(work, ignore_errors=True)
    say(f"region {out} done in {time.time() - t:.0f} s")


def main():
    global PROGRESS
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("extract", metavar="PBF")
    ap.add_argument("out", metavar="OUTDIR")
    ap.add_argument("--preset", required=True, choices=sorted(PRESETS))
    ap.add_argument("--workers", "--jobs", dest="workers", type=int,
                    default=os.cpu_count() or 1, help="processes drawing tiles")
    ap.add_argument("--progress", action="store_true",
                    help="machine-readable lines on stdout (@stage, @progress, @done, @error)")
    ap.add_argument("--voice-dir", default=None,
                    help="a voice set (scripts/voice-clips.py) to check and give a map.toml")
    ap.add_argument("--estimate", action="store_true",
                    help='print "@estimate BYTES SECONDS" and exit')
    ap.add_argument("--name", default=None,
                    help="the region's name; the preset's, or the extract's for auto")
    ap.add_argument("--work", default=None, help="the loose tiles' directory (OUTDIR/.work)")
    ap.add_argument("--keep-work", action="store_true", help="keep the loose tiles")
    ap.add_argument("--only", choices=("graph", "tiles", "manifest"), default=None)
    ap.add_argument("--python", default=sys.executable,
                    help="the Python with osmium and Pillow to run the other scripts")
    a = ap.parse_args()
    PROGRESS = a.progress
    signal.signal(signal.SIGTERM, on_term)
    try:
        if not os.path.isfile(a.extract):
            raise Failed(f"no extract at {a.extract}")
        if a.estimate:
            size, seconds = estimate(a.extract, a.preset, a.workers)
            print(f"@estimate {size} {seconds}", flush=True)
            return
        build(a)
    except Failed as e:
        emit(f"@error {e}")
        say(f"map-region: {e}")
        sys.exit(1)
    except Exception as e:  # noqa: BLE001 - said to the caller, then raised
        emit(f"@error {type(e).__name__}: {e}")
        raise
    emit("@done")


if __name__ == "__main__":
    main()
