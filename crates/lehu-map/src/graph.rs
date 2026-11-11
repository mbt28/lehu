// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The road network, for finding routes on the board: junctions, the roads
//! between them, and a grid to find the road nearest a point. It is written
//! on a PC by scripts/map-graph.py as `roads.bin` and mapped from the card
//! here, never loaded: a country's network is a few megabytes, and a route
//! looks at only part of it.
//!
//! The file is little-endian 32-bit words. After a header of 26 words (20
//! in version 2, 18 in version 1) come these sections, each right after
//! the one before:
//!
//! | Section | Words each | What |
//! |---|---|---|
//! | nodes + 1 | 4 | x, y (world pixels at zoom 22), its first arc, how many roads meet there (low 8 bits) + its place in the layer + 1 << 8 (0 not in it) |
//! | arcs | 3 | the node it leads to, milliseconds, edge * 2 (+ 1 when it runs the edge from b to a) + 1 << 31 a main road |
//! | edges + 1 | 5 | a, b, decimetres, kind (low 24 bits) + 1 << 24 a to b allowed + 1 << 25 b to a allowed + 1 << 26 a motorway (or its link), its first shape point |
//! | shape points | 1 | x, then all the y |
//! | kinds | 2 | the road's name (a byte offset into the names), speed limit km/h (0 none) + 1 << 16 roundabout |
//! | cells + 1 | 1 | the grid: each cell's first entry in the next section |
//! | entries | 1 | the edges with a piece in the cell |
//! | names | bytes | each a length byte, then UTF-8 |
//! | layer junctions + 1 | 4 | its node, x, y, its first layer road |
//! | layer roads + 1 | 6 | the layer junction it leads to, milliseconds, decimetres, the milliseconds and the decimetres of it on motorways, its first step |
//! | steps | 1 | the edges a layer road is made of, in order, as an arc's (edge * 2, + 1 from b to a) |
//!
//! The header is "LEHUROAD", the version (3), the counts of nodes, arcs,
//! edges, shape points, kinds, grid entries and name bytes, the grid (its
//! corner x and y, the cell's size as a shift, its columns and rows), the
//! search's speed bound (milliseconds per 65536 world pixels, the fewest
//! a straight line can take anywhere on the network), the top speed in
//! km/h, a word kept 0 (the count of speed cameras in version 2), the top
//! speed of the roads that are not motorways, a word kept 0, the build's
//! ID (two words, low first: what `cameras.bin` is matched by), the counts
//! of layer junctions, layer roads and steps, and a word kept 0.
//!
//! Version 2, which LEHU still reads, ends after the names with the speed
//! cameras (6 words each, as `cameras.bin` has them, crate::cameras), has
//! no layer and no build ID, and its nodes' last word is only the count of
//! roads. Version 1 stops after the top speed: it has no cameras and no
//! motorway marks, so with it routes cannot avoid motorways. A node's arcs
//! are the ones a car may take from it, its last one just before the next
//! node's first (the extra node at the end closes the list); an edge's
//! shape is the points between a and b, up to the next edge's first; a
//! layer junction's roads and a layer road's steps end in the same way.
//!
//! Finding a route is A* from the point nearest the start to the point
//! nearest the goal, both on a road, in integer arithmetic only: by time
//! (the fastest), the lower bound on the time left being the straight line
//! at the network's top speed; or by distance (the shortest), the bound
//! the straight line itself. Avoiding motorways makes each motorway cost
//! five times its time or length, so a route still goes on one where there
//! is no other way; the bound is then the straight line at the speed of
//! the fastest other road (or a fifth of the motorways', when that is
//! more), which the costs never beat.
//!
//! **Across a country** the search would read most of the network. The
//! main roads (motorways, trunk, primary and secondary roads, with their
//! links) are therefore also a network of their own, the layer: its
//! junctions are where main roads meet, each of its roads the run of main
//! road from one junction to the next, with its cost in each measure and
//! the edges it is made of. The search runs on two levels at once: near
//! the start or the goal on every road, as before; further out only along
//! main roads, and from a layer junction on the layer, a whole run of main
//! road at a step. It comes down off the layer at a junction whose runs
//! can reach the goal's surroundings. Near is [`NEAR_M`], or as far as
//! the trip is long for a short one (up to 4 * NEAR_M, back down to
//! NEAR_M at 7 * NEAR_M), so a short trip is searched over every road.
//! The route found is the best of those that leave the main roads only
//! near either end: on most trips the best of all, on average less than
//! a percent dearer, and a few thousand junctions settled instead of
//! hundreds of thousands. When there is no such route (an end further
//! from any main road), the search is made with four times NEAR_M, then
//! over every road everywhere.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;

use crate::blob::{Blob, short_str};
use crate::cameras::CAMERA;
use crate::track::{metres_between, metres_per_px};

/// The file's name in the map's directory.
pub const ROADS_FILE: &str = "roads.bin";

const MAGIC: &[u8; 8] = b"LEHUROAD";
const VERSION: u32 = 3;
const HEADER: usize = 26;
/// Version 2's header: no layer, no build ID; the cameras inside.
const HEADER_V2: usize = 20;
/// Version 1's header: no cameras, no motorway marks.
const HEADER_V1: usize = 18;
const NODE: usize = 4;
const ARC: usize = 3;
const EDGE: usize = 5;
const KIND: usize = 2;
const LAYER_NODE: usize = 4;
const LAYER_ARC: usize = 6;

/// A node's last word: how many roads meet there in these bits, its place
/// in the layer + 1 above them.
const DEGREE_BITS: u32 = 8;
/// An arc's last word: the edge * 2 (+ 1 from b to a) in these bits, and
/// this one for a main road.
const ARC_EDGE: u32 = 0x7FFF_FFFF;
const ARC_MAIN: u32 = 1 << 31;

/// Within this many metres of the start or the goal a route may take any
/// road; further from both, only main roads.
pub const NEAR_M: u32 = 5_000;

/// An edge's kind word: may be driven a to b, b to a; a motorway.
const FORWARD: u32 = 1 << 24;
const BACKWARD: u32 = 1 << 25;
const MOTORWAY: u32 = 1 << 26;

/// Avoiding motorways, a motorway costs this many times its time or
/// length: dear enough that a route takes another road wherever one goes
/// the same way within several times the time, never forbidden.
const MOTORWAY_PENALTY: u32 = 5;

/// Off by more than this from any road, a point is not on the network.
const SNAP_MAX_PX: i64 = 1 << 18;

/// Starting against the way we are heading costs this much: a U-turn,
/// half a minute when the route is the fastest, 400 m (what half a minute
/// covers in town) when it is the shortest.
const U_TURN_MS: u32 = 30_000;
const U_TURN_DM: u32 = 4_000;

/// What a route is the best of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RouteBy {
    /// The least time, at the roads' speeds.
    #[default]
    Fastest,
    /// The least distance.
    Shortest,
}

