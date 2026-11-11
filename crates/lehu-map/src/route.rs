// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Routes found on the board: a destination searched by name, the fastest
//! (or the shortest) way there over the road network, and that route made
//! a drive with its turns, as scripts/route-gpx.py makes one on a PC.

use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use crate::graph::{Graph, RoadPath, RouteBy, RouteOptions, Work};
use crate::search::{Found as Place, Places};
use crate::track::{Leg, LegStep, Track, Turn, metres_per_px};

/// The turn at a junction is measured over this much road either side.
const TURN_SPAN_M: f64 = 15.0;

/// The road network and the names, from the map's directory.
pub struct Router {
    graph: Graph,
    places: Option<Places>,
    checked: OnceLock<Result<(), String>>,
    work: Mutex<Work>,
}

impl Router {
    /// `roads.bin` and, when it is there, `places.bin` in `dir`, mapped.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let graph = Graph::open(dir)?;
        let places = match Places::open(dir) {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("map: no search: {e}");
                None
            }
        };
        log::info!(
            "map: {} junctions and {} roads to route over, {} names",
            graph.nodes(),
            graph.edges(),
            places.as_ref().map_or(0, Places::len)
        );
        Ok(Self::new(graph, places))
    }

    pub fn new(graph: Graph, places: Option<Places>) -> Self {
        Self {
            graph,
            places,
            checked: OnceLock::new(),
            work: Mutex::new(Work::default()),
        }
    }

    /// Check the road network now rather than at the first route, which
    /// then starts at once; reading it all also brings the file in from
    /// the card. Meant for a thread of its own: it takes ~0.4 s on the
    /// board.
    pub fn prepare(&self) {
        let _ = self.checked.get_or_init(|| self.graph.check());
    }

    /// There are names to search.
    pub fn can_search(&self) -> bool {
        self.places.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// Up to `limit` destinations for `query`, nearest `near` first among
    /// equals (see [`Places::search`]).
    pub fn search(&self, query: &str, near: Option<(i64, i64)>, limit: usize) -> Vec<Place> {
        self.places
            .as_ref()
            .map_or_else(Vec::new, |p| p.search(query, near, limit))
    }

    /// The fastest route from `from` to `to` (world pixels at zoom 22),
    /// each taken to the nearest road, as a drive. `heading`: the way we
    /// are going, when we move. The reason, in a few words for the screen,
    /// when there is none.
    pub fn route(
        &self,
        from: (i64, i64),
        heading: Option<(i32, i32)>,
        to: (i64, i64),
    ) -> Result<Track, String> {
        self.route_with(from, heading, to, &RouteOptions::default())
    }

    /// The same, the route chosen as `options` say: the fastest or the
    /// shortest, motorways avoided or not. With road data that has no
    /// motorway marks (version 1) motorways cannot be told apart, and
    /// avoiding them changes nothing.
    pub fn route_with(
        &self,
        from: (i64, i64),
        heading: Option<(i32, i32)>,
        to: (i64, i64),
        options: &RouteOptions,
    ) -> Result<Track, String> {
        let began = Instant::now();
        if let Err(e) = self.checked.get_or_init(|| self.graph.check()) {
            log::warn!("map: the road network is broken: {e}");
            return Err("The road data is broken".into());
        }
        let checked = began.elapsed();
        let start = self
            .graph
            .snap(from.0, from.1)
            .ok_or("No road near where we are")?;
        let end = self
            .graph
            .snap(to.0, to.1)
            .ok_or("No road near the destination")?;
        let path = {
            let mut work = self.work.lock().unwrap_or_else(PoisonError::into_inner);
            self.graph
                .route_with(&start, heading, &end, options, &mut work)
        }
        .ok_or("No route there")?;
        let searched = began.elapsed();
        let leg = leg(&self.graph, &path);
        if leg.points.len() < 2 {
            return Err("We are there".into());
        }
        let track = Track::from_route(&leg)?;
        log::info!(
            "map: {} route of {:.1} km, {} min, {} turns: {} junctions in {} ms, {} ms in all{}",
            match (options.by, options.avoid_motorways) {
                (RouteBy::Fastest, false) => "fastest",
                (RouteBy::Fastest, true) => "fastest off motorways",
                (RouteBy::Shortest, false) => "shortest",
                (RouteBy::Shortest, true) => "shortest off motorways",
            },
            f64::from(track.metres()) / 1000.0,
            track.duration().div_ceil(60_000),
            track.maneuvers().len(),
            path.settled,
            (searched - checked).as_millis(),
            began.elapsed().as_millis(),
            if checked.as_millis() > 1 {
                format!(" ({} ms checking the data)", checked.as_millis())
            } else {
                String::new()
            }
        );
        Ok(track)
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }
}

