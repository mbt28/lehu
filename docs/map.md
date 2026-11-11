# The offline map

Home > Map shows an OpenStreetMap map from the SD card, with no phone and
no internet. It scrolls and zooms, finds a destination by name and the
fastest route there, and guides along it. There is no GPS yet: the
position is a stand-in (see "Where we are"), and a route found is driven
by itself from there, as a replayed drive is.

## Controls

| Input | What it does |
|---|---|
| Drag | moves the map with the finger |
| Arrow keys | move it a quarter of the screen |
| Knob, or the - and + keys of the bar | zoom out and in |
| Enter | zoom in |
| The bar's Me key | centre on where we are, and follow it |
| The bar's Go to key | find a destination and drive there ([below](#going-somewhere)) |
| The bar's Stop key, on a route found there | end the route |
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

`scripts/route-gpx.py` makes such a drive on a PC from the same extract as
the tiles: the fastest car route between two points, keeping to one-way
streets, timed at each road's speed, with its turns and the roads' speed
limits (OSM `maxspeed`, where the road has one). The head unit finds
routes of its own too: see [Going somewhere](#going-somewhere).

```sh
v/bin/python scripts/route-gpx.py luxembourg-latest.osm.pbf \
    49.6045,6.1300 49.6188,6.1440 drive.gpx --name "Gare to Kirchberg"
```

## Spoken prompts

While the map guides, LEHU says the turns, in English: early, about half
a minute ahead ("In 500 metres, turn left" in town, "In 1 kilometre" on a
motorway), near, about ten seconds or 200 m ahead ("In 200 metres, turn
left"), and as the turn comes ("Now, turn left"; at a roundabout "Take
the second exit"). Each is said once. When a turn comes soon after the
last, the prompts that no longer fit are left out; when the next turn is
closer than the near prompt's distance, it is told with this one ("In 200
metres, turn left, then turn right") and only its last prompt is left to
say. The destination is "You will reach your destination" ahead and "You
have arrived at your destination" there. Distances are spoken round: 50 m
steps up to 250 m, 100 m steps up to 900 m, then 1, 1.5 and 2 km.

The prompts play through LEHU's own audio at the navigation level
(`audio.navigation-level`), ducking the music as the phone's voice does.
They do not talk over the phone: while a call, the phone's assistant or
its own navigation is speaking, a prompt waits for half a second of quiet,
and one that has waited a second and a half is dropped, since a turn
announced late is worse than one not announced. `map.voice = false` turns
them off.

A prompt is put together from clips, WAV files of a phrase each ("In 500
metres", "turn left", "then", "Take the second exit"), read from
`map.voice-dir` (`/var/lib/lehu/voice/en` by default) as they are first
needed. They are made on a PC by `scripts/voice-clips.py`, with
[Piper](https://github.com/OHF-voice/piper1-gpl), an offline speech
synthesizer, and a voice from its catalogue. `en_GB-cori-high`, a British
English voice, is in the public domain: its recordings are LibriVox's and
the model was trained from scratch on them. Nothing is installed system
wide:

```sh
python3 -m venv voice-venv
voice-venv/bin/pip install piper-tts
voice-venv/bin/python -m piper.download_voices --data-dir voices en_GB-cori-high
voice-venv/bin/python scripts/voice-clips.py voices/en_GB-cori-high.onnx voice/en \
    --demo demo.wav
```

The script writes the forty-one clips, about 2.8 MB, into `voice/en`: 16-bit
mono PCM at 24 kHz, half the mixer's rate, their silent ends trimmed and
all at one loudness, so the board only copies them. `--demo` also writes
a few whole prompts, put together as the board does, to hear on the PC.
Copy the directory to the head unit as `map.voice-dir`, readable by
LEHU's user. A clip that is missing is named once in the log, and the
prompts that need it stay silent. Routes found on the board (below) are
spoken the same way, and so is the speed camera warning ("Speed camera
ahead", the clip `speed-camera`).

## Going somewhere

The bar's Go to key, there when the map has the road network and the
names (see [Routing data](#routing-data)), opens the destination screen.
It opens on the destinations driven to last, when there are some: one
tap on one drives there again. Otherwise, or after a tap on the top row,
it is a keyboard: type a few letters of a street or a place, and the two
best matches show over the keyboard, each with its town and how far it is
from here. A tap on one finds the route; the keyboard's List key shows
all the matches as rows (a knob or a remote walks them, Enter takes one).
Case and accents never matter, and every word typed has to begin a word
of the name or of its town: "gare" finds Gare and Avenue de la Gare,
"gare esch" the Rue de la Gare in Esch-sur-Alzette. Places come before
streets, and among equal matches the nearest first.

The route is found on the head unit, from where we are now (the replayed
drive's position, else `map.fake-position`, else the middle of the map) to
the nearest road to the destination: the fastest by car, keeping to
one-way streets and roundabouts, timed at each road's speed. The screen
says "Finding the route" meanwhile, then goes back to the map, which
drives the route at the roads' speeds (slowing for sharp turns), as it
replays a drive: the route line with the part driven grey, the next turn
on the bar, closer in near turns, the speed limit and the time of
arrival. It replaces a replayed drive. Leaving the map and coming back
keeps it, driven on meanwhile. The bar's Stop key ends it, and so does
arriving, half a minute later; where it ended is the position from then
on (until LEHU restarts). When there is no route, the screen says why.

### Route options

The route is the fastest unless the settings say otherwise:

```toml
[map]
route-by = "shortest"     # or "fastest", the default
avoid-motorways = true    # false by default
```

The shortest is the least distance, whatever the roads' speeds. Avoiding
motorways, a motorway or a motorway's link costs five times its time (or
its length), so the route takes another road wherever one goes the same
way in less than several times the time, and still takes the motorway
where nothing else goes (a service area, say). Road data written before
the motorways were marked (`roads.bin` version 1) cannot tell them apart:
there avoiding them changes nothing. The screens change the options by
persisting these two keys; `AppHost::map_route_options` is what a route
is found with.

### Speed cameras

With `map.speed-cameras = true`, LEHU says "Speed camera ahead" once
before each fixed speed camera on the route: twelve seconds of driving
ahead of it, but no nearer than 300 m and no further than 500 m (300 m up
to 90 km/h, 433 m at 130). The phrase takes a second and a half, and
slowing from 130 to 90 km/h comfortably takes seven seconds more, so
twelve leave room; in town 300 m is before the junction or two that come
first. It is off by default: warning of speed cameras is against the law
in some countries (France, Germany and Switzerland among them), so turn
it on only where it is allowed. It needs the prompts on (`map.voice`).
`AppHost::map_camera_ahead(within)` gives the metres to the next camera on
the route within that distance, for a sign on the screen.

The cameras are OpenStreetMap's (`highway=speed_camera`), each taken to
the nearest road within 50 m. A camera watches the traffic its
enforcement relations say (from one node to another), else the way its
`direction=forward` or `backward` says when it is a node of the road,
else both ways; its limit is its `maxspeed`. Luxembourg has 39, all on
roads kept.

### Routing data

`scripts/map-graph.py` writes the road network and the names from the
same extract as the tiles, into two files: `roads.bin` and `places.bin`.

```sh
v/bin/python scripts/map-graph.py luxembourg-latest.osm.pbf maps
```

Luxembourg makes a 6.6 MB `roads.bin` (63,000 junctions, 75,000 roads)
and a 0.5 MB `places.bin` (10,000 streets, 2,200 places) in half a
minute. Copy them into the map's directory, beside `map.toml`, or into
another directory named by `map.route-dir`. The roads and their speeds
are the ones `scripts/route-gpx.py` drives, so both find the same routes;
only the part of the network where every junction can be reached from
every other is kept, so a destination is never in a car park with no way
out.

The files are little-endian 32-bit words, mapped from the card and read
in place rather than loaded, so they cost the head unit page cache, not
memory of its own; a route looks at only part of the network.

`roads.bin` starts with a header of 20 words: "LEHUROAD", the version
(2), the numbers of junctions, arcs, roads, shape points, road kinds,
grid entries and name bytes, the grid (its corner, its cell size as a
shift of world pixels, its columns and rows), the time a straight line
takes at the top speed (milliseconds per 65,536 world pixels), the top
speed (km/h), the number of speed cameras, the top speed of the roads
that are not motorways, and a word kept 0. Version 1 (still read) has the
first 18 words only, no cameras and no motorway marks. Then, each right
after the one before:

| Section | Words each | What |
|---|---|---|
| junctions, and one more | 4 | x, y (world pixels at zoom 22), its first arc, how many roads meet there |
| arcs | 3 | the junction it leads to, milliseconds, road × 2 (+ 1 when it runs the road from its b to its a) |
| roads, and one more | 5 | a, b, decimetres, kind + 2^24 when it may be driven from a to b + 2^25 from b to a + 2^26 a motorway or its link, its first shape point |
| shape points | 1 | all the x, then all the y |
| road kinds | 2 | the name (a byte offset into the names), the speed limit in km/h (0 none) + 2^16 for a roundabout |
| grid cells, and one more | 1 | each cell's first entry |
| grid entries | 1 | the roads with a piece in the cell |
| names | bytes | each a length byte, then UTF-8 |
| speed cameras | 6 | x, y (on the road), the road, decimetres from its a, the ways it watches (1 a to b, 2 b to a, 3 both), its limit in km/h (0 not known) |

The cameras are in the order of their roads, and along each from a. A
junction's arcs are the ways a car may leave it, up to the next
junction's first arc; a road's shape is the points between its a and its
b, up to the next road's first.

`places.bin` starts with 6 words: "LEHUFIND", the version (1), the
numbers of names and towns, and the text's bytes. Then the towns, 2 words
each (the name and its folded key, byte offsets into the text); the
names, 5 words each (x, y, the name, its folded key, and the town's index
+ the kind × 2^16 (0 a street, 1 a place) + the rank × 2^24, lower more
important); then the text, each string a length byte and UTF-8. A key is
the name folded: lower case, without accents, its words split by single
spaces. A street is in the city, town or village nearest it (a city
counts from three times as far, a town twice); it has one name for each
town it runs through, at the middle of its longest piece.

### How routing works

The route is A* by time over the junctions: integer arithmetic only, and
the time still to go is never more than the straight line at the
network's top speed, so the route found is the fastest. The shortest is
the same search by distance, the bound the straight line itself; avoiding
motorways, the bound is the straight line at the fastest other road's
speed (or a fifth of the motorways', when that is more), which no road
beats once motorways cost five times over, so each mode's route is the
best by its own measure. The start and
the goal are taken to the nearest point of the nearest road (the grid
finds it), and setting off against the way the drive is going costs a
U-turn's half minute. The turns are found as `scripts/route-gpx.py`
finds them: at a junction, a turn of 45 degrees or more or a road with
another name; into a roundabout, the exit to take. On a PC the sample
drive above (Gare to Kirchberg) takes about 2 ms; across Luxembourg, Schengen to
Troisvierges (112 km), 15 to 45 ms; a search, 1 ms. The board is some
tens of times slower: the test `routing_cost` in
`crates/lehu-map/src/routing_tests.rs` times both there.

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

The prompts' timing is decided in `crates/lehu-map/src/voice.rs`, each
tick of the bar (five times a second), in integer arithmetic on the
metres to the turn and the speed. A phrase's clips are joined with a short
pause into one segment and handed to the mixer's third input, which plays
LEHU's own prompts beside the phone's music and voice.

## Licence

Map data © OpenStreetMap contributors, under the
[Open Database License](https://www.openstreetmap.org/copyright). The map
screen shows the attribution.

The spoken prompts made with the `en_GB-cori-high` Piper voice are in the
public domain, as the voice is. Another voice comes with a licence of its
own, in its model card: some of Piper's voices are for non-commercial use
only.