/// How a route is chosen: by time or by distance, and whether motorways
/// are kept off (made dear, not forbidden).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouteOptions {
    pub by: RouteBy,
    pub avoid_motorways: bool,
}

/// Parent marks: reached from the start's point, along its edge towards
/// b, or towards a; from the same junction on the other level, onto the
/// layer or down off it.
const FROM_START_B: u32 = u32::MAX;
const FROM_START_A: u32 = u32::MAX - 1;
const LIFT: u32 = u32::MAX - 2;
const DROP: u32 = u32::MAX - 3;

/// A search's state: a node, or (with this bit) a layer junction.
const ON_LAYER: u32 = 1 << 31;

/// The road network, read in place.
pub struct Graph {
    blob: Blob,
    nodes: usize,
    arcs: usize,
    edges: usize,
    shapes: usize,
    kinds: usize,
    entries: usize,
    names_len: usize,
    grid: Grid,
    /// Milliseconds per 65536 world pixels of straight line, at least.
    h_q16: u32,
    top_kmh: u32,
    /// The fastest road's speed that is not a motorway, km/h.
    top_other_kmh: u32,
    version: u32,
    /// Version 2's speed cameras, inside the file.
    cameras: usize,
    build_id: Option<u64>,
    layer_nodes: usize,
    layer_arcs: usize,
    steps: usize,
    /// Within this of either end any road; 0: every road everywhere.
    near_m: u32,
    // Where each section starts, in words.
    node_at: usize,
    arc_at: usize,
    edge_at: usize,
    shape_x_at: usize,
    shape_y_at: usize,
    kind_at: usize,
    cell_at: usize,
    entry_at: usize,
    name_at: usize,
    camera_at: usize,
    layer_node_at: usize,
    layer_arc_at: usize,
    step_at: usize,
}

#[derive(Clone, Copy, Debug)]
struct Grid {
    x0: i64,
    y0: i64,
    shift: u32,
    cols: i64,
    rows: i64,
}

/// A point on a road: the nearest to where someone is or wants to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snap {
    pub edge: u32,
    /// Where on the road, world pixels at zoom 22.
    pub x: i64,
    pub y: i64,
    /// The piece of the edge's line it is on: 0 from a to its first shape
    /// point (or b).
    pub seg: u32,
    /// Decimetres from the edge's a.
    pub from_a_dm: u32,
}

/// A route the search found: from the start's point, the edges in order
/// and the way each is driven, to the goal's point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoadPath {
    pub start: Snap,
    pub end: Snap,
    /// The start's edge is driven towards b (else towards a).
    pub start_forward: bool,
    /// The goal is on the start's edge, ahead: straight there.
    pub direct: bool,
    /// The edges between, each (edge, driven from b to a); none when
    /// `direct`, or when the goal's edge leaves the start's.
    pub arcs: Vec<(u32, bool)>,
    /// The goal's edge is entered at a (else at b); when `arcs` is empty,
    /// as `start_forward`.
    pub end_forward: bool,
    /// What it costs: milliseconds at the roads' speeds for the fastest,
    /// decimetres for the shortest, a motorway's counted five times over
    /// when they are avoided; and a U-turn at the start.
    pub cost: u64,
    /// The nodes (and layer junctions) the search took from its queue.
    pub settled: u32,
}

/// One edge as the file has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub a: u32,
    pub b: u32,
    pub dm: u32,
    pub kind: u32,
    pub forward: bool,
    pub backward: bool,
    /// A motorway or a motorway's link (always false in version 1).
    pub motorway: bool,
    shape: (u32, u32),
}

/// A road's name, its speed limit (km/h) and whether it is a roundabout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kind<'a> {
    pub name: &'a str,
    pub limit: Option<u16>,
    pub roundabout: bool,
}

/// What a search keeps between routes: a cost and a parent for each state
/// it reached, and the queue. Only what a search reaches is kept, not a
/// word for every node of a country.
#[derive(Default)]
pub struct Work {
    reached: HashMap<u32, (u32, u32), BuildHasherDefault<StateHash>>,
    heap: BinaryHeap<Reverse<u64>>,
}

/// After a search this large, the memory goes back.
const WORK_KEEP: usize = 1 << 16;

/// A state's hash: its number spread by a multiplication (the states are
/// small integers; no attacker chooses them).
#[derive(Default)]
struct StateHash(u64);

impl Hasher for StateHash {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u32(&mut self, v: u32) {
        self.write_u64(u64::from(v));
    }

    fn write_u64(&mut self, v: u64) {
        let h = (self.0 ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 29);
    }
}

