// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A recorded drive, replayed as the position until there is a real one: a
//! GPX track (scripts/route-gpx.py writes one), its points timed, followed
//! in real time and from the start again at its end. Its waypoints, when
//! it has them, are the route's turns, so the drive is also a route to be
//! guided along.

use crate::view::world_from_degrees;

/// Where the drive is, and where it heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fix {
    /// World pixels at `WORLD_Z`.
    pub x: i64,
    pub y: i64,
    /// The way it goes, as a vector of length about 1024 (x right, y down);
    /// (0, 0) before it has moved.
    pub heading: (i32, i32),
    /// The track point it is heading for: the route behind it is the
    /// points before this one, then the fix.
    pub step: usize,
    /// Its speed, km/h, and the road's speed limit where the track has it.
    pub kmh: u32,
    pub limit: Option<u16>,
}

/// What to do at a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Straight,
    SlightLeft,
    Left,
    SharpLeft,
    SlightRight,
    Right,
    SharpRight,
    UTurn,
    /// Into a roundabout, out at this exit.
    Roundabout(u8),
    Arrive,
}

impl Turn {
    /// A waypoint's `<type>`, as scripts/route-gpx.py writes it.
    fn parse(text: &str) -> Option<Self> {
        Some(match text.trim() {
            "straight" => Self::Straight,
            "slight-left" => Self::SlightLeft,
            "left" => Self::Left,
            "sharp-left" => Self::SharpLeft,
            "slight-right" => Self::SlightRight,
            "right" => Self::Right,
            "sharp-right" => Self::SharpRight,
            "u-turn" => Self::UTurn,
            "arrive" => Self::Arrive,
            other => Self::Roundabout(other.strip_prefix("roundabout-")?.parse().ok()?),
        })
    }
}

/// A turn on the route: what, onto which road, at which point of the track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Maneuver {
    pub turn: Turn,
    pub road: String,
    /// The track point it is at.
    point: usize,
}

/// The next turn from where the drive is, and how far away things are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ahead<'a> {
    pub maneuver: &'a Maneuver,
    /// Metres to the turn.
    pub metres: u32,
    /// Metres and milliseconds to the end of the route.
    pub left_metres: u32,
    pub left_ms: u64,
}

/// The drive: points in world pixels at `WORLD_Z`, each with its time in
/// milliseconds from the first, the metres along the route to it, and the
/// speed limit of the road it is on; the turns, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    points: Vec<(i64, i64, u64)>,
    metres: Vec<f64>,
    limits: Vec<Option<u16>>,
    maneuvers: Vec<Maneuver>,
}

impl Track {
    /// A GPX file's track points, in order. A point's `<time>` times it;
    /// a track without times is driven at a point a second.
    pub fn parse_gpx(text: &str) -> Result<Self, String> {
        let mut points = Vec::new();
        let mut limits = Vec::new();
        let mut first: Option<i64> = None;
        let mut rest = text;
        while let Some(at) = rest.find("<trkpt") {
            rest = &rest[at + 6..];
            let end = rest.find('>').ok_or("a <trkpt> is cut short")?;
            let attrs = &rest[..end];
            let lat = attr(attrs, "lat").ok_or("a <trkpt> without lat")?;
            let lon = attr(attrs, "lon").ok_or("a <trkpt> without lon")?;
            let (x, y) = world_from_degrees(lat, lon)
                .ok_or_else(|| format!("a <trkpt> off the map: {lat}, {lon}"))?;
            // The point's own time, if it has one before the next point.
            let next = rest.find("<trkpt").unwrap_or(rest.len());
            let stamp = rest[..next]
                .find("<time>")
                .and_then(|t| seconds(&rest[t + 6..next]));
            let ms = match (stamp, first) {
                (Some(s), Some(f)) => u64::try_from(s - f).unwrap_or(0) * 1000,
                (Some(s), None) => {
                    first = Some(s);
                    0
                }
                (None, _) => points.len() as u64 * 1000,
            };
            points.push((x, y, ms));
            // The road's speed limit: LEHU's extension, as route-gpx.py
            // writes it.
            limits.push(element(&rest[..next], "maxspeed").and_then(|v| v.parse().ok()));
        }
        if points.len() < 2 {
            return Err("a track wants two points or more".into());
        }
        // Times only go forward.
        for i in 1..points.len() {
            points[i].2 = points[i].2.max(points[i - 1].2);
        }
        if points[points.len() - 1].2 == 0 {
            return Err("the track takes no time".into());
        }
        let mut metres = Vec::with_capacity(points.len());
        let mut run = 0.0;
        for (i, p) in points.iter().enumerate() {
            if i > 0 {
                run += ground(points[i - 1], *p);
            }
            metres.push(run);
        }
        let maneuvers = waypoints(text, &points)?;
        Ok(Self {
            points,
            metres,
            limits,
            maneuvers,
        })
    }

