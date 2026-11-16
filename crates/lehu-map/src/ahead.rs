// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The road ahead without a route: from where the position is, along the
//! road it is on the way it goes, and on at each junction where that road
//! goes on (the same name, else the way straight on), for the speed
//! cameras on it. A route of ours knows its cameras itself; this is for a
//! drive with none.
//!
//! The road ahead is found once and then followed: a position near it is
//! placed on it, so a cross street at a junction is not taken for the road.
//! Away from it (the driver turned off), or near its end, it is found anew.

use crate::cameras::Cameras;
use crate::graph::Graph;
use crate::route::Router;
use crate::track::{Fix, ground_px};
use crate::voice::camera_lead;

/// Further from the road than this, metres: not on it (a car park, a road
/// the data does not have, or another road taken).
const ON_ROAD_M: f64 = 25.0;
/// The road ahead is found this far: past the furthest warning (500 m)
/// with some to spare.
const LOOK_M: f64 = 800.0;
/// Found anew when less than this is left ahead.
const RELOOK_M: f64 = 550.0;
/// A road ahead that stopped short (a fork, a dead end) is found anew
/// after this much more of it was driven.
const RELOOK_SHORT_M: f64 = 100.0;
/// Going along a road: the heading within 40 degrees of its way.
const ALONG_COS: f64 = 0.766;
/// At a junction the road goes on into one within 50 degrees of straight
/// on, when it does not go on by name.
const ON_COS: f64 = 0.643;
/// Turned round on the road ahead: against it by more than 120 degrees.
const BACK_COS: f64 = -0.5;
/// Slower than this, km/h, is standing: the heading is not trusted.
const MOVING_KMH: u32 = 5;
/// The road ahead is found anew only this fast: slower, a heading from
/// positions swings (a queue, a junction crawled over), and the road ahead
/// found before is kept.
const FIND_KMH: u32 = 15;
/// The way into or out of a junction is measured over this much road.
const WAY_M: f64 = 10.0;
/// A road ahead found afresh (not on from the last) counts once this much
/// of it was driven: one position off by a lot can put it on a cross
/// street.
const CONFIRM_M: f64 = 30.0;
/// A camera warned of is not warned of again for this long.
const TOLD_MS: u64 = 10 * 60_000;
/// Nor one this near it, metres: a camera is often mapped twice (one
/// point a carriageway or a side of the road), and a road beside it may
/// have the other.
const TOLD_NEAR_M: f64 = 100.0;
/// Not on a road going its way: looked for again this much later.
const MISSED_MS: u64 = 1000;
/// The edges looked along at most.
const EDGES_MAX: usize = 64;

/// The next speed camera on the road ahead: metres to it, the limit it
/// checks when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraAhead {
    pub metres: u32,
    pub limit: Option<u16>,
    /// The camera: its edge, decimetres from the edge's a; where it is.
    id: (u32, u32),
    at: (i64, i64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OnRoad {
    metres: f64,
    id: (u32, u32),
    at: (i64, i64),
    limit: Option<u16>,
}

/// The road ahead of a position: its line from there, the metres along it
/// at each point, and the cameras on it that watch the way it goes.
#[derive(Clone, Debug)]
pub(crate) struct RoadAhead {
    pts: Vec<(i64, i64)>,
    cum: Vec<f64>,
    cameras: Vec<OnRoad>,
    /// It goes the whole `LOOK_M`: it did not stop at a fork or an end.
    full: bool,
}

impl RoadAhead {
    fn len_m(&self) -> f64 {
        self.cum.last().copied().unwrap_or(0.0)
    }

    /// Metres along it to where `pos` is placed on it, when that is within
    /// `ON_ROAD_M`; None too when `heading` goes against it there.
    fn locate(&self, pos: (i64, i64), heading: Option<(i32, i32)>) -> Option<f64> {
        let mut best: Option<(f64, usize, (i64, i64))> = None;
        for (i, w) in self.pts.windows(2).enumerate() {
            let p = nearest_on(w[0], w[1], pos);
            let d = ground_px(pos, p);
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, i, p));
            }
        }
        let (d, i, p) = best?;
        if d > ON_ROAD_M {
            return None;
        }
        if let Some(h) = heading {
            let way = (
                self.pts[i + 1].0 - self.pts[i].0,
                self.pts[i + 1].1 - self.pts[i].1,
            );
            if cos(way, (i64::from(h.0), i64::from(h.1))).is_some_and(|c| c < BACK_COS) {
                return None;
            }
        }
        Some(self.cum[i] + ground_px(self.pts[i], p))
    }
}

/// A road out of a junction: where it leads, its edge, driven from b to
/// a, its line the way it is driven, how straight on it goes (the cosine
/// from the way come), and whether it has the name of the road come along.
struct Out {
    to: u32,
    edge: u32,
    back: bool,
    line: Vec<(i64, i64)>,
    straight: f64,
    named: bool,
}