impl Graph {
    /// The file in `dir`, mapped.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let path = dir.join(ROADS_FILE);
        Self::new(Blob::map(&path)?).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// A file's bytes (tests).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        Self::new(Blob::from_bytes(bytes)?)
    }

    fn new(blob: Blob) -> Result<Self, String> {
        let w = blob.words();
        if w.len() < HEADER_V1 || blob.bytes(0, 8) != Some(&MAGIC[..]) {
            return Err("not a LEHU road network".into());
        }
        let version = w[2];
        if !(1..=VERSION).contains(&version) {
            return Err(format!("version {version}, LEHU reads 1 to {VERSION}"));
        }
        let header = match version {
            1 => HEADER_V1,
            2 => HEADER_V2,
            _ => HEADER,
        };
        if w.len() < header {
            return Err("cut short".into());
        }
        let n = |i: usize| w[i] as usize;
        let (nodes, arcs, edges, shapes, kinds, entries, names_len) =
            (n(3), n(4), n(5), n(6), n(7), n(8), n(9));
        let grid = Grid {
            x0: i64::from(w[10]),
            y0: i64::from(w[11]),
            shift: w[12],
            cols: i64::from(w[13]),
            rows: i64::from(w[14]),
        };
        if grid.shift > 30 || grid.cols == 0 || grid.rows == 0 {
            return Err("a broken grid".into());
        }
        let cells = (grid.cols * grid.rows) as usize;
        let (cameras, top_other_kmh) = match version {
            1 => (0, w[16]),
            2 => (n(17), w[18]),
            _ => (0, w[18]),
        };
        let (build_id, layer_nodes, layer_arcs, steps) = if version >= 3 {
            (
                Some(u64::from(w[20]) | u64::from(w[21]) << 32),
                n(22),
                n(23),
                n(24),
            )
        } else {
            (None, 0, 0, 0)
        };
        let node_at = header;
        let arc_at = node_at + (nodes + 1) * NODE;
        let edge_at = arc_at + arcs * ARC;
        let shape_x_at = edge_at + (edges + 1) * EDGE;
        let shape_y_at = shape_x_at + shapes;
        let kind_at = shape_y_at + shapes;
        let cell_at = kind_at + kinds * KIND;
        let entry_at = cell_at + cells + 1;
        let name_at = entry_at + entries;
        let camera_at = name_at + names_len.div_ceil(4);
        let layer_node_at = camera_at + cameras * CAMERA;
        let (layer_arc_at, step_at, end) = if version >= 3 {
            let layer_arc_at = layer_node_at + (layer_nodes + 1) * LAYER_NODE;
            let step_at = layer_arc_at + (layer_arcs + 1) * LAYER_ARC;
            (layer_arc_at, step_at, step_at + steps)
        } else {
            (layer_node_at, layer_node_at, layer_node_at)
        };
        if end != w.len() {
            return Err("its sections and its length disagree".into());
        }
        Ok(Self {
            nodes,
            arcs,
            edges,
            shapes,
            kinds,
            entries,
            names_len,
            grid,
            h_q16: w[15],
            top_kmh: w[16],
            top_other_kmh,
            version,
            cameras,
            build_id,
            layer_nodes,
            layer_arcs,
            steps,
            near_m: if layer_nodes > 0 { NEAR_M } else { 0 },
            node_at,
            arc_at,
            edge_at,
            shape_x_at,
            shape_y_at,
            kind_at,
            cell_at,
            entry_at,
            name_at,
            camera_at,
            layer_node_at,
            layer_arc_at,
            step_at,
            blob,
        })
    }

    /// The search with any road within `m` metres of either end, main
    /// roads beyond (0: every road everywhere, the plain search).
    pub fn with_near(mut self, m: u32) -> Self {
        self.near_m = if self.layer_nodes > 0 { m } else { 0 };
        self
    }

    /// Every index in the file points where it should, so nothing read
    /// from it later can be out of range. A pass over the whole file: done
    /// once, before the first route.
    pub fn check(&self) -> Result<(), String> {
        let w = self.blob.words();
        let nodes = &w[self.node_at..self.arc_at];
        let mut last = 0;
        for i in 0..=self.nodes {
            let first = nodes[i * NODE + 2] as usize;
            if first < last || first > self.arcs {
                return Err(format!("node {i}: its arcs are out of order"));
            }
            last = first;
            if nodes[i * NODE] >= 1 << 30 || nodes[i * NODE + 1] >= 1 << 30 {
                return Err(format!("node {i}: off the map"));
            }
            if i < self.nodes
                && let Some(l) = self.layer_of(i as u32)
                && (l as usize >= self.layer_nodes
                    || w[self.layer_node_at + l as usize * LAYER_NODE] != i as u32)
            {
                return Err(format!("node {i}: its layer junction is not its own"));
            }
        }
        if last != self.arcs {
            return Err("the arcs do not end with the nodes".into());
        }
        for (i, arc) in w[self.arc_at..self.edge_at]
            .as_chunks::<ARC>()
            .0
            .iter()
            .enumerate()
        {
            if arc[0] as usize >= self.nodes || ((arc[2] & ARC_EDGE) >> 1) as usize >= self.edges {
                return Err(format!("arc {i}: out of range"));
            }
        }
        let mut last = 0;
        for (i, e) in w[self.edge_at..self.shape_x_at]
            .as_chunks::<EDGE>()
            .0
            .iter()
            .enumerate()
        {
            let first = e[4] as usize;
            if first < last || first > self.shapes {
                return Err(format!("edge {i}: its shape is out of order"));
            }
            last = first;
            if i < self.edges
                && (e[0] as usize >= self.nodes
                    || e[1] as usize >= self.nodes
                    || (e[3] & 0xFF_FFFF) as usize >= self.kinds)
            {
                return Err(format!("edge {i}: out of range"));
            }
        }
        if w[self.shape_x_at..self.kind_at]
            .iter()
            .any(|&v| v >= 1 << 30)
        {
            return Err("a shape point off the map".into());
        }
        for (i, k) in w[self.kind_at..self.cell_at]
            .as_chunks::<KIND>()
            .0
            .iter()
            .enumerate()
        {
            if self.names_len > 0 && k[0] as usize >= self.names_len {
                return Err(format!("kind {i}: its name is out of range"));
            }
        }
        let mut last = 0;
        for &first in &w[self.cell_at..self.entry_at] {
            if (first as usize) < last || first as usize > self.entries {
                return Err("the grid is out of order".into());
            }
            last = first as usize;
        }
        if w[self.entry_at..self.name_at]
            .iter()
            .any(|&e| e as usize >= self.edges)
        {
            return Err("the grid names an edge out of range".into());
        }
        self.check_layer()
    }

    /// The layer's junctions are nodes, each of its roads leads to one and
    /// is a run of edges, each leaving where the one before arrived, from
    /// its own junction.
    fn check_layer(&self) -> Result<(), String> {
        if self.version < 3 {
            return Ok(());
        }
        let w = self.words();
        let mut last = 0;
        for i in 0..=self.layer_nodes {
            let at = self.layer_node_at + i * LAYER_NODE;
            let first = w[at + 3] as usize;
            if first < last || first > self.layer_arcs {
                return Err(format!("layer junction {i}: its roads are out of order"));
            }
            last = first;
            if i < self.layer_nodes {
                let n = w[at];
                if n as usize >= self.nodes
                    || self.layer_of(n) != Some(i as u32)
                    || (i64::from(w[at + 1]), i64::from(w[at + 2])) != self.node(n)
                {
                    return Err(format!("layer junction {i}: not its node's"));
                }
            }
        }
        if last != self.layer_arcs {
            return Err("the layer's roads do not end with its junctions".into());
        }
        let mut from = 0;
        let mut last = 0;
        for i in 0..=self.layer_arcs {
            let at = self.layer_arc_at + i * LAYER_ARC;
            let first = w[at + 5] as usize;
            if first < last || first > self.steps || (i == self.layer_arcs && first != self.steps) {
                return Err(format!("layer road {i}: its steps are out of order"));
            }
            last = first;
            if i == self.layer_arcs {
                break;
            }
            while w[self.layer_node_at + (from + 1) * LAYER_NODE + 3] as usize <= i {
                from += 1;
            }
            let end = w[at + LAYER_ARC + 5] as usize;
            let to = w[at] as usize;
            if to >= self.layer_nodes || end <= first {
                return Err(format!("layer road {i}: out of range"));
            }
            // Its steps, joined end to end, from its junction to the next.
            let mut n = w[self.layer_node_at + from * LAYER_NODE];
            for &code in &w[self.step_at + first..self.step_at + end] {
                let e = code >> 1;
                if code & !ARC_EDGE != 0 || e as usize >= self.edges {
                    return Err(format!("layer road {i}: a step out of range"));
                }
                let edge = self.edge(e);
                let (a, b) = if code & 1 == 1 {
                    (edge.b, edge.a)
                } else {
                    (edge.a, edge.b)
                };
                if a != n {
                    return Err(format!("layer road {i}: its steps do not join"));
                }
                n = b;
            }
            if n != w[self.layer_node_at + to * LAYER_NODE] {
                return Err(format!("layer road {i}: it does not end at its junction"));
            }
        }
        Ok(())
    }

    /// The build's ID, which `cameras.bin` names (version 3 on).
    pub fn build_id(&self) -> Option<u64> {
        self.build_id
    }

    /// The speed cameras a version 2 file has inside, as their words.
    pub(crate) fn v2_cameras(&self) -> Option<Vec<u32>> {
        (self.version == 2)
            .then(|| self.words()[self.camera_at..self.camera_at + self.cameras * CAMERA].to_vec())
    }

    /// The layer junction a node is, when it is one.
    fn layer_of(&self, n: u32) -> Option<u32> {
        if self.version < 3 {
            return None;
        }
        let l = self.words()[self.node_at + n as usize * NODE + 3] >> DEGREE_BITS;
        l.checked_sub(1)
    }

    /// How many junctions the layer has (0: none, version 2 and before).
    pub fn layer_junctions(&self) -> usize {
        self.layer_nodes
    }

    pub fn nodes(&self) -> usize {
        self.nodes
    }

    pub fn edges(&self) -> usize {
        self.edges
    }

    /// The fastest road's speed, km/h.
    pub fn top_kmh(&self) -> u32 {
        self.top_kmh
    }

    fn words(&self) -> &[u32] {
        self.blob.words()
    }

    #[cfg(test)]
    pub(crate) fn blob(&self) -> &Blob {
        &self.blob
    }

    /// A node's point, world pixels.
    pub fn node(&self, n: u32) -> (i64, i64) {
        let at = self.node_at + n as usize * NODE;
        let w = self.words();
        (i64::from(w[at]), i64::from(w[at + 1]))
    }

    /// How many roads meet at a node, whichever way they may be driven.
    pub fn degree(&self, n: u32) -> u32 {
        let d = self.words()[self.node_at + n as usize * NODE + 3];
        if self.version >= 3 {
            d & ((1 << DEGREE_BITS) - 1)
        } else {
            d
        }
    }

    /// The arcs a car may take from a node: (the node it leads to, edge,
    /// driven from b to a).
    pub fn out(&self, n: u32) -> impl Iterator<Item = (u32, u32, bool)> + '_ {
        let w = self.words();
        let at = self.node_at + n as usize * NODE;
        let (first, end) = (w[at + 2] as usize, w[at + NODE + 2] as usize);
        w[self.arc_at + first * ARC..self.arc_at + end * ARC]
            .as_chunks::<ARC>()
            .0
            .iter()
            .map(|a| (a[0], (a[2] & ARC_EDGE) >> 1, a[2] & 1 == 1))
    }

    pub fn edge(&self, e: u32) -> Edge {
        let at = self.edge_at + e as usize * EDGE;
        let w = &self.words()[at..at + 2 * EDGE];
        Edge {
            a: w[0],
            b: w[1],
            dm: w[2].max(1),
            kind: w[3] & 0xFF_FFFF,
            forward: w[3] & FORWARD != 0,
            backward: w[3] & BACKWARD != 0,
            motorway: w[3] & MOTORWAY != 0,
            shape: (w[4], w[EDGE + 4]),
        }
    }

    /// An edge's travel time, milliseconds: its arcs carry it.
    pub fn edge_ms(&self, e: u32) -> u32 {
        let edge = self.edge(e);
        let (from, back) = if edge.forward {
            (edge.a, false)
        } else {
            (edge.b, true)
        };
        let w = self.words();
        let at = self.node_at + from as usize * NODE;
        let (first, end) = (w[at + 2] as usize, w[at + NODE + 2] as usize);
        w[self.arc_at + first * ARC..self.arc_at + end * ARC]
            .as_chunks::<ARC>()
            .0
            .iter()
            .find(|a| a[2] & ARC_EDGE == e * 2 + u32::from(back))
            .map_or(1, |a| a[1].max(1))
    }

    pub fn kind(&self, k: u32) -> Kind<'_> {
        let at = self.kind_at + k as usize * KIND;
        let w = self.words();
        let names = self.blob.bytes(self.name_at, self.names_len).unwrap_or(&[]);
        let limit = (w[at + 1] & 0xFFFF) as u16;
        Kind {
            name: short_str(names, w[at]),
            limit: (limit > 0).then_some(limit),
            roundabout: w[at + 1] & (1 << 16) != 0,
        }
    }

    /// The edge's line, from a to b: a, its shape points, b.
    pub fn line(&self, e: u32) -> Vec<(i64, i64)> {
        let edge = self.edge(e);
        let w = self.words();
        let mut out = Vec::with_capacity((edge.shape.1 - edge.shape.0) as usize + 2);
        out.push(self.node(edge.a));
        for i in edge.shape.0 as usize..edge.shape.1 as usize {
            out.push((
                i64::from(w[self.shape_x_at + i]),
                i64::from(w[self.shape_y_at + i]),
            ));
        }
        out.push(self.node(edge.b));
        out
    }

    /// The point on a road nearest `(x, y)`, within about 6 km; None
    /// further away.
    pub fn snap(&self, x: i64, y: i64) -> Option<Snap> {
        let g = self.grid;
        let w = self.words();
        let (cx, cy) = ((x - g.x0) >> g.shift, (y - g.y0) >> g.shift);
        let cell = 1i64 << g.shift;
        let mut best: Option<(i64, u32, u32, i64, i64)> = None;
        let rings = (SNAP_MAX_PX >> g.shift).max(1);
        for r in 0..=rings {
            for gy in cy - r..=cy + r {
                for gx in cx - r..=cx + r {
                    if (gx - cx).abs().max((gy - cy).abs()) != r
                        || !(0..g.cols).contains(&gx)
                        || !(0..g.rows).contains(&gy)
                    {
                        continue;
                    }
                    let c = (gy * g.cols + gx) as usize;
                    let (from, to) = (
                        w[self.cell_at + c] as usize,
                        w[self.cell_at + c + 1] as usize,
                    );
                    for &e in &w[self.entry_at + from..self.entry_at + to] {
                        let line = self.line(e);
                        for (s, pair) in line.windows(2).enumerate() {
                            let (px, py) = nearest_on(pair[0], pair[1], (x, y));
                            let d = (px - x) * (px - x) + (py - y) * (py - y);
                            if best.is_none_or(|b| d < b.0) {
                                best = Some((d, e, s as u32, px, py));
                            }
                        }
                    }
                }
            }
            // A cell further out is at least r cells away.
            if let Some(b) = best
                && b.0 <= (r * cell) * (r * cell)
            {
                break;
            }
        }
        let (d, edge, seg, px, py) = best?;
        if d > SNAP_MAX_PX * SNAP_MAX_PX {
            return None;
        }
        // How far along the edge, by the share of its line's length.
        let line = self.line(edge);
        let len = |a: (i64, i64), b: (i64, i64)| ((b.0 - a.0) as f64).hypot((b.1 - a.1) as f64);
        let total: f64 = line.windows(2).map(|p| len(p[0], p[1])).sum();
        let before: f64 = line[..=seg as usize]
            .windows(2)
            .map(|p| len(p[0], p[1]))
            .sum::<f64>()
            + len(line[seg as usize], (px, py));
        let dm = self.edge(edge).dm;
        let from_a_dm = if total > 0.0 {
            ((before / total) * f64::from(dm)).round() as u32
        } else {
            0
        };
        Some(Snap {
            edge,
            x: px,
            y: py,
            seg,
            from_a_dm: from_a_dm.min(dm),
        })
    }

    /// What driving edge `e` whole costs, as `options` count it.
    fn edge_cost(&self, e: u32, options: &RouteOptions) -> u32 {
        let edge = self.edge(e);
        let base = match options.by {
            RouteBy::Fastest => self.edge_ms(e),
            RouteBy::Shortest => edge.dm,
        };
        if options.avoid_motorways && edge.motorway {
            base.saturating_mul(MOTORWAY_PENALTY)
        } else {
            base
        }
    }

    /// The bound on what a straight line of 65536 world pixels costs, as
    /// `options` count it: never more than any road there does.
    fn bound_q16(&self, options: &RouteOptions) -> u64 {
        let h = u64::from(self.h_q16);
        let top = u64::from(self.top_kmh.max(1));
        match options.by {
            // h_q16 is the time at the top speed: ms = dm * 360 / km/h.
            RouteBy::Shortest => h * top / 360,
            RouteBy::Fastest if options.avoid_motorways => {
                // The fastest a route can go: the best other road, or a
                // motorway at a fifth of its speed.
                let other = u64::from(self.top_other_kmh.min(self.top_kmh));
                let v = other.max(top.div_ceil(u64::from(MOTORWAY_PENALTY))).max(1);
                h * top / v
            }
            RouteBy::Fastest => h,
        }
    }

    /// The fastest way from `start` to `end`: A* by time. `heading` (x
    /// right, y down), when we are moving, makes setting off backwards
    /// cost a U-turn. None when the goal cannot be reached.
    pub fn route(
        &self,
        start: &Snap,
        heading: Option<(i32, i32)>,
        end: &Snap,
        work: &mut Work,
    ) -> Option<RoadPath> {
        self.route_with(start, heading, end, &RouteOptions::default(), work)
    }

    /// The best way from `start` to `end` as `options` choose it: A* by
    /// time or by distance, motorways made dear or not; far from both ends
    /// over the main roads' layer, when the file has one (see the module's
    /// notes), else, or when that finds nothing, over every road. As
    /// [`Self::route`] otherwise.
    pub fn route_with(
        &self,
        start: &Snap,
        heading: Option<(i32, i32)>,
        end: &Snap,
        options: &RouteOptions,
        work: &mut Work,
    ) -> Option<RoadPath> {
        // Nothing found (an end further than near_m from any main road):
        // four times as far, then every road everywhere.
        let found = if self.near_m > 0 {
            self.search(start, heading, end, options, self.near_m, work)
                .or_else(|| {
                    log::info!("map: no route over the main roads; nearer them");
                    let wider = self.near_m.saturating_mul(4);
                    self.search(start, heading, end, options, wider, work)
                })
                .or_else(|| {
                    log::info!("map: no route over the main roads; over every road");
                    self.search(start, heading, end, options, 0, work)
                })
        } else {
            self.search(start, heading, end, options, 0, work)
        };
        // A plain search across a country leaves a large table: its memory
        // goes back.
        if work.reached.capacity() > WORK_KEEP {
            work.reached = HashMap::default();
            work.heap = BinaryHeap::new();
        }
        found
    }

    /// The search itself: `near_m` 0 over every road, else on two levels
    /// (any road within `near_m` metres of either end, the layer between).
    fn search(
        &self,
        start: &Snap,
        heading: Option<(i32, i32)>,
        end: &Snap,
        options: &RouteOptions,
        near_m: u32,
        work: &mut Work,
    ) -> Option<RoadPath> {
        let w = self.words();
        work.reached.clear();
        work.heap.clear();

        let se = self.edge(start.edge);
        let sms = self.edge_cost(start.edge, options);
        let along = |edge_ms: u32, dm: u32, part: u32| {
            (u64::from(edge_ms) * u64::from(part) / u64::from(dm.max(1))) as u32
        };
        let s_to_a = along(sms, se.dm, start.from_a_dm);
        let s_to_b = sms.saturating_sub(s_to_a);
        // Setting off against the heading: a U-turn first.
        let line = self.line(start.edge);
        let (behind, ahead) = (line[start.seg as usize], line[start.seg as usize + 1]);
        let dot = heading.map_or(0, |(hx, hy)| {
            (ahead.0 - behind.0) * i64::from(hx) + (ahead.1 - behind.1) * i64::from(hy)
        });
        let u_turn = match options.by {
            RouteBy::Fastest => U_TURN_MS,
            RouteBy::Shortest => U_TURN_DM,
        };
        let u_fwd = if dot < 0 { u_turn } else { 0 };
        let u_bwd = if dot > 0 { u_turn } else { 0 };

        let ee = self.edge(end.edge);
        let ems = self.edge_cost(end.edge, options);
        let a_to_e = along(ems, ee.dm, end.from_a_dm);
        let b_to_e = ems.saturating_sub(a_to_e);

        // The goal straight along the start's edge.
        let mut best: Option<(u64, Option<u32>, bool)> = None;
        if start.edge == end.edge {
            if se.forward && end.from_a_dm >= start.from_a_dm {
                let ms = along(sms, se.dm, end.from_a_dm - start.from_a_dm) + u_fwd;
                best = Some((u64::from(ms), None, true));
            }
            if se.backward && end.from_a_dm <= start.from_a_dm {
                let ms = u64::from(along(sms, se.dm, start.from_a_dm - end.from_a_dm) + u_bwd);
                if best.is_none_or(|b| ms < b.0) {
                    best = Some((ms, None, false));
                }
            }
        }

        let (tx, ty) = (end.x, end.y);
        let h_q16 = self.bound_q16(options);
        let h = |x: i64, y: i64| -> u32 {
            let dx = (x - tx).unsigned_abs();
            let dy = (y - ty).unsigned_abs();
            // A lower bound on the straight line, with no root: the
            // longer side, or the two together over a little more than
            // the square root of two.
            let d = dx.max(dy).max(((dx + dy) * 181) >> 8);
            ((d * h_q16) >> 16).min(u64::from(u32::MAX >> 1)) as u32
        };
        // Near either end: within near_m, in world pixels there.
        let layered = near_m > 0;
        let px = |m: u32, y: i64| (f64::from(m) / metres_per_px(y)) as u64;
        // A shorter trip is searched over every road further out, so that
        // up to 4 * near_m all of it is: near either end is as far as the
        // trip is long (at least near_m), and from there back down to
        // near_m at 7 * near_m.
        let trip_m = metres_between((start.x, start.y), (tx, ty));
        let near_m = near_m.max(trip_m.min((8 * near_m).saturating_sub(trip_m)));
        let (near_s, near_t) = (px(near_m, start.y), px(near_m, ty));
        let dist2 = |x: i64, y: i64, (px_, py_): (i64, i64)| {
            let (dx, dy) = ((x - px_).unsigned_abs(), (y - py_).unsigned_abs());
            dx * dx + dy * dy
        };
        let near = |x: i64, y: i64| {
            !layered
                || dist2(x, y, (start.x, start.y)) <= near_s * near_s
                || dist2(x, y, (tx, ty)) <= near_t * near_t
        };
        // A layer road of `dm` reaches this many world pixels at most (an
        // eighth more, for the scale changing along it).
        let reach = |dm: u32| px(dm / 10, ty) * 9 / 8;
        // A layer junction's roads.
        let roads = |l: u32| {
            let at = self.layer_node_at + l as usize * LAYER_NODE;
            w[at + 3] as usize..w[at + LAYER_NODE + 3] as usize
        };
        // The search comes down off the layer at a junction (at x, y) when
        // one of its roads can reach the goal's surroundings.
        let comes_down = |l: u32, x: i64, y: i64| {
            let furthest = roads(l)
                .map(|k| w[self.layer_arc_at + k * LAYER_ARC + 2])
                .max()
                .unwrap_or(0);
            let r = near_t + reach(furthest);
            dist2(x, y, (tx, ty)) <= r * r
        };
        let push = |work: &mut Work, state: u32, g: u32, parent: u32, (x, y): (i64, i64)| {
            let slot = work.reached.entry(state).or_insert((u32::MAX, parent));
            if g < slot.0 {
                *slot = (g, parent);
                let f = u64::from(g.saturating_add(h(x, y)));
                work.heap.push(Reverse((f << 32) | u64::from(state)));
            }
        };
        let node_xy = |n: u32| {
            let at = self.node_at + n as usize * NODE;
            (i64::from(w[at]), i64::from(w[at + 1]))
        };
        let junction = |l: u32| {
            let at = self.layer_node_at + l as usize * LAYER_NODE;
            (w[at], (i64::from(w[at + 1]), i64::from(w[at + 2])))
        };
        if se.forward {
            push(work, se.b, s_to_b + u_fwd, FROM_START_B, node_xy(se.b));
        }
        if se.backward {
            // Two starts at one node (a loop) keep the cheaper.
            push(work, se.a, s_to_a + u_bwd, FROM_START_A, node_xy(se.a));
        }
        let mut settled = 0u32;
        let arcs = &w[self.arc_at..self.edge_at];
        let edges = &w[self.edge_at..self.shape_x_at];
        let layer_arcs = &w[self.layer_arc_at..self.step_at];
        // An arc's cost: its time as it stands for the fastest; else its
        // edge's length, or a motorway's made dear.
        let plain = *options == RouteOptions::default();
        let arc_cost = |k: usize| -> u32 {
            let ms = arcs[k * ARC + 1];
            if plain {
                return ms;
            }
            let e = ((arcs[k * ARC + 2] & ARC_EDGE) >> 1) as usize * EDGE;
            let base = match options.by {
                RouteBy::Fastest => ms,
                RouteBy::Shortest => edges[e + 2].max(1),
            };
            if options.avoid_motorways && edges[e + 3] & MOTORWAY != 0 {
                base.saturating_mul(MOTORWAY_PENALTY)
            } else {
                base
            }
        };
        // A layer road's: the same, summed over its edges.
        let layer_cost = |k: usize| -> u32 {
            let r = &layer_arcs[k * LAYER_ARC..k * LAYER_ARC + 5];
            let (all, motorway) = match options.by {
                RouteBy::Fastest => (r[1], r[3]),
                RouteBy::Shortest => (r[2].max(1), r[4]),
            };
            if options.avoid_motorways {
                all.saturating_add(motorway.saturating_mul(MOTORWAY_PENALTY - 1))
            } else {
                all
            }
        };
        while let Some(Reverse(top)) = work.heap.pop() {
            let f = top >> 32;
            let state = (top & 0xFFFF_FFFF) as u32;
            if best.is_some_and(|b| f >= b.0) {
                break;
            }
            let (g, parent) = work.reached[&state];
            if state & ON_LAYER != 0 {
                // A layer junction: along the layer's roads, and down off
                // it where one of them can reach the goal's surroundings.
                let l = state & !ON_LAYER;
                let (n, (x, y)) = junction(l);
                if f != u64::from(g.saturating_add(h(x, y))) {
                    continue; // a stale entry: reached cheaper since
                }
                settled += 1;
                for k in roads(l) {
                    let to = layer_arcs[k * LAYER_ARC];
                    let g2 = g.saturating_add(layer_cost(k));
                    push(work, ON_LAYER | to, g2, k as u32, junction(to).1);
                }
                if comes_down(l, x, y) {
                    push(work, n, g, DROP, (x, y));
                }
                continue;
            }
            let n = state;
            let i = n as usize;
            let (x, y) = node_xy(n);
            if f != u64::from(g.saturating_add(h(x, y))) {
                continue; // a stale entry: reached cheaper since
            }
            settled += 1;
            // The goal's edge from here.
            if n == ee.a && ee.forward {
                let total = u64::from(g) + u64::from(a_to_e);
                if best.is_none_or(|b| total < b.0) {
                    best = Some((total, Some(n), true));
                }
            }
            if n == ee.b && ee.backward {
                let total = u64::from(g) + u64::from(b_to_e);
                if best.is_none_or(|b| total < b.0) {
                    best = Some((total, Some(n), false));
                }
            }
            let close = near(x, y);
            let layer = if layered { self.layer_of(n) } else { None };
            // Far from both ends a layer junction is left on the layer,
            // and its main roads are followed only towards the goal's
            // surroundings.
            if let Some(l) = layer {
                if parent != DROP {
                    push(work, ON_LAYER | l, g, LIFT, (x, y));
                }
                if !close && !comes_down(l, x, y) {
                    continue;
                }
            }
            let at = self.node_at + i * NODE;
            let (first, last) = (w[at + 2] as usize, w[at + NODE + 2] as usize);
            for k in first..last {
                // Far from both ends, main roads only.
                if close || arcs[k * ARC + 2] & ARC_MAIN != 0 {
                    let to = arcs[k * ARC];
                    push(
                        work,
                        to,
                        g.saturating_add(arc_cost(k)),
                        k as u32,
                        node_xy(to),
                    );
                }
            }
        }
        let (cost, via, end_forward) = best?;
        // Back from the node the goal's edge was entered at.
        let mut path = Vec::new();
        let mut start_forward = end_forward;
        if let Some(n) = via {
            let mut state = n;
            let mut hops = 0usize;
            loop {
                hops += 1;
                if hops > self.nodes + self.layer_nodes + 2 {
                    return None; // a loop of parents: cannot happen
                }
                let &(_, k) = work.reached.get(&state)?;
                if state & ON_LAYER == 0 {
                    match k {
                        FROM_START_B | FROM_START_A => {
                            start_forward = k == FROM_START_B;
                            break;
                        }
                        DROP => state = ON_LAYER | self.layer_of(state)?,
                        _ => {
                            let code = arcs.get(k as usize * ARC + 2)? & ARC_EDGE;
                            let (edge, back) = (code >> 1, code & 1 == 1);
                            path.push((edge, back));
                            let e = self.edge(edge);
                            state = if back { e.b } else { e.a };
                        }
                    }
                } else if k == LIFT {
                    state = junction(state & !ON_LAYER).0;
                } else {
                    // A layer road: its edges, last first, back to the
                    // junction it left.
                    let at = k as usize * LAYER_ARC + 5;
                    let (first, end) = (*layer_arcs.get(at)?, *layer_arcs.get(at + LAYER_ARC)?);
                    let steps =
                        w.get(self.step_at + first as usize..self.step_at + end as usize)?;
                    path.extend(steps.iter().rev().map(|&c| (c >> 1, c & 1 == 1)));
                    let &c = steps.first()?;
                    let e = self.edge(c >> 1);
                    state = ON_LAYER | self.layer_of(if c & 1 == 1 { e.b } else { e.a })?;
                }
            }
            path.reverse();
        }
        Some(RoadPath {
            start: *start,
            end: *end,
            start_forward,
            direct: via.is_none(),
            arcs: path,
            end_forward,
            cost,
            settled,
        })
    }
}