/// A route's points with what the turns need: at each point the junction
/// it is, when it is one, and for each step the edge it is on.
#[derive(Default)]
struct Line {
    pts: Vec<(i64, i64)>,
    node: Vec<Option<u32>>,
    edge: Vec<u32>,
}

impl Line {
    /// The next point, reached along `edge`; one that is where the last
    /// point is adds no step (but its junction).
    fn push(&mut self, p: (i64, i64), node: Option<u32>, edge: u32) {
        if self.pts.last() == Some(&p) {
            if node.is_some() {
                *self.node.last_mut().expect("a point") = node;
            }
            return;
        }
        if !self.pts.is_empty() {
            self.edge.push(edge);
        }
        self.pts.push(p);
        self.node.push(node);
    }
}

/// The points of the route, its steps' speeds and limits, and its turns.
pub(crate) fn leg(graph: &Graph, path: &RoadPath) -> Leg {
    let line = line(graph, path);
    let turns = turns(graph, &line);
    let steps = line
        .edge
        .iter()
        .map(|&e| {
            let edge = graph.edge(e);
            let ms = graph.edge_ms(e).max(1);
            LegStep {
                cm_s: (u64::from(edge.dm) * 10_000 / u64::from(ms)).min(u64::from(u32::MAX)) as u32,
                limit: graph.kind(edge.kind).limit,
            }
        })
        .collect();
    Leg {
        points: line.pts,
        steps,
        turns,
    }
}

/// The route's points along the roads, from the start's point to the
/// goal's.
fn line(graph: &Graph, path: &RoadPath) -> Line {
    let mut out = Line::default();
    // An edge's points in order, a first and b last: their junctions.
    let at = |e: u32, line: &[(i64, i64)], k: usize| {
        let edge = graph.edge(e);
        if k == 0 {
            Some(edge.a)
        } else if k == line.len() - 1 {
            Some(edge.b)
        } else {
            None
        }
    };
    let (s, t) = (path.start, path.end);
    out.push((s.x, s.y), None, s.edge);
    if path.direct {
        let l = graph.line(s.edge);
        if path.start_forward {
            for k in s.seg as usize + 1..=t.seg as usize {
                out.push(l[k], at(s.edge, &l, k), s.edge);
            }
        } else {
            for k in (t.seg as usize + 1..=s.seg as usize).rev() {
                out.push(l[k], at(s.edge, &l, k), s.edge);
            }
        }
        out.push((t.x, t.y), None, s.edge);
        return out;
    }
    let l = graph.line(s.edge);
    if path.start_forward {
        for k in s.seg as usize + 1..l.len() {
            out.push(l[k], at(s.edge, &l, k), s.edge);
        }
    } else {
        for k in (0..=s.seg as usize).rev() {
            out.push(l[k], at(s.edge, &l, k), s.edge);
        }
    }
    for &(e, back) in &path.arcs {
        let l = graph.line(e);
        if back {
            for k in (0..l.len() - 1).rev() {
                out.push(l[k], at(e, &l, k), e);
            }
        } else {
            for k in 1..l.len() {
                out.push(l[k], at(e, &l, k), e);
            }
        }
    }
    let l = graph.line(t.edge);
    if path.end_forward {
        for k in 1..=t.seg as usize {
            out.push(l[k], at(t.edge, &l, k), t.edge);
        }
    } else {
        for k in (t.seg as usize + 1..l.len() - 1).rev() {
            out.push(l[k], at(t.edge, &l, k), t.edge);
        }
    }
    out.push((t.x, t.y), None, t.edge);
    out
}

