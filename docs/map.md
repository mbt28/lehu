# The offline map

Home > Map shows an OpenStreetMap map from the SD card, with no phone and
no internet. It is the first step towards offline navigation: today the
map can be scrolled and zoomed; the position, routes and guidance come
later.

## Controls

| Input | What it does |
|---|---|
| Drag | moves the map with the finger |
| Arrow keys | move it a quarter of the screen |
| Knob, or the - and + keys of the bar | zoom out and in |
| Enter | zoom in |
| The bar's Me key | centre on where we are, and follow it |
| Back, or the bar's Back key | leave the map |

LEHU opens the map where it was left, or on where we are when it knows.

## Where we are

There is no GPS receiver yet. For testing, `map.fake-position` in the
settings gives a position, "latitude, longitude" in degrees:

```toml
[map]
fake-position = "49.6116, 6.1300"
```

The map then opens on it and shows it as a blue dot, and the bar gets a Me
key that centres the map on it again.

A recorded drive can stand in for a moving position: `map.fake-track`
names a GPX file, which LEHU follows in real time, from the start again at
its end. The position is a blue arrow along the road, and the map follows
it until it is dragged; the Me key makes it follow again. While it
follows, the map turns so that the way ahead is up, with more road ahead
than behind. When the GPX
has the route's turns as waypoints, the map draws the route and guides
along it: the bar shows the next turn, its distance and the road it leads
onto, the time of arrival, and the time and distance left. The route
already driven turns grey. Following, the map zooms in a level within
250 m of a turn, and back out after it. Where the drive's points carry
the road's speed limit, the bar shows a limit sign beside the speed, the
speed in red when it is over.

`scripts/route-gpx.py` makes such a drive from the same extract as the
tiles: the fastest car route between two points, keeping to one-way
streets, timed at each road's speed, with its turns and the roads' speed
limits (OSM `maxspeed`, where the road has one).

```sh
v/bin/python scripts/route-gpx.py luxembourg-latest.osm.pbf \
    49.6045,6.1300 49.6188,6.1440 drive.gpx --name "Gare to Kirchberg"
```

## Making a map

