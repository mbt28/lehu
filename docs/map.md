# The offline map

Home > Map shows an OpenStreetMap map from the SD card, with no phone and
no internet. It scrolls and zooms, finds a destination by name (or the
nearest fuel, parking or charging) and the fastest or shortest route
there, and guides along it. There is no GPS yet: the
position is a stand-in (see "Where we are"), and a route found is driven
by itself from there, as a replayed drive is.

## Controls

Everything works with the knob alone and with touch alone, the same way.
The bar's keys never move: at its right end `-`, `+`, the menu key and
Back; just left of them a Me key while the map has been moved off where
we are, and a muted key left of that while the spoken prompts are off.
The rest of the bar is words: the map's name and zoom, or, while it
guides, the next turn. Only while a route just found is shown whole do
Start and Cancel take the keys' place ([below](#the-whole-route-first)).

| Input | What it does |
|---|---|
| Drag | moves the map with the finger |
| Arrow keys | move it a quarter of the screen |
| Knob turn, or the bar's - and + | zoom out and in |
| Knob click, or the bar's menu key | open the map's menu |
| Back, or the bar's Back key | leave the map |
| The bar's Me key, there while the map is off where we are | centre on where we are, and follow it |
| The bar's muted key, there while the voice is off | the spoken prompts on again |

The map's menu is a column at the right, over the map, the bar's keys at
its foot. Its rows, those that apply:

| Row | What it does |
|---|---|
| My position | as the Me key, while the map is off where we are |
| Stop route | ends a route found on the head unit |
| Go to | find a destination and drive there ([below](#going-somewhere)) |
| Home, Work | drive there at once; under the name, the place and how far. Not set yet: choose it (below) |
| Route | how routes go, under it what is chosen ("Fastest", "Shortest, no motorways"): opens its choices ([below](#route-options)) |
| Voice | the spoken prompts on or off (`map.voice`); the menu stays open |

With the knob the ring starts on the first row: turning moves it (round at
the ends), a click chooses, Back closes the menu (a second Back leaves the
map). A tap chooses a row; a tap on the map beside the menu, or the menu
key again, closes it, and it closes by itself eight seconds after it was
last used. Go to, Home, Work and Route need the road data; without it the
menu has only what is left. The rows are 60 pixels tall; when all seven
are there (the map moved off where we are, on a route), they are a little
lower, so all of them fit above the bar.

LEHU opens the map where it was left, or on where we are when it knows.

## Where we are

There is no GPS receiver yet. For testing, `map.fake-position` in the
settings gives a position, "latitude, longitude" in degrees:

```toml
[map]
fake-position = "49.6116, 6.1300"
```

The map then opens on it and shows it as a blue dot; once the map is
moved off it, the bar's Me key (and the menu's My position) centres the
map on it again.

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
them off, and so does the map menu's Voice row, at once.

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

The menu's Go to, there when the map has the road network and the
names (see [Routing data](#routing-data)), opens the destination screen.
It opens on a list: the row to type a name, under it the nearest fuel,
parking and charging ([below](#fuel-parking-and-charging)), then the
destinations driven to last, one choice each. With none of these it opens
on the keyboard. After a tap on the top row (or a letter typed) it is a
keyboard: type a few letters of a street or a place, and the two
best matches show over the keyboard, each with its town and how far it is
from here. A tap on one finds the route; the keyboard's List key shows
all the matches as rows (a knob or a remote walks them, Enter takes one).
Case and accents never matter, and every word typed has to begin a word
of the name or of its town: "gare" finds Gare and Avenue de la Gare,
"gare esch" the Rue de la Gare in Esch-sur-Alzette. Places come before
streets, and among equal matches the nearest first.

The route is found on the head unit, from where we are now (the replayed
drive's position, else `map.fake-position`, else the middle of the map) to
the nearest road to the destination: the fastest by car (or as the
[route options](#route-options) say), keeping to one-way streets and
roundabouts, timed at each road's speed. The screen says "Finding the
route" meanwhile, then goes back to the map, which shows the route whole
first ([below](#the-whole-route-first)), then drives it at the roads'
speeds (slowing for sharp turns), as it
replays a drive: the route line with the part driven grey, the next turn
on the bar, closer in near turns, the speed limit and the time of
arrival. It replaces a replayed drive. Leaving the map and coming back
keeps it, driven on meanwhile, with its prompts, and the navigation card
of the other screens (Home, Now playing, Settings) shows its next turn
while the phone is not guiding. The menu's Stop route ends it, and so does
arriving: the bar (or the card) shows a flag, "Arrived" and the
destination for ten seconds, and "You have arrived at your destination" is
said. Where it ended is the position from then on (until LEHU restarts).
When there is no route, the screen says why.

When the turn after the next one comes within 300 m of it, the bar and
the card show it beside the next one, small, under "then", so both can be
prepared for at once.

### The whole route first

A route to a destination chosen on Go to (a name, a recent one, the
nearest fuel...) is shown whole before it is driven: the map turns north
up and zooms to hold all of it above the bar, where we are and the route
drawn, and the bar shows the destination, its distance, how long it takes
and the time of arrival ("12 km · 14 min · arrival 14:46"). Its keys are
Start, where -, + and the menu key were, and Cancel, where Back was.

The knob's ring is on Start: a click starts, a turn moves the ring to
Cancel (and back), Back cancels. A tap on either does the same. Start
counts down ("Start in 5 s"): left alone for eight seconds, the route
starts by itself, so a driver never has to touch it; any key or a drag
starts the count again, and leaving the map starts it at once. Cancel
leaves the map as it was, a route driven before going on. Meanwhile a
drag or the arrows move the map round the route.

Home and Work from the menu, and a route found anew when the route
options change, are driven at once, without it.

`View::fitting` (in `crates/lehu-map/src/view.rs`) finds the view: the
closest zoom at which the route's extent fits the panel less a margin and
the bar, the route's middle in the middle of what is left.

### Home and Work

The menu's Home and Work drive there with one choice. Not set yet, the row
says "Set Home" and opens the destination screen, titled "Set Home", to
choose it as a destination is chosen; the place is kept (`map.home`,
`map.work`) and the bar says "Home set: ..." for a moment, without driving
there. To change them later, the destination screen's recent list ends
with Change Home and Change Work, for those that are set.

### Route options

The menu's Route row says how routes go ("Fastest", "Shortest, no
motorways"). Chosen, its choices take the menu's place: Fastest and
Shortest (a green dot on the one taken), Avoid motorways (on or off) and
Speed cameras ([below](#speed-cameras)). With the knob the ring starts on
Fastest; a click takes a row, and the menu stays; Back goes back to the
menu's rows, the ring on Route. A tap takes a row too; the menu key, a
tap beside the menu or eight seconds left alone close it all. Once the
choices are left, a route being driven is found anew with them, from
where we are ("Finding the route" on the bar); left unchanged, nothing is.

The choices are kept in the settings:

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
is found with, and `Host::map_reroute` finds the route of ours anew.

### Fuel, parking and charging

Besides names, `places.bin` has the filling stations, the public car
parks and the charging stations of the extract (version 2; one written
before has none). Go to's list has a row of three under the search row,
Fuel, Parking and Charging, those the data has: the knob reaches them in
the list's order, between the search row and the recent destinations.
One chosen lists the eight nearest of its kind to where we are in the
recent ones' place, the nearest first, each with its name and under it a
few words and how far: "Aral · 1.2 km", "Multi-storey, 400 spaces ·
350 m", "4 points, 22 kW · 2.1 km". One of them is picked as any
destination (the route shown whole, then driven). Back, or the same kind
chosen again, lists the recent ones again; another kind lists its own.
Choosing Home or Work offers none. They are not searched by name.

`Host::map_nearest(kind, limit)` fills the list (`Nearby::Fuel`,
`Parking` or `Charging`, the program's `FoundKind`), and
`Host::map_can_find(kind)` says whether there are any.

A car park is one anyone may use (not private, nor only a shop's
customers), not the side of a street, with a name or room for ten cars
or more; a charging station one for cars, not private. Each is at the
middle of its outline when it is mapped as an area, and one mapped twice
(a point and an area, within 60 m with the same name) is one. Luxembourg
has 237 filling stations, 403 car parks and 531 charging stations.

### Speed cameras

With `map.speed-cameras = true` (the Route choices' Speed cameras row
switches it: Warn or Off), LEHU says "Speed camera ahead" once
before each fixed speed camera on the route: twelve seconds of driving
ahead of it, but no nearer than 300 m and no further than 500 m (300 m up
to 90 km/h, 433 m at 130). The phrase takes a second and a half, and
slowing from 130 to 90 km/h comfortably takes seven seconds more, so
twelve leave room; in town 300 m is before the junction or two that come
first. It is off by default: warning of speed cameras is against the law
in some countries (France, Germany and Switzerland among them), so turn
it on only where it is allowed. The spoken warning needs the prompts on (`map.voice`).
While the map guides, from 500 m before a camera the bar shows a red
camera sign and the distance to it, in 50 m steps ("350 m"), in the place
of the time of arrival under the road, until it is passed; with the voice
off too. `Host::map_camera_ahead(within)` gives the metres to the next
camera on the route within that distance.

The cameras are OpenStreetMap's (`highway=speed_camera`), each taken to
the nearest road within 50 m. A camera watches the traffic its
enforcement relations say (from one node to another), else the way its
`direction=forward` or `backward` says when it is a node of the road,
else both ways; its limit is its `maxspeed`. Luxembourg has 39, all on
roads kept; Hungary 480, and two more further from a road kept, left
out. They are in a file of their own, `cameras.bin`
([below](#routing-data)).

### Speed limit

Where the road's speed limit is known, the bar shows its sign beside the
speed, the speed in red when it is over. When the speed stays more than
5 km/h over it for 2 seconds, a short soft chime plays once, through the
same audio as the prompts (waiting while the phone speaks); it chimes
again only after the speed came back to the limit, or on a road with
another limit. `map.speed-warning = false` turns the chime off.

### Routing data

`scripts/map-graph.py` writes the road network, the speed cameras and
the names from the same extract as the tiles, into three files:
`roads.bin`, `cameras.bin` and `places.bin`.

```sh
v/bin/python scripts/map-graph.py luxembourg-latest.osm.pbf maps
```

| | Luxembourg | Hungary |
|---|---|---|
| `roads.bin` | 7.0 MB: 63,000 junctions, 75,000 roads; 3,900 junctions of main roads | 68.6 MB: 620,000 junctions, 770,000 roads, 1.8 million shape points; 18,500 junctions of main roads, 35,000 runs of them |
| `cameras.bin` | 1 kB: 39 speed cameras | 11.5 kB: 480 |
| `places.bin` | 0.5 MB: 10,000 streets, 2,200 places, 1,200 filling stations, car parks and charging stations | 4.1 MB: 119,000 streets, 17,900 places (3,200 towns), 1,800 filling stations, 1,500 car parks, 1,500 charging stations |
| on a PC | half a minute | about 5 minutes and 2.2 GB of memory (a Raspberry Pi 5) |

Copy them into the map's directory, beside `map.toml`, or into another
directory named by `map.route-dir`. `cameras.bin` is optional: without
it, or with one made for other road data, there are no camera warnings
and routes are found as before. The roads and their speeds are the ones
`scripts/route-gpx.py` drives, so both find the same routes; only the
part of the network where every junction can be reached from every other
is kept, so a destination is never in a car park with no way out.

The files are little-endian 32-bit words, mapped from the card and read
in place rather than loaded, so they cost the head unit page cache, not
memory of its own; a route looks at only part of the network.

`roads.bin` starts with a header of 26 words: "LEHUROAD", the version
(3), the numbers of junctions, arcs, roads, shape points, road kinds,
grid entries and name bytes, the grid (its corner, its cell size as a
shift of world pixels, its columns and rows), the time a straight line
takes at the top speed (milliseconds per 65,536 world pixels), the top
speed (km/h), a word kept 0, the top speed of the roads that are not
motorways, a word kept 0, the build's ID (two words, low first), the
numbers of layer junctions, layer roads and steps, and a word kept 0.
Then, each right after the one before:

| Section | Words each | What |
|---|---|---|
| junctions, and one more | 4 | x, y (world pixels at zoom 22), its first arc, how many roads meet there (the low 8 bits) + (its place in the layer + 1) × 2^8, 0 when it is not a layer junction |
| arcs | 3 | the junction it leads to, milliseconds, road × 2 (+ 1 when it runs the road from its b to its a) + 2^31 for a main road |
| roads, and one more | 5 | a, b, decimetres, kind + 2^24 when it may be driven from a to b + 2^25 from b to a + 2^26 a motorway or its link, its first shape point |
| shape points | 1 | all the x, then all the y |
| road kinds | 2 | the name (a byte offset into the names), the speed limit in km/h (0 none) + 2^16 for a roundabout |
| grid cells, and one more | 1 | each cell's first entry |
| grid entries | 1 | the roads with a piece in the cell |
| names | bytes | each a length byte, then UTF-8 |
| layer junctions, and one more | 4 | its junction, x, y, its first layer road |
| layer roads, and one more | 6 | the layer junction it leads to, milliseconds, decimetres, the milliseconds and the decimetres of it on motorways, its first step |
| steps | 1 | the roads a layer road is made of, in order, each road × 2 (+ 1 from b to a) |

A junction's arcs are the ways a car may leave it, up to the next
junction's first arc; a road's shape is the points between its a and its
b, up to the next road's first; a layer junction's roads and a layer
road's steps end the same way. The main roads and the layer are
described [below](#how-routing-works). The build's ID is a hash of all
the sections, so the same network always has the same ID.

LEHU still reads the versions before. Version 2 has a header of 20 words
(the number of speed cameras where version 3 keeps a word 0, and nothing
after the word kept 0 that follows the top speed of other roads), ends
with the speed cameras (6 words each, as in `cameras.bin`) after the
names, and has no layer, no main road marks and no build ID; its routes
are found over every road, as before. Version 1 has the first 18 words
only, no cameras and no motorway marks. A LEHU from before version 3
refuses version 3 (its log says "version 3, LEHU reads 1 to 2"), and its
map has no Go to.

`cameras.bin` is 6 words, "LEHUCAMS", the version (1), the number of
cameras and the ID of the `roads.bin` build they were placed on, then the
cameras, 6 words each: x, y (on the road), the road, decimetres from its
a, the ways it watches (1 a to b, 2 b to a, 3 both), its limit in km/h (0
not known), in the order of their roads and along each from a. A road is
a number in one build of the network, so a `cameras.bin` whose ID is not
the `roads.bin`'s is ignored, with a line in the log ("placed on roads.bin
..., not on this one"), as one that is broken is. The cameras change far
more often than the roads, and can be replaced alone, as long as they
are placed on the same build.

`places.bin` starts with 7 words: "LEHUFIND", the version (2), the
numbers of names and towns, the text's bytes and the number of points of
interest. Then the towns, 2 words each (the name and its folded key, byte
offsets into the text); the names, 5 words each (x, y, the name, its
folded key, and the town's index + the kind × 2^16 (0 a street, 1 a
place) + the rank × 2^24, lower more important); the points of interest,
5 words each (x, y, the name, a detail, and the town's index + the
category × 2^16: 1 fuel, 2 parking, 3 charging), in the order of their
categories; then the text, each string a length byte and UTF-8. Version
1 (still read) has the first 6 words and no points of interest. A key is
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
another name; into a roundabout, the exit to take.

**Across a country** that search reads most of the network: Budapest to
Debrecen settles 160,000 of Hungary's 620,000 junctions and reads 9 MB of
`roads.bin`, Sopron to Szeged without motorways 400,000 and 30 MB. On the
board (some tens of times slower than a PC, 64 MB of memory, the file on
the SD card) that would be many seconds. So the main roads (motorways,
trunk, primary and secondary roads and their links: in Hungary the roads
numbered with one to three digits) are also a smaller network of their
own, the layer: its junctions are where main roads meet (or the way they
may be driven changes), each of its roads the run of main road from one
such junction to the next, with what it costs in each measure (time,
length, and the part of both on motorways, so avoiding them is counted
exactly) and the roads it is made of. Hungary's has 18,500 junctions,
`roads.bin` 2 MB more.

The search runs on two levels at once. Near the start and near the goal
it takes every road, as before. Further from both it follows only main
roads, and from a layer junction it goes on along the layer, a whole run
of main road at a step; it comes down off the layer at a junction one of
whose runs can reach the goal's surroundings, and follows the main road
there. Near is 5 km (`NEAR_M` in `crates/lehu-map/src/graph.rs`), or as
far as the trip is long for a short one: up to 20 km for a trip of 20 km,
then back down to 5 km at 35 km, so a trip in town is searched over
every road. Each step's roads are the real ones, so the route is driven,
turned, timed and warned of cameras as before.

The route found is the best of those that leave the main roads only near
either end. That is not always the best of all: a back road in the middle
of a long trip is never taken. Between 300 pairs of junctions drawn at
random across Hungary (half of them near each other), the fastest was
the best in 237 and on average 0.3 % slower, at worst 12 %; the
shortest 1.1 % longer on average, at worst 20 % (the shortest likes
small roads); avoiding motorways 0.6 % dearer on average, at worst 23 %.
The worst are trips of 30 to 50 km. None can be better than the best of
all; the test `layer_against_plain` checks it. When there is no such
route at all (an end further than 5 km from any main road along small
ones), the search is made again with 20 km, then over every road
everywhere. A search keeps only what it reached, not a word for every
junction of the country.

| On a PC (Raspberry Pi 5) | every road everywhere | with the layer |
|---|---|---|
| Budapest to Debrecen, fastest (230 km) | 157,000 junctions, 28 ms, 9 MB read | 24,000, 8 ms, 3 MB |
| Sopron to Szeged, fastest (380 km) | 207,000, 41 ms, 12 MB | 16,000, 7 ms, 4 MB |
| Győr to Pécs, fastest (300 km) | 301,000, 53 ms, 15 MB | 18,000, 9 ms, 3.5 MB |
| Sopron to Szeged, no motorways (378 km) | 407,000, 78 ms, 30 MB | 19,000, 11 ms, 4.4 MB |
| Újpest to Kelenföld, in Budapest (15 km) | 29,000, 5 ms, 2 MB | 30,000, 10 ms, 2 MB (every road: the trip is short) |

The bytes are what is read of `roads.bin` from the card with nothing of
it in memory and no read-ahead, for the search, the line, the turns,
the limits and the cameras together; a third of it is the route's own
roads and shapes. The fastest routes above are the same either way; the
shortest 0.3 to 1.4 % longer, the ones without motorways up to 1.5 %
slower. On the board, tens of times slower and reading the card, a long
route should take one to two seconds; the test `country_routes` in
`crates/lehu-map/src/routing_tests.rs` measures it there (`LEHU_ROUTE_DIR`
the directory, `LEHU_DRIVES` other drives than Hungary's). In
Luxembourg the sample drive above (Gare to Kirchberg) takes about 0.5
ms, Schengen to Troisvierges (112 km) 3 to 4 ms; a search, 1 ms; the
nearest of a kind, a tenth of that; `routing_cost` times those.

Before the first route the whole of `roads.bin` is checked once, so that
nothing read from it later can be out of range: in the background when
the map opens, but for Hungary that is reading 69 MB from the card.

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
names, when the map has them. With Night set to Auto, night is when the
sun is down where the head unit is ([below](#night-by-the-sun)). The
night tiles are a second set in `night/` under the map's
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

## Night by the sun

With Night (`display.night-mode`) set to Auto, LEHU follows the sun:
night from when it is 2 degrees below the horizon, about ten minutes
after sunset in Luxembourg (a quarter of an hour in midsummer), when the
street lights come on and a bright screen starts to dazzle, until as long
before sunrise. Sunset itself is still broad daylight; civil twilight's
end, 6 degrees down, would be too late. There is half a degree to cross
each way (night at -2.5, day at -1.5), so it never flaps. The sun's place
is reckoned once a minute from the clock and where the head unit is: the
drive's position, the map's, `map.fake-position`, else the middle of the
map where it was left (a country's width is a few minutes of sunset).
While the clock is not set, or there is no position, Auto is day, as
before. The map's night tiles follow it, and so do LEHU's own screens,
which turn to their night palette (`Host::night_now`).

The phones do not tell the head unit day or night: CarPlay is asked to
darken by itself (it follows the phone's clock and place, so it agrees
with LEHU within minutes), and Android Auto is told day with Auto, as
before, while its session runs.

## Licence

Map data © OpenStreetMap contributors, under the
[Open Database License](https://www.openstreetmap.org/copyright). The map
screen shows the attribution.

The spoken prompts made with the `en_GB-cori-high` Piper voice are in the
public domain, as the voice is. Another voice comes with a licence of its
own, in its model card: some of Piper's voices are for non-commercial use
only.