/// The point of the segment a-b nearest p.
fn nearest_on(a: (i64, i64), b: (i64, i64), p: (i64, i64)) -> (i64, i64) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = i128::from(dx) * i128::from(dx) + i128::from(dy) * i128::from(dy);
    if len2 == 0 {
        return a;
    }
    let dot = i128::from(p.0 - a.0) * i128::from(dx) + i128::from(p.1 - a.1) * i128::from(dy);
    if dot <= 0 {
        return a;
    }
    if dot >= len2 {
        return b;
    }
    (
        a.0 + (i128::from(dx) * dot / len2) as i64,
        a.1 + (i128::from(dy) * dot / len2) as i64,
    )
}

/// A network written as scripts/map-graph.py writes it, for tests.
#[cfg(test)]
pub(crate) mod build {
    use std::collections::{BTreeSet, HashMap};

    /// A road: its points from a to b (the first and last are junctions,
    /// numbered by `node`), its name, km/h, one-way, roundabout, limit,
    /// motorway, a main road.
    pub(crate) struct Road {
        pub a: u32,
        pub b: u32,
        pub between: Vec<(i64, i64)>,
        pub name: &'static str,
        pub kmh: u32,
        pub oneway: bool,
        pub roundabout: bool,
        pub limit: u16,
        pub motorway: bool,
        pub main: bool,
    }