/// The road ahead of `pos` going `heading`, with the cameras on it: None
/// when `pos` is not on a road going that way.
pub(crate) fn road_ahead(
    graph: &Graph,
    cams: &Cameras,
    pos: (i64, i64),
    heading: (i32, i32),
) -> Option<RoadAhead> {
    let snap = graph.snap(pos.0, pos.1)?;
    if ground_px(pos, (snap.x, snap.y)) > ON_ROAD_M {
        return None;
    }
    let first = graph.edge(snap.edge);
    let line = graph.line(snap.edge);
    let s = snap.seg as usize;
    let way = (line[s + 1].0 - line[s].0, line[s + 1].1 - line[s].1);
    let c = cos(way, (i64::from(heading.0), i64::from(heading.1)))?;
    let towards_b = if c >= ALONG_COS {
        true
    } else if c <= -ALONG_COS {
        false
    } else {
        // Across the road: on another one, at a junction.
        return None;
    };
    // A one-way road the other way: another road, or driven against it.
    if (towards_b && !first.forward) || (!towards_b && !first.backward) {
        return None;
    }
    // The pieces driven: (edge, towards b, from, to decimetres from its a,
    // its first point in `pts`, its last).
    let mut runs: Vec<(u32, bool, u32, u32, usize, usize)> = Vec::new();
    let mut pts = vec![(snap.x, snap.y)];
    let mut node = if towards_b {
        pts.extend_from_slice(&line[s + 1..]);
        runs.push((snap.edge, true, snap.from_a_dm, first.dm, 0, pts.len() - 1));
        first.b
    } else {
        pts.extend(line[..=s].iter().rev());
        runs.push((snap.edge, false, 0, snap.from_a_dm, 0, pts.len() - 1));
        first.a
    };
    let mut cur = snap.edge;
    let mut len = path_m(&pts);
    let mut full = true;
    while len < LOOK_M {
        if runs.len() >= EDGES_MAX {
            break;
        }
        let came = way_back(&pts);
        let name = graph.kind(graph.edge(cur).kind).name;
        // Each road out of the junction but the one come along: its line
        // the way it would be driven, and how straight on it goes.
        let outs: Vec<Out> = graph
            .out(node)
            .filter(|&(_, e, _)| e != cur)
            .map(|(to, edge, back)| {
                let mut line = graph.line(edge);
                if back {
                    line.reverse();
                }
                let n = graph.kind(graph.edge(edge).kind).name;
                Out {
                    to,
                    edge,
                    back,
                    straight: cos(came, way_on(&line)).unwrap_or(-1.0),
                    named: !n.is_empty() && n == name,
                    line,
                }
            })
            .collect();
        let best_by = |keep: fn(&Out) -> bool| {
            outs.iter()
                .filter(|o| keep(o) && o.straight >= ON_COS)
                .max_by(|a, b| a.straight.total_cmp(&b.straight))
        };
        let next = if outs.len() == 1 {
            // The road only goes on: however it bends.
            outs.first()
        } else {
            best_by(|o| o.named).or_else(|| best_by(|_| true))
        };
        let Some(o) = next else {
            full = false;
            break;
        };
        if runs.iter().any(|r| r.0 == o.edge) {
            // Round a loop: as far as it goes.
            full = false;
            break;
        }
        let from = pts.len() - 1;
        pts.extend_from_slice(&o.line[1..]);
        runs.push((
            o.edge,
            !o.back,
            0,
            graph.edge(o.edge).dm,
            from,
            pts.len() - 1,
        ));
        node = o.to;
        cur = o.edge;
        len = path_m(&pts);
    }
    if len < LOOK_M {
        full = false;
    }
    let mut cum = Vec::with_capacity(pts.len());
    let mut m = 0.0;
    cum.push(0.0);
    for w in pts.windows(2) {
        m += ground_px(w[0], w[1]);
        cum.push(m);
    }
    let mut cameras: Vec<OnRoad> = Vec::new();
    for &(e, towards_b, lo, hi, from, to) in &runs {
        for c in cams.on(e) {
            let watches = if towards_b { c.forward } else { c.backward };
            if !watches || !(lo..=hi).contains(&c.from_a_dm) {
                continue;
            }
            // Where it is along the road ahead: on the piece of its edge
            // that passes nearest.
            let along = (from..to)
                .map(|i| {
                    let p = nearest_on(pts[i], pts[i + 1], (c.x, c.y));
                    (ground_px(p, (c.x, c.y)), cum[i] + ground_px(pts[i], p))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map_or(cum[from], |(_, m)| m);
            cameras.push(OnRoad {
                metres: along,
                id: (c.edge, c.from_a_dm),
                at: (c.x, c.y),
                limit: c.limit,
            });
        }
    }
    cameras.sort_by(|a, b| a.metres.total_cmp(&b.metres));
    Some(RoadAhead {
        pts,
        cum,
        cameras,
        full,
    })
}

/// A camera warned of: which, where, when.
#[derive(Clone, Copy, Debug)]
struct Told {
    id: (u32, u32),
    at: (i64, i64),
    when: u64,
}

/// The speed cameras on the road ahead, without a route: which comes next,
/// and each warned of once.
#[derive(Clone, Debug, Default)]
pub struct CameraLookout {
    ahead: Option<RoadAhead>,
    /// Where on `ahead` it was found.
    found_at: f64,
    /// When the road ahead was last looked for and not found.
    missed_at: Option<u64>,
    /// Found afresh: it counts from this far along it.
    counts_from: Option<f64>,
    told: Vec<Told>,
}

impl CameraLookout {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next camera on the road ahead of `fix`, when there is one; the
    /// road ahead found anew when `fix` left it or nears its end.
    /// At most once every `MISSED_MS` while it is not found.
    pub fn look(&mut self, router: &Router, fix: &Fix, now_ms: u64) -> Option<CameraAhead> {
        let cams = router.camera_list().filter(|c| !c.is_empty())?;
        let moving = fix.kmh >= MOVING_KMH && fix.heading != (0, 0);
        let finds = moving && fix.kmh >= FIND_KMH;
        let pos = (fix.x, fix.y);
        let heading = moving.then_some(fix.heading);
        let here = self.ahead.as_ref().and_then(|a| {
            let m = a.locate(pos, heading)?;
            let left = a.len_m() - m;
            let stale = if a.full {
                left < RELOOK_M
            } else {
                m - self.found_at > RELOOK_SHORT_M
            };
            Some((m, stale))
        });
        let along = match here {
            Some((m, false)) => m,
            // Slow: the road ahead kept as it is.
            Some((m, true)) if !finds => m,
            None if !finds => {
                self.ahead = None;
                return None;
            }
            _ => {
                if self
                    .missed_at
                    .is_some_and(|at| now_ms.saturating_sub(at) < MISSED_MS)
                {
                    self.ahead = None;
                    return None;
                }
                self.ahead = road_ahead(router.graph(), cams, pos, fix.heading);
                self.found_at = 0.0;
                self.missed_at = self.ahead.is_none().then_some(now_ms);
                // On from the last road ahead: it counts at once.
                self.counts_from = here.is_none().then_some(CONFIRM_M);
                0.0
            }
        };
        if self.counts_from.is_some_and(|m| along < m) {
            return None;
        }
        self.counts_from = None;
        let a = self.ahead.as_ref()?;
        // A camera just passed (by less than a position's error) is passed.
        let c = a.cameras.iter().find(|c| c.metres >= along - 5.0)?;
        Some(CameraAhead {
            metres: (c.metres - along).max(0.0) as u32,
            limit: c.limit,
            id: c.id,
            at: c.at,
        })
    }

    /// True when "speed camera ahead" is due now for `camera` at `kmh`:
    /// once it is within [`camera_lead`], and once a camera (or cameras
    /// close together).
    pub fn due(&mut self, camera: &CameraAhead, kmh: u32, now_ms: u64) -> bool {
        self.told
            .retain(|t| now_ms.saturating_sub(t.when) < TOLD_MS);
        if camera.metres > camera_lead(kmh) {
            return false;
        }
        let told = self
            .told
            .iter()
            .any(|t| t.id == camera.id || ground_px(t.at, camera.at) <= TOLD_NEAR_M);
        // Each kept, so that the one beside it is told too.
        if !self.told.iter().any(|t| t.id == camera.id) {
            self.told.push(Told {
                id: camera.id,
                at: camera.at,
                when: now_ms,
            });
        }
        !told
    }
}

/// The cosine of the angle between two ways; None when one has no length.
fn cos(a: (i64, i64), b: (i64, i64)) -> Option<f64> {
    let (ax, ay, bx, by) = (a.0 as f64, a.1 as f64, b.0 as f64, b.1 as f64);
    let n = ax.hypot(ay) * bx.hypot(by);
    (n > 0.0).then(|| (ax * bx + ay * by) / n)
}

/// The way the line ends: from `WAY_M` back (or its start) to its end.
fn way_back(pts: &[(i64, i64)]) -> (i64, i64) {
    let end = *pts.last().unwrap_or(&(0, 0));
    let from = pts
        .iter()
        .rev()
        .find(|&&p| ground_px(p, end) >= WAY_M)
        .or(pts.first())
        .copied()
        .unwrap_or(end);
    (end.0 - from.0, end.1 - from.1)
}

/// The way a line sets off: from its start to `WAY_M` along (or its end).
fn way_on(line: &[(i64, i64)]) -> (i64, i64) {
    let start = *line.first().unwrap_or(&(0, 0));
    let to = line
        .iter()
        .find(|&&p| ground_px(p, start) >= WAY_M)
        .or(line.last())
        .copied()
        .unwrap_or(start);
    (to.0 - start.0, to.1 - start.1)
}

fn path_m(pts: &[(i64, i64)]) -> f64 {
    pts.windows(2).map(|w| ground_px(w[0], w[1])).sum()
}

/// The point of the segment a-b nearest p.
fn nearest_on(a: (i64, i64), b: (i64, i64), p: (i64, i64)) -> (i64, i64) {
    let (dx, dy) = ((b.0 - a.0) as f64, (b.1 - a.1) as f64);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return a;
    }
    let f = (((p.0 - a.0) as f64 * dx + (p.1 - a.1) as f64 * dy) / len2).clamp(0.0, 1.0);
    (a.0 + (dx * f) as i64, a.1 + (dy * f) as i64)
}