/// The turns along the route, as scripts/route-gpx.py's `maneuvers` finds
/// them: into a roundabout, the exit to take (each way out of it passed,
/// and the one taken); elsewhere at a junction, a turn of 45 degrees or
/// more, or a road with another name; the arrival last.
fn turns(graph: &Graph, line: &Line) -> Vec<(usize, Turn, String)> {
    let steps = line.edge.len();
    let kind = |i: usize| graph.kind(graph.edge(line.edge[i]).kind);
    let mut out = Vec::new();
    let mut i = 1;
    while i + 1 < line.pts.len() {
        let (before, after) = (kind(i - 1), kind(i));
        if after.roundabout && !before.roundabout {
            // Count the ways out up to the one taken, and name the road
            // after it.
            let mut j = i;
            let mut exits = 0u8;
            while j < steps && kind(j).roundabout {
                j += 1;
                let leaving = j >= steps || !kind(j).roundabout;
                let others = line.node[j].is_some_and(|n| {
                    graph.out(n).any(|(_, e, _)| {
                        !graph.kind(graph.edge(e).kind).roundabout
                            && e != line.edge[j - 1]
                            && line.edge.get(j) != Some(&e)
                    })
                });
                if leaving || others {
                    exits = exits.saturating_add(1);
                }
            }
            let onto = if j < steps { kind(j).name } else { "" };
            out.push((i, Turn::Roundabout(exits.max(1)), onto.to_string()));
            i = j + 1;
            continue;
        }
        let (Some(turn_in), Some(turn_out)) =
            (heading_near(line, i, true), heading_near(line, i, false))
        else {
            i += 1;
            continue;
        };
        let turn = (turn_out - turn_in + 180.0).rem_euclid(360.0) - 180.0;
        let what = kind_of(turn);
        let junction = line.node[i].is_some_and(|n| graph.degree(n) > 2);
        let renamed = after.name != before.name;
        let worth =
            (renamed && junction) || (junction && what != Turn::Straight && turn.abs() >= 45.0);
        // Straight on onto a road with no name: nothing to say.
        if worth && !(what == Turn::Straight && after.name.is_empty()) {
            out.push((i, what, after.name.to_string()));
        }
        i += 1;
    }
    out.push((line.pts.len() - 1, Turn::Arrive, String::new()));
    out
}

/// The bearing at point i (degrees clockwise from north), over about 15 m
/// of the route behind it (`back`) or ahead of it.
fn heading_near(line: &Line, i: usize, back: bool) -> Option<f64> {
    let here = line.pts[i];
    let mut j = i;
    let mut run = 0.0;
    while run < TURN_SPAN_M {
        let k = if back {
            match j.checked_sub(1) {
                Some(k) => k,
                None => break,
            }
        } else {
            j + 1
        };
        if k >= line.pts.len() {
            break;
        }
        let (a, b) = (line.pts[j], line.pts[k]);
        run += ((b.0 - a.0) as f64).hypot((b.1 - a.1) as f64) * metres_per_px(a.1);
        j = k;
    }
    if j == i {
        return None;
    }
    let there = line.pts[j];
    let (from, to) = if back { (there, here) } else { (here, there) };
    // East is x, north is up the map: -y.
    let (east, north) = ((to.0 - from.0) as f64, (from.1 - to.1) as f64);
    Some(east.atan2(north).to_degrees())
}

/// A turn's manoeuvre, from its angle in degrees (right positive).
fn kind_of(turn: f64) -> Turn {
    let a = turn.abs();
    let right = turn > 0.0;
    if a < 20.0 {
        Turn::Straight
    } else if a < 45.0 {
        if right {
            Turn::SlightRight
        } else {
            Turn::SlightLeft
        }
    } else if a < 135.0 {
        if right { Turn::Right } else { Turn::Left }
    } else if a < 170.0 {
        if right {
            Turn::SharpRight
        } else {
            Turn::SharpLeft
        }
    } else {
        Turn::UTurn
    }
}
