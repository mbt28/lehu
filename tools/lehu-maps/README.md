# LEHU Maps

A small desktop program that puts LEHU's map regions and the spoken
prompts' voice on the head unit's SD card. Take the card out of the head
unit, put it in the PC's card reader, and in one window:

1. pick a region (any country or sub-region Geofabrik offers);
2. pick a voice (English Piper voices; or none);
3. check that the card's **LEHU-MAPS** drive is found (or type a folder);
4. press **Write to card**.

It then:

- downloads the region's OpenStreetMap extract (`.osm.pbf`) from
  Geofabrik into its cache, continuing a download that stopped, and
  checks it against Geofabrik's `.md5`; the same extract is not
  downloaded twice;
- sets up a Python environment of its own in the cache the first time
  (`osmium`, `pillow`, `numpy`, `piper-tts`);
- asks `scripts/map-region.py --estimate` for the region's size and time,
  and stops at once when the card has no room for it;
- builds the region with `scripts/map-region.py --progress` (the bar
  follows its stages: road network, tiles, names, packing, checksums);
  a region built once is kept in the cache per extract date and zoom
  preset, so a second card is written in a minute;
- downloads the Piper voice and makes the 41 clips with
  `scripts/voice-clips.py`, also kept in the cache;
- copies both to the card into `.incoming/`, flushes them, reads every
  file back and checks its sha256 against the folder's `map.toml`, and only
  then renames it into place, replacing an older copy of the same region.

The card's layout (see [docs/map.md](../../docs/map.md#data-on-the-card)):

```
<LEHU-MAPS>/<region>/      map.toml, tiles.pmtiles, labels.pack, roads.bin,
                           places.bin, cameras.bin
<LEHU-MAPS>/voice/en/      the 41 WAV clips and their map.toml
```

The window also lists what is on the card (each region's name, OSM date
and size; the voice) with **Remove**, and the free space. Cancel stops
any step: a download continues from where it stopped next time, and
nothing on the card changes until a new copy has been checked. **LEHU Maps
never partitions or formats anything**: the LEHU-MAPS partition is made by
the head unit's image.

When it says it is done, eject the drive (Windows: "Safely remove", macOS:
Eject in the Finder, Linux: the file manager's eject, or `umount`) before
taking the card out.

## What it needs

- **Python 3.11 or later** on the PC (Windows: from python.org, with "Add
  python.exe to PATH"; macOS: python.org or Homebrew; Linux: `python3` and
  `python3-venv`). The packages are installed into the tool's own
  environment, never into the system's.
- **A LEHU checkout**, for its `scripts/`: start the program from inside
  it (`cargo run` in this folder does), or give `--scripts DIR`.
- About 1 GB in the cache for a country like Hungary (the extract, the
  build), more for larger regions. Building Hungary takes over an hour on a
  Raspberry Pi 5 class PC; a small country (Luxembourg) a few minutes.

## Building and running

It is a cargo workspace of its own (not part of the head unit's, which is
built for the board):

```
cd tools/lehu-maps
cargo run --release
```

- **Linux:** a Rust toolchain (1.88 or later) and a C compiler. The window
  uses winit (X11 or Wayland) with OpenGL (femtovg); the libraries are
  loaded at run time, so no `-dev` packages are needed to build. Without
  OpenGL, `SLINT_BACKEND=winit-software cargo run` draws in software.
- **Windows:** Rust with the MSVC toolchain (Visual Studio Build Tools,
  "Desktop development with C++"). `cargo build --release` gives
  `target\release\lehu-maps.exe`, a window program with no console.
- **macOS:** Rust and the Xcode command line tools (`xcode-select
  --install`).

A CI building it for the three would need only the stock GitHub runners
(`ubuntu-latest`, `windows-latest`, `macos-latest`) with
`dtolnay/rust-toolchain@stable`, then `cargo build --release --locked` and
`cargo test --locked` in `tools/lehu-maps`.

Options:

```
lehu-maps [--scripts DIR] [--workers N] [--card DIR]
lehu-maps --write REGION --card DIR [--voice KEY|none]   the same work without the window
lehu-maps --screenshot DIR    the window's states as PNGs (offscreen), for the docs
```

`--write luxembourg --card /media/me/LEHU-MAPS` does what **Write to
card** does for Geofabrik's region `luxembourg`, with its stages on the
terminal (for scripts, or a PC without a desktop).

`--workers` is how many processes draw tiles (default: the cores less
one). `--card DIR` writes to a folder instead of a LEHU-MAPS drive (also in
the window: type the folder's path and press **Use folder**).

## Where things are kept

The cache is `LEHU_MAPS_CACHE` when set, else:

| System | Cache |
|---|---|
| Linux | `~/.cache/lehu-maps` (or `$XDG_CACHE_HOME/lehu-maps`) |
| macOS | `~/Library/Caches/lehu-maps` |
| Windows | `%LOCALAPPDATA%\lehu-maps` |

In it: `extracts/<region>/` (the `.osm.pbf` and its checked md5; an older
extract of the region is removed once a newer one is downloaded),
`venv/` (the Python environment), `regions/<region>/<OSM date>-<preset>/`
(built regions), `piper/<voice>/` (the voice models), `voices/` (the
clips), and Geofabrik's and Piper's lists. Deleting the folder is safe:
everything in it is downloaded or built again when needed.

How the card is found: on Linux `lsblk` (or `/dev/disk/by-label` and
`/proc/mounts`), on macOS `/Volumes/LEHU-MAPS`, on Windows the drives'
volume labels. The drive must be mounted (the desktop does that when the
card goes in).

## Data sources and licences

- **Map data** © OpenStreetMap contributors, under the Open Database
  Licence 1.0 (<https://www.openstreetmap.org/copyright>). A map made from
  it must say so; LEHU shows the attribution on the map.
- **Extracts** from Geofabrik GmbH (<https://download.geofabrik.de>),
  which publishes OpenStreetMap data in regions; the list is its
  `index-v1-nogeom.json`. The program asks for each file once, with the
  User-Agent `lehu-maps/<version>` and nothing about the user.
- **Voices** from Piper's catalogue, `rhasspy/piper-voices` on Hugging
  Face (<https://huggingface.co/rhasspy/piper-voices>); each voice has its
  own licence in its `MODEL_CARD` there. The default, `en_GB-cori-high`,
  is in the public domain (trained on LibriVox recordings). The speech is
  made on the PC by `piper-tts` (GPL-3.0).
- The program itself is GPL-3.0-or-later, like LEHU. It uses Slint
  (GPL-3.0 for open-source programs), ureq and rustls, serde, toml, sha2
  and md-5, and dirs.

No map, voice or other navigation data is in LEHU's repository: it is
always downloaded or built by the user.
