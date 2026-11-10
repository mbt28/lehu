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
| Back, or the bar's Back key | leave the map |

LEHU opens the map where it was left.

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
240 MB and five minutes on four cores.

The tiles are drawn by the script, never fetched from OpenStreetMap's tile
servers: their usage policy forbids offline use. The drawing is plain so
far: land use, water, buildings, roads and railways, with no labels.

## How it works

The map is shown on the video plane, beneath the screens, like the camera.
Tiles are decoded into memory and composed into a canvas a tile larger than
the panel on each side; the display engine shows a panel-sized window of
it. Scrolling moves the window, which costs one commit and no copy; when
the window would leave the canvas, the next canvas is composed around it.
Everything runs on a thread of its own, so the screens and the input never
wait for a decode. The phone's decoder makes way while the map is shown,
since both need video memory.

The F1C200s's video engine can decode JPEG in hardware; until that is
available, the tiles are decoded in software.

## Licence

Map data © OpenStreetMap contributors, under the
[Open Database License](https://www.openstreetmap.org/copyright). The map
screen shows the attribution.
