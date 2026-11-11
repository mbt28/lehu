// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The road network, for finding routes on the board: junctions, the roads
//! between them, and a grid to find the road nearest a point. It is written
//! on a PC by scripts/map-graph.py as `roads.bin` and mapped from the card
//! here, never loaded: a country's network is a few megabytes, and a route
//! looks at only part of it.
//!
//! The file is little-endian 32-bit words. After a header of 20 words (18
//! in version 1) come these sections, each right after the one before:
//!
//! | Section | Words each | What |
//! |---|---|---|
//! | nodes + 1 | 4 | x, y (world pixels at zoom 22), its first arc, how many roads meet there |
//! | arcs | 3 | the node it leads to, milliseconds, edge * 2 (+ 1 when it runs the edge from b to a) |
//! | edges + 1 | 5 | a, b, decimetres, kind (low 24 bits) + 1 << 24 a to b allowed + 1 << 25 b to a allowed + 1 << 26 a motorway (or its link), its first shape point |
//! | shape points | 1 | x, then all the y |
//! | kinds | 2 | the road's name (a byte offset into the names), speed limit km/h (0 none) + 1 << 16 roundabout |
//! | cells + 1 | 1 | the grid: each cell's first entry in the next section |
//! | entries | 1 | the edges with a piece in the cell |
//! | names | bytes | each a length byte, then UTF-8 |
//! | cameras | 6 | x, y, the edge, decimetres from its a, the ways it watches (1 a to b, 2 b to a), its speed limit km/h (0 not known) |
//!
//! The header is "LEHUROAD", the version (2), the counts of nodes, arcs,
//! edges, shape points, kinds, grid entries and name bytes, the grid (its
//! corner x and y, the cell's size as a shift, its columns and rows), the
//! search's speed bound (milliseconds per 65536 world pixels, the fewest
//! a straight line can take anywhere on the network), the top speed in
//! km/h, the count of speed cameras, the top speed of the roads that are
//! not motorways, and a word kept 0. Version 1, which LEHU still reads,
//! stops after the top speed: it has no cameras and no motorway marks, so
//! with it routes cannot avoid motorways. The cameras are in the order of
//! their edges, and along each from a. A node's arcs are the ones a car may take from it, its last one
//! just before the next node's first (the extra node at the end closes the
//! list); an edge's shape is the points between a and b, up to the next
//! edge's first.
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

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;

use crate::blob::{Blob, short_str};

/// The file's name in the map's directory.
pub const ROADS_FILE: &str = "roads.bin";

const MAGIC: &[u8; 8] = b"LEHUROAD";
const VERSION: u32 = 2;
const HEADER: usize = 20;
/// Version 1's header: no cameras, no motorway marks.
const HEADER_V1: usize = 18;
const NODE: usize = 4;
const ARC: usize = 3;
const EDGE: usize = 5;
const KIND: usize = 2;
const CAMERA: usize = 6;

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
/// b, or towards a.
const FROM_START_B: u32 = u32::MAX;
const FROM_START_A: u32 = u32::MAX - 1;

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
    cameras: usize,
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

/// A speed camera, on an edge: where, how far from the edge's a, which ways
/// along it it watches, and the speed limit it checks when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Camera {
    pub x: i64,
    pub y: i64,
    pub edge: u32,
    pub from_a_dm: u32,
    /// It watches the traffic from a to b, from b to a.
    pub forward: bool,
    pub backward: bool,
    pub limit: Option<u16>,
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
    /// The nodes the search took from its queue.
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