    /// The route as a line: its points, world pixels at `WORLD_Z`.
    pub fn line(&self) -> Vec<(i64, i64)> {
        self.points.iter().map(|p| (p.0, p.1)).collect()
    }

    /// The turns, in order.
    pub fn maneuvers(&self) -> &[Maneuver] {
        &self.maneuvers
    }

    /// The next turn `ms` after the drive started (as `at` has it), and
    /// what is left of the route; None for a track without turns.
    pub fn ahead(&self, ms: u64) -> Option<Ahead<'_>> {
        let t = ms % self.duration().max(1);
        let i = self
            .points
            .partition_point(|p| p.2 <= t)
            .clamp(1, self.points.len() - 1);
        let (at, bt) = (self.points[i - 1].2, self.points[i].2);
        let f = if bt > at {
            (t - at) as f64 / (bt - at) as f64
        } else {
            1.0
        };
        let here = self.metres[i - 1] + (self.metres[i] - self.metres[i - 1]) * f;
        // The first turn not yet passed: its point is ahead of the step.
        let maneuver = self.maneuvers.iter().find(|m| m.point >= i)?;
        let end = self.metres[self.metres.len() - 1];
        Some(Ahead {
            maneuver,
            metres: (self.metres[maneuver.point] - here).max(0.0) as u32,
            left_metres: (end - here).max(0.0) as u32,
            left_ms: self.duration() - t,
        })
    }

    /// The turn after the one `ahead` names, and the metres between the
    /// two: what a prompt says after "then". None after the last.
    pub fn then(&self, ahead: &Ahead<'_>) -> Option<(&Maneuver, u32)> {
        let i = self
            .maneuvers
            .iter()
            .position(|m| std::ptr::eq(m, ahead.maneuver))?;
        let next = self.maneuvers.get(i + 1)?;
        let gap = self.metres[next.point] - self.metres[ahead.maneuver.point];
        Some((next, gap.max(0.0) as u32))
    }

    /// How long the drive takes, in milliseconds.
    pub fn duration(&self) -> u64 {
        self.points[self.points.len() - 1].2
    }

    /// Where the drive is `ms` after it started, from the start again past
    /// its end.
    pub fn at(&self, ms: u64) -> Fix {
        let t = ms % self.duration().max(1);
        // The step that holds t: the last point at or before it.
        let i = self
            .points
            .partition_point(|p| p.2 <= t)
            .clamp(1, self.points.len() - 1);
        let (ax, ay, at) = self.points[i - 1];
        let (bx, by, bt) = self.points[i];
        let span = bt.saturating_sub(at);
        let (x, y) = if span == 0 {
            (bx, by)
        } else {
            let f = (t - at) as i128;
            let span = span as i128;
            (
                ax + ((i128::from(bx - ax) * f) / span) as i64,
                ay + ((i128::from(by - ay) * f) / span) as i64,
            )
        };
        // The step's speed: its metres over its time.
        let kmh = if span == 0 {
            0
        } else {
            ((self.metres[i] - self.metres[i - 1]) * 3600.0 / span as f64).round() as u32
        };
        Fix {
            x,
            y,
            heading: self.heading(i),
            step: i,
            kmh,
            limit: self.limits[i - 1],
        }
    }

    /// The way the step ending at point `i` goes; a step that does not move
    /// takes the heading of the one before it.
    fn heading(&self, i: usize) -> (i32, i32) {
        for j in (1..=i).rev() {
            let (ax, ay, _) = self.points[j - 1];
            let (bx, by, _) = self.points[j];
            let (dx, dy) = ((bx - ax) as f64, (by - ay) as f64);
            let len = dx.hypot(dy);
            if len > 0.0 {
                return ((dx / len * 1024.0) as i32, (dy / len * 1024.0) as i32);
            }
        }
        (0, 0)
    }
}