    /// A camera as the file has it: x, y, edge, decimetres from a, the
    /// ways it watches (1 a to b, 2 b to a), limit.
    pub(crate) type CameraWords = [u32; 6];

    pub(crate) fn road(a: u32, b: u32, name: &'static str) -> Road {
        Road {
            a,
            b,
            between: Vec::new(),
            name,
            kmh: 50,
            oneway: false,
            roundabout: false,
            limit: 0,
            motorway: false,
            main: false,
        }
    }

    /// Metres per world pixel near 49.07 degrees north, where the tests'
    /// networks are.
    pub(crate) const M_PER_PX: f64 = 0.024_45;

    /// The file's bytes for these junctions and roads.
    pub(crate) fn write(nodes: &[(i64, i64)], roads: &[Road]) -> Vec<u8> {
        write_with(nodes, roads, &[], super::VERSION)
    }

    /// The same as version `version` writes it (1: no cameras and no
    /// motorway marks; 2: the cameras inside, no layer; 3: the layer, the
    /// cameras in a file of their own, [`crate::cameras::write`]).
    pub(crate) fn write_with(
        nodes: &[(i64, i64)],
        roads: &[Road],
        cameras: &[CameraWords],
        version: u32,
    ) -> Vec<u8> {
        let mut names: Vec<u8> = Vec::new();
        let mut name_at = HashMap::new();
        let mut kinds: Vec<(u32, u32)> = Vec::new();
        let mut kind_of = HashMap::new();
        let mut arcs: Vec<Vec<[u32; 3]>> = vec![Vec::new(); nodes.len()];
        let mut degree = vec![0u32; nodes.len()];
        let mut edges = Vec::new();
        let mut shape_x = Vec::new();
        let mut shape_y = Vec::new();
        let mut costs = Vec::new(); // (ms, dm) by edge
        for (e, r) in roads.iter().enumerate() {
            let at = *name_at.entry(r.name).or_insert_with(|| {
                let at = names.len() as u32;
                names.push(r.name.len() as u8);
                names.extend_from_slice(r.name.as_bytes());
                at
            });
            let key = (at, u32::from(r.limit) | (u32::from(r.roundabout) << 16));
            let kind = *kind_of.entry(key).or_insert_with(|| {
                kinds.push(key);
                kinds.len() as u32 - 1
            });
            let mut pts = vec![nodes[r.a as usize]];
            pts.extend(&r.between);
            pts.push(nodes[r.b as usize]);
            let px: f64 = pts
                .windows(2)
                .map(|p| ((p[1].0 - p[0].0) as f64).hypot((p[1].1 - p[0].1) as f64))
                .sum();
            let m = px * M_PER_PX;
            let ms = ((m / (f64::from(r.kmh) / 3.6)) * 1000.0).round().max(1.0) as u32;
            let dm = (m * 10.0).round().max(1.0) as u32;
            costs.push((ms, dm));
            let forward = true;
            let backward = !(r.oneway || r.roundabout);
            let main = if r.main && version > 2 {
                super::ARC_MAIN
            } else {
                0
            };
            arcs[r.a as usize].push([r.b, ms, (e as u32 * 2) | main]);
            if backward {
                arcs[r.b as usize].push([r.a, ms, (e as u32 * 2 + 1) | main]);
            }
            degree[r.a as usize] += 1;
            degree[r.b as usize] += 1;
            let motorway = r.motorway && version > 1;
            let info = kind
                | (u32::from(forward) << 24)
                | (u32::from(backward) << 25)
                | (u32::from(motorway) << 26);
            edges.push([r.a, r.b, dm, info, shape_x.len() as u32]);
            for p in &r.between {
                shape_x.push(p.0 as u32);
                shape_y.push(p.1 as u32);
            }
        }
        edges.push([0, 0, 0, 0, shape_x.len() as u32]);
        let layer = if version > 2 {
            layer(nodes, roads, &costs)
        } else {
            Layer::default()
        };
        // One cell holding every edge.
        let all: Vec<(i64, i64)> = nodes.to_vec();
        let x0 = all.iter().map(|p| p.0).min().unwrap() & !0xFFFF;
        let y0 = all.iter().map(|p| p.1).min().unwrap() & !0xFFFF;
        let shift = 24u32;
        let arc_count: usize = arcs.iter().map(Vec::len).sum();
        let h_q16 = (M_PER_PX * 0.98 / (130.0 / 3.6) * 1000.0 * 65536.0) as u32;
        let top_other = roads
            .iter()
            .filter(|r| !r.motorway)
            .map(|r| r.kmh)
            .max()
            .unwrap_or(0);
        let mut body: Vec<u32> = Vec::new();
        let mut first = 0u32;
        for (i, p) in nodes.iter().enumerate() {
            let place = layer.place.get(&(i as u32)).map_or(0, |l| l + 1);
            body.extend([p.0 as u32, p.1 as u32, first, degree[i] | place << 8]);
            first += arcs[i].len() as u32;
        }
        body.extend([0, 0, first, 0]);
        for out in &arcs {
            for a in out {
                body.extend(a);
            }
        }
        for e in &edges {
            body.extend(e);
        }
        body.extend(&shape_x);
        body.extend(&shape_y);
        for k in &kinds {
            body.extend([k.0, k.1]);
        }
        body.extend([0, roads.len() as u32]);
        body.extend(0..roads.len() as u32);
        while !names.len().is_multiple_of(4) {
            names.push(0);
        }
        body.extend(
            names
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c)),
        );
        if version == 2 {
            body.extend(cameras.iter().flatten());
        }
        if version > 2 {
            body.extend(&layer.nodes);
            body.extend(&layer.arcs);
            body.extend(&layer.steps);
        }
        let mut w: Vec<u32> = vec![0, 0];
        w.extend([
            version,
            nodes.len() as u32,
            arc_count as u32,
            roads.len() as u32,
            shape_x.len() as u32,
            kinds.len() as u32,
            roads.len() as u32,
            names.len() as u32,
            x0 as u32,
            y0 as u32,
            shift,
            1,
            1,
            h_q16,
            130,
        ]);
        match version {
            1 => w.push(0),
            2 => w.extend([cameras.len() as u32, top_other, 0]),
            _ => {
                let id = build_id(&body);
                w.extend([0, top_other, 0, id as u32, (id >> 32) as u32]);
                w.extend([
                    (layer.nodes.len() / super::LAYER_NODE - 1) as u32,
                    (layer.arcs.len() / super::LAYER_ARC - 1) as u32,
                    layer.steps.len() as u32,
                    0,
                ]);
            }
        }
        w.extend(body);
        let mut bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
        bytes[..8].copy_from_slice(super::MAGIC);
        bytes
    }

    /// A build's ID from its words (map-graph.py hashes them otherwise:
    /// any hash does, the same network giving the same one).
    fn build_id(words: &[u32]) -> u64 {
        words.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &w| {
            (h ^ u64::from(w)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    /// The layer's sections, and each junction's place in it by node.
    #[derive(Default)]
    struct Layer {
        place: HashMap<u32, u32>,
        nodes: Vec<u32>,
        arcs: Vec<u32>,
        steps: Vec<u32>,
    }

    /// The main roads as a network of their own, as map-graph.py's
    /// `main_layer` makes it.
    fn layer(nodes: &[(i64, i64)], roads: &[Road], costs: &[(u32, u32)]) -> Layer {
        let ends = |e: usize| {
            let r = &roads[e];
            (r.a, r.b, true, !(r.oneway || r.roundabout))
        };
        let mut touching: HashMap<u32, Vec<usize>> = HashMap::new();
        for (e, r) in roads.iter().enumerate() {
            if r.main {
                touching.entry(r.a).or_default().push(e);
                touching.entry(r.b).or_default().push(e);
            }
        }
        let leaves = |e: usize, n: u32| {
            let (a, b, f, bk) = ends(e);
            (f && a == n) || (bk && b == n)
        };
        let enters = |e: usize, n: u32| {
            let (a, b, f, bk) = ends(e);
            (f && b == n) || (bk && a == n)
        };
        let other = |e: usize, n: u32| {
            let (a, b, _, _) = ends(e);
            if a == n { b } else { a }
        };
        let interior = |n: u32| {
            let es = &touching[&n];
            if es.len() != 2 || es[0] == es[1] {
                return false;
            }
            let (e1, e2) = (es[0], es[1]);
            (enters(e1, n), leaves(e1, n)) == (leaves(e2, n), enters(e2, n))
        };
        let mut junction: BTreeSet<u32> =
            touching.keys().copied().filter(|&n| !interior(n)).collect();
        let mut seen = vec![false; roads.len()];
        let mut found: Vec<(u32, u32, Vec<u32>)> = Vec::new();
        let mut walk = |j: u32, mut e: usize, junction: &BTreeSet<u32>, seen: &mut [bool]| {
            let (mut steps, mut n, mut along, mut against) = (Vec::new(), j, true, true);
            loop {
                seen[e] = true;
                let (a, b, _, _) = ends(e);
                let back = b == n && a != n;
                along &= leaves(e, n);
                against &= enters(e, n);
                steps.push(e as u32 * 2 + u32::from(back));
                n = other(e, n);
                if junction.contains(&n) {
                    break;
                }
                let es = &touching[&n];
                e = if es[0] == e { es[1] } else { es[0] };
            }
            if n == j {
                return;
            }
            if along {
                found.push((j, n, steps.clone()));
            }
            if against {
                found.push((n, j, steps.iter().rev().map(|s| s ^ 1).collect()));
            }
        };
        for j in junction.clone() {
            for &e in &touching[&j] {
                if !seen[e] {
                    walk(j, e, &junction, &mut seen);
                }
            }
        }
        for e in 0..roads.len() {
            if roads[e].main && !seen[e] {
                let j = roads[e].a;
                junction.insert(j);
                walk(j, e, &junction, &mut seen);
            }
        }
        let order: Vec<u32> = junction.into_iter().collect();
        let place: HashMap<u32, u32> = order
            .iter()
            .enumerate()
            .map(|(i, &n)| (n, i as u32))
            .collect();
        let mut out: Vec<Vec<(u32, Vec<u32>)>> = vec![Vec::new(); order.len()];
        for (f, t, steps) in found {
            out[place[&f] as usize].push((place[&t], steps));
        }
        let mut l = Layer {
            place,
            ..Layer::default()
        };
        let mut first = 0;
        for (i, &n) in order.iter().enumerate() {
            let p = nodes[n as usize];
            l.nodes.extend([n, p.0 as u32, p.1 as u32, first]);
            first += out[i].len() as u32;
        }
        l.nodes.extend([0, 0, 0, first]);
        for road_out in &out {
            for (t, steps) in road_out {
                let sum = |motorway_only: bool, f: fn(&(u32, u32)) -> u32| -> u32 {
                    steps
                        .iter()
                        .map(|s| (s >> 1) as usize)
                        .filter(|&e| !motorway_only || roads[e].motorway)
                        .map(|e| f(&costs[e]))
                        .sum()
                };
                l.arcs.extend([
                    *t,
                    sum(false, |c| c.0),
                    sum(false, |c| c.1),
                    sum(true, |c| c.0),
                    sum(true, |c| c.1),
                    l.steps.len() as u32,
                ]);
                l.steps.extend(steps);
            }
        }
        l.arcs.extend([0, 0, 0, 0, 0, l.steps.len() as u32]);
        l
    }
}
