# The road data files

The layout of the three files `scripts/map-graph.py` writes into a
region's folder and LEHU maps from the card: `roads.bin`, `cameras.bin`
and `places.bin`. [map.md](map.md#routing-data) says what they are for.
All are little-endian 32-bit words, read in place.

## roads.bin

A header of 26 words: "LEHUROAD", the version (3), the numbers of
junctions, arcs, roads, shape points, road kinds, grid entries and name
bytes, the grid (its corner, its cell size as a shift of world pixels, its
columns and rows), the time a straight line takes at the top speed
(milliseconds per 65,536 world pixels), the top speed (km/h), a word kept
0, the top speed of the roads that are not motorways, a word kept 0, the
build's ID (two words, low first), the numbers of layer junctions, layer
roads and steps, and a word kept 0. Then, each right after the one before:

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
described in [map.md](map.md#how-routing-works). The build's ID is a hash
of all the sections, so the same network always has the same ID.

LEHU still reads the versions before. Version 2 has a header of 20 words
(the number of speed cameras where version 3 keeps a word 0, and nothing
after the word kept 0 that follows the top speed of other roads), ends
with the speed cameras (6 words each, as in `cameras.bin`) after the
names, and has no layer, no main road marks and no build ID; its routes
are found over every road. Version 1 has the first 18 words only, no
cameras and no motorway marks. A LEHU from before version 3 refuses
version 3 (its log says "version 3, LEHU reads 1 to 2"), and its map has
no Go to.

## cameras.bin

6 words: "LEHUCAMS", the version (1), the number of cameras and the ID of
the `roads.bin` build they were placed on; then the cameras, 6 words
each: x, y (on the road), the road, decimetres from its a, the ways it
watches (1 a to b, 2 b to a, 3 both), its limit in km/h (0 not known), in
the order of their roads and along each from a. A road is a number in one
build of the network, so a `cameras.bin` whose ID is not the `roads.bin`'s
is ignored, with a line in the log ("placed on roads.bin ..., not on this
one"), as one that is broken is.

## places.bin

9 words: "LEHUFIND", the version (3), the numbers of names and towns, the
text's bytes, the number of points of interest, of houses and of words.
Then the towns, 2 words each (the name and its folded key, byte offsets
into the text); the names, 5 words each (x, y, the name, its folded key,
and the town's index + the kind × 2^16 (0 a street, 1 a place) + the rank
× 2^24, lower more important); the points of interest, 5 words each (x, y,
the name, a detail, and the town's index + the category × 2^16: 1 fuel, 2
parking, 3 charging), in the order of their categories; the words of the
names' keys, 2 words each (the word and its name), sorted by the word's
bytes; where each town's names begin (towns + 1 words) and the names'
indices, town by town; where each street's houses begin (names + 1 words)
and the houses, 4 words each (x, y, the number as written and the
postcode); then the text, each string a length byte and UTF-8. Version 2
(still read) has the first 7 words and stops after the points of
interest, version 1 the first 6 and no points of interest; with either, a
search reads every name. A key is the name folded: lower case, without
accents, its words split by single spaces. A street is in the city, town
or village nearest it (a city counts from three times as far, a town
twice); it has one name for each town it runs through, at the middle of
its longest piece.