/// What a search keeps between routes: a cost and a parent per node, and
/// the queue.
#[derive(Default)]
pub struct Work {
    g: Vec<u32>,
    parent: Vec<u32>,
    heap: BinaryHeap<Reverse<u64>>,
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
        if w.len() < HEADER || blob.bytes(0, 8) != Some(&MAGIC[..]) {
            return Err("not a LEHU road network".into());
        }
        let version = w[2];
        if !(1..=VERSION).contains(&version) {
            return Err(format!("version {version}, LEHU reads 1 to {VERSION}"));
        }
        let header = if version == 1 { HEADER_V1 } else { HEADER };
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
        let (cameras, top_other_kmh) = if version == 1 {
            (0, w[16])
        } else {
            (n(17), w[18])
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
        if camera_at + cameras * CAMERA != w.len() {
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
            cameras,
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
            blob,
        })
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
            if arc[0] as usize >= self.nodes || (arc[2] >> 1) as usize >= self.edges {
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
        let mut last = (0, 0);
        for i in 0..self.cameras {
            let c = &w[self.camera_at + i * CAMERA..self.camera_at + (i + 1) * CAMERA];
            if c[2] as usize >= self.edges || c[0] >= 1 << 30 || c[1] >= 1 << 30 {
                return Err(format!("camera {i}: out of range"));
            }
            if (c[2], c[3]) < last {
                return Err(format!("camera {i}: out of order"));
            }
            last = (c[2], c[3]);
        }
        Ok(())
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

    /// How many speed cameras the file has.
    pub fn cameras(&self) -> usize {
        self.cameras
    }

    /// Camera `i`, in the order of their edges.
    pub fn camera(&self, i: usize) -> Camera {
        let at = self.camera_at + i * CAMERA;
        let c = &self.words()[at..at + CAMERA];
        let limit = (c[5] & 0xFFFF) as u16;
        Camera {
            x: i64::from(c[0]),
            y: i64::from(c[1]),
            edge: c[2],
            from_a_dm: c[3],
            forward: c[4] & 1 != 0,
            backward: c[4] & 2 != 0,
            limit: (limit > 0).then_some(limit),
        }
    }

    /// The cameras on edge `e`, from its a.
    pub fn cameras_on(&self, e: u32) -> impl Iterator<Item = Camera> + '_ {
        let w = self.words();
        let edge_of = |i: usize| w[self.camera_at + i * CAMERA + 2];
        // The first at or after the edge: they are in its order.
        let (mut lo, mut hi) = (0, self.cameras);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if edge_of(mid) < e {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo..self.cameras)
            .take_while(move |&i| edge_of(i) == e)
            .map(|i| self.camera(i))
    }

    fn words(&self) -> &[u32] {
        self.blob.words()
    }

    /// A node's point, world pixels.
    pub fn node(&self, n: u32) -> (i64, i64) {
        let at = self.node_at + n as usize * NODE;
        let w = self.words();
        (i64::from(w[at]), i64::from(w[at + 1]))
    }