/// Metres on the ground between two points (world pixels at `WORLD_Z`):
/// a Mercator pixel is the equator's length over the world's pixels,
/// shrunk by the cosine of the latitude.
fn ground(a: (i64, i64, u64), b: (i64, i64, u64)) -> f64 {
    const EQUATOR: f64 = 40_075_016.686;
    let size = 256.0 * f64::from(1u32 << crate::view::WORLD_Z);
    let y = (a.1 + b.1) as f64 / 2.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / size))
        .sinh()
        .atan();
    let px = ((b.0 - a.0) as f64).hypot((b.1 - a.1) as f64);
    px * EQUATOR / size * lat.cos()
}

/// The GPX's waypoints as the route's turns, each at the track point
/// nearest it, in order along the track.
fn waypoints(text: &str, points: &[(i64, i64, u64)]) -> Result<Vec<Maneuver>, String> {
    let mut out = Vec::new();
    let mut from = 0;
    let mut rest = text;
    while let Some(at) = rest.find("<wpt") {
        rest = &rest[at + 4..];
        let end = rest.find("</wpt>").unwrap_or(rest.len());
        let body = &rest[..end];
        let tag_end = body.find('>').ok_or("a <wpt> is cut short")?;
        let (lat, lon) = (attr(&body[..tag_end], "lat"), attr(&body[..tag_end], "lon"));
        let (Some(lat), Some(lon)) = (lat, lon) else {
            return Err("a <wpt> without lat or lon".into());
        };
        let Some(turn) = element(body, "type").and_then(|t| Turn::parse(&t)) else {
            continue;
        };
        let road = element(body, "name").unwrap_or_default();
        let (x, y) = world_from_degrees(lat, lon)
            .ok_or_else(|| format!("a <wpt> off the map: {lat}, {lon}"))?;
        // The nearest track point from the last turn's on.
        let point = (from..points.len())
            .min_by_key(|&i| {
                let (dx, dy) = (points[i].0 - x, points[i].1 - y);
                i128::from(dx) * i128::from(dx) + i128::from(dy) * i128::from(dy)
            })
            .unwrap_or(points.len() - 1);
        from = point;
        out.push(Maneuver { turn, road, point });
    }
    Ok(out)
}

/// An element's text, its entities read: `<name>Rue &amp; Co</name>`.
fn element(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&format!("</{name}>"))? + start;
    Some(
        body[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
            .trim()
            .to_string(),
    )
}

/// An attribute's number in a tag's text: `name="1.5"`.
fn attr(attrs: &str, name: &str) -> Option<f64> {
    let mut rest = attrs;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().last();
        rest = &rest[at + name.len()..];
        if before.is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let value = rest.trim_start().strip_prefix('=')?.trim_start();
        let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let value = &value[1..];
        return value[..value.find(quote)?].trim().parse().ok();
    }
    None
}

/// An ISO 8601 UTC time, "2026-11-10T12:00:05Z", as seconds since 1970.
fn seconds(text: &str) -> Option<i64> {
    let t = text.trim();
    let num = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

#[cfg(test)]
pub(crate) fn seconds_for_test(text: &str) -> Option<i64> {
    seconds(text)
}