The tiles are drawn on a PC from an OpenStreetMap extract, for example
one from [Geofabrik](https://download.geofabrik.de):

```sh
python3 -m venv v && v/bin/pip install osmium pillow
v/bin/python scripts/map-tiles.py luxembourg-latest.osm.pbf maps --zoom 8-16
```

The script writes `maps/<z>/<x>/<y>.jpg` (256 x 256 baseline JPEGs) and
`maps/map.toml`, which names the zoom levels, the area and where to open.
Copy the `maps` directory to the head unit as `map.dir`
(`/var/lib/lehu/maps` by default), readable by LEHU's user. A whole
country takes a while on a PC: Luxembourg, zoom 8 to 16, is 41,000 tiles,
240 MB and five minutes on four cores. Most of that is the last two zoom
levels (zoom 8 to 14 is 26 MB), so mind the space on the card: the system
updater needs room of its own there.

At night (Settings, Night) the map shows night tiles, dark with light
names, when the map has them: a second set in `night/` under the map's
directory, drawn with `--night` over the same area and zooms. The night
set of the Luxembourg example is about 30 MB.

```sh
v/bin/python scripts/map-tiles.py luxembourg-latest.osm.pbf maps/night --zoom 8-16 --night
```

The tiles are drawn by the script, never fetched from OpenStreetMap's tile
servers: their usage policy forbids offline use. The drawing is plain:
land use, water, buildings, roads and railways, with the names of streets
(from zoom 13 for primary roads and up, 14 for secondary and tertiary
roads, 15 for the rest, 17 for service roads and paths) and of places, in
LEHU's own font. The names are placed once for the whole map at each zoom,
so they never overlap and run on across a tile's edge. `--font` takes
another TrueType font.

## Names

The names can be drawn into the tiles (`--labels draw`, the default) or
left to LEHU (`--labels data`; `both` does both, `none` or `--no-labels`
neither). Names in the tiles are small (12 pixels), turn with the map
heading up and can then read upside down. Names LEHU draws itself are 20
to 24 pixels on the panel at every zoom, are drawn over the tiles and the
route, and always read the right way up: a street's name lies along the
street, turned half round when it would read upside down, and a place's
name stays level on the panel.

With `--labels data` the script places the names for the size LEHU draws
them, for every zoom from the first to two past the last (the zooms LEHU
enlarges the tiles to; `--label-zoom` names others), none overlapping
another, and writes them to `labels/` beside the tiles: a small text file
a block of 8 x 8 tiles, `labels/<z>/<bx>/<by>.txt`, each line a label
(kind, place, angle, size and text; the script's header describes the
format). The same data serves the day and the night tiles: LEHU picks the
colours, dark text on a white halo by day, light on a dark halo at night.
Where a zoom has not covered a block (`labels/<z>/areas.txt`) LEHU draws
the names of a zoom or two out, so a pass over a city at deeper zooms can follow one over the whole
country. A set without `labels/` has its names in the tiles, and LEHU
draws none.

The Luxembourg example with names as data, the city deeper:

```sh
P="v/bin/python scripts/map-tiles.py luxembourg-latest.osm.pbf"
CITY="--zoom 15-16 --bbox 6.02,49.56,6.22,49.67"
$P maps --zoom 8-14 --labels data --label-zoom 8-16
$P maps $CITY --labels data
$P maps/night --zoom 8-14 --labels none --night
$P maps/night $CITY --labels none --night
```

The second pass writes a `map.toml` for its own zooms and area: set
`min-zoom` and `bounds` back to the first pass's (and the same in
`night/`). `--no-tiles` writes only the label data, to place the names
again without drawing the tiles again. The data is small: 320 KB in 780
files for the example (3.4 MB on the card, a 4 KB block a file).

## How it works

The map is shown on the video plane, beneath the screens, like the camera.
Tiles are decoded into memory and composed into a canvas a tile larger than
the panel on each side; the display engine shows a panel-sized window of
it. Scrolling moves the window, which costs one commit and no copy; when
the window would leave the canvas, the next canvas is composed around it.
Everything runs on a thread of its own, so the screens and the input never
wait for a decode.

A drive moves the map by itself, from a thread of its own that wakes at
every second refresh of the panel, so the map moves in even steps, thirty
times a second.

Heading up, the map is turned on the CPU into a picture a little larger
than the panel, with some road ahead; driving on moves the window over it,
and the map is turned anew only when the heading has changed by most of a
degree or the road ahead runs out. The arrow is on a display plane of its
own above the map, so the map moves under it without being redrawn; on a
display without such a plane it is drawn into the map instead. The phone's decoder makes way while the map is shown,
since both need video memory.

Names LEHU draws itself are laid out from the font's glyphs, each
rasterized once a size (by swash, the rasterizer the screens' toolkit
links in already) and widened into its halo; a name's picture is kept
while it is drawn. On a canvas or a turned frame each pixel the name can
cover samples its picture in integer fixed point, the right way up, so the
names add a few milliseconds to a frame.

The canvas is NV12, the video plane's own format, so the display's scaler
does the colour conversion. Tiles are decoded by the video engine when the
kernel's cedrus driver has JPEG support (about a millisecond a tile on the
F1C200s), then copied out of its tiled layout. A tile the engine refuses,
such as a progressive or 4:4:4 JPEG, or any tile on a system without the
engine, is decoded in software, which on the F1C200s takes about a quarter
of a second. `scripts/map-tiles.py` writes baseline 4:2:0 tiles, which the
engine takes.

## Licence

Map data © OpenStreetMap contributors, under the
[Open Database License](https://www.openstreetmap.org/copyright). The map
screen shows the attribution.