    /// How many roads meet at a node, whichever way they may be driven.
    pub fn degree(&self, n: u32) -> u32 {
        self.words()[self.node_at + n as usize * NODE + 3]
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
            .map(|a| (a[0], a[2] >> 1, a[2] & 1 == 1))
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
            .find(|a| a[2] == e * 2 + u32::from(back))
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
    /// time or by distance, motorways made dear or not. As [`Self::route`]
    /// otherwise.
    pub fn route_with(
        &self,
        start: &Snap,
        heading: Option<(i32, i32)>,
        end: &Snap,
        options: &RouteOptions,
        work: &mut Work,
    ) -> Option<RoadPath> {
        let w = self.words();
        work.g.clear();
        work.g.resize(self.nodes, u32::MAX);
        work.parent.clear();
        work.parent.resize(self.nodes, FROM_START_B);
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
        let h = |n: usize| -> u32 {
            let at = self.node_at + n * NODE;
            let dx = (i64::from(w[at]) - tx).unsigned_abs();
            let dy = (i64::from(w[at + 1]) - ty).unsigned_abs();
            // A lower bound on the straight line, with no root: the
            // longer side, or the two together over a little more than
            // the square root of two.
            let d = dx.max(dy).max(((dx + dy) * 181) >> 8);
            ((d * h_q16) >> 16).min(u64::from(u32::MAX >> 1)) as u32
        };
        let push = |work: &mut Work, n: u32, g: u32, parent: u32| {
            let i = n as usize;
            if g < work.g[i] {
                work.g[i] = g;
                work.parent[i] = parent;
                let f = u64::from(g.saturating_add(h(i)));
                work.heap.push(Reverse((f << 32) | u64::from(n)));
            }
        };
        if se.forward {
            push(work, se.b, s_to_b + u_fwd, FROM_START_B);
        }
        if se.backward {
            // Two starts at one node (a loop) keep the cheaper.
            push(work, se.a, s_to_a + u_bwd, FROM_START_A);
        }
        let mut settled = 0u32;
        let arcs = &w[self.arc_at..self.edge_at];
        let edges = &w[self.edge_at..self.shape_x_at];
        // An arc's cost: its time as it stands for the fastest; else its
        // edge's length, or a motorway's made dear.
        let plain = *options == RouteOptions::default();
        let arc_cost = |k: usize| -> u32 {
            let ms = arcs[k * ARC + 1];
            if plain {
                return ms;
            }
            let e = (arcs[k * ARC + 2] >> 1) as usize * EDGE;
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
        while let Some(Reverse(top)) = work.heap.pop() {
            let f = top >> 32;
            let n = (top & 0xFFFF_FFFF) as u32;
            let i = n as usize;
            if best.is_some_and(|b| f >= b.0) {
                break;
            }
            let g = work.g[i];
            if f != u64::from(g.saturating_add(h(i))) {
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
            let at = self.node_at + i * NODE;
            let (first, last) = (w[at + 2] as usize, w[at + NODE + 2] as usize);
            for k in first..last {
                let to = arcs[k * ARC];
                push(work, to, g.saturating_add(arc_cost(k)), k as u32);
            }
        }
        let (cost, via, end_forward) = best?;
        // Back from the node the goal's edge was entered at.
        let mut path = Vec::new();
        let mut start_forward = end_forward;
        if let Some(mut n) = via {
            loop {
                let k = work.parent[n as usize];
                if k == FROM_START_B || k == FROM_START_A {
                    start_forward = k == FROM_START_B;
                    break;
                }
                let k = k as usize;
                let code = arcs[k * ARC + 2];
                let (edge, back) = (code >> 1, code & 1 == 1);
                path.push((edge, back));
                let e = self.edge(edge);
                n = if back { e.b } else { e.a };
                if path.len() > self.nodes {
                    return None; // a loop of parents: cannot happen
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
    /// A road: its points from a to b (the first and last are junctions,
    /// numbered by `node`), its name, km/h, one-way, roundabout, limit,
    /// motorway.
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
        }
    }

    /// Metres per world pixel near 49.07 degrees north, where the tests'
    /// networks are.
    pub(crate) const M_PER_PX: f64 = 0.024_45;

    /// The file's bytes for these junctions and roads.
    pub(crate) fn write(nodes: &[(i64, i64)], roads: &[Road]) -> Vec<u8> {
        write_with(nodes, roads, &[], super::VERSION)
    }

    /// The same with cameras, as version `version` writes it (1: no
    /// cameras and no motorway marks).
    pub(crate) fn write_with(
        nodes: &[(i64, i64)],
        roads: &[Road],
        cameras: &[CameraWords],
        version: u32,
    ) -> Vec<u8> {
        let mut names: Vec<u8> = Vec::new();
        let mut name_at = std::collections::HashMap::new();
        let mut kinds: Vec<(u32, u32)> = Vec::new();
        let mut kind_of = std::collections::HashMap::new();
        let mut arcs: Vec<Vec<[u32; 3]>> = vec![Vec::new(); nodes.len()];
        let mut degree = vec![0u32; nodes.len()];
        let mut edges = Vec::new();
        let mut shape_x = Vec::new();
        let mut shape_y = Vec::new();
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
            let forward = true;
            let backward = !(r.oneway || r.roundabout);
            arcs[r.a as usize].push([r.b, ms, e as u32 * 2]);
            if backward {
                arcs[r.b as usize].push([r.a, ms, e as u32 * 2 + 1]);
            }
            degree[r.a as usize] += 1;
            degree[r.b as usize] += 1;
            let motorway = r.motorway && version > 1;
            let info = kind
                | (u32::from(forward) << 24)
                | (u32::from(backward) << 25)
                | (u32::from(motorway) << 26);
            edges.push([
                r.a,
                r.b,
                (m * 10.0).round().max(1.0) as u32,
                info,
                shape_x.len() as u32,
            ]);
            for p in &r.between {
                shape_x.push(p.0 as u32);
                shape_y.push(p.1 as u32);
            }
        }
        edges.push([0, 0, 0, 0, shape_x.len() as u32]);
        // One cell holding every edge.
        let all: Vec<(i64, i64)> = nodes.to_vec();
        let x0 = all.iter().map(|p| p.0).min().unwrap() & !0xFFFF;
        let y0 = all.iter().map(|p| p.1).min().unwrap() & !0xFFFF;
        let shift = 24u32;
        let mut w: Vec<u32> = Vec::new();
        w.extend([0, 0]); // the magic, below
        let arc_count: usize = arcs.iter().map(Vec::len).sum();
        let h_q16 = (M_PER_PX * 0.98 / (130.0 / 3.6) * 1000.0 * 65536.0) as u32;
        let top_other = roads
            .iter()
            .filter(|r| !r.motorway)
            .map(|r| r.kmh)
            .max()
            .unwrap_or(0);
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
        if version > 1 {
            w.extend([cameras.len() as u32, top_other, 0]);
        } else {
            w.push(0);
        }
        let mut first = 0u32;
        for (i, p) in nodes.iter().enumerate() {
            w.extend([p.0 as u32, p.1 as u32, first, degree[i]]);
            first += arcs[i].len() as u32;
        }
        w.extend([0, 0, first, 0]);
        for out in &arcs {
            for a in out {
                w.extend(a);
            }
        }
        for e in &edges {
            w.extend(e);
        }
        w.extend(&shape_x);
        w.extend(&shape_y);
        for k in &kinds {
            w.extend([k.0, k.1]);
        }
        w.extend([0, roads.len() as u32]);
        w.extend(0..roads.len() as u32);
        while !names.len().is_multiple_of(4) {
            names.push(0);
        }
        let mut bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
        bytes[..8].copy_from_slice(super::MAGIC);
        bytes.extend(&names);
        if version > 1 {
            bytes.extend(cameras.iter().flatten().flat_map(|v| v.to_le_bytes()));
        }
        bytes
    }
}
