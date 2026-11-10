// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A recorded drive, replayed as the position until there is a real one: a
//! GPX track (scripts/route-gpx.py writes one), its points timed, followed
//! in real time and from the start again at its end.

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
}

/// The drive: points in world pixels at `WORLD_Z`, each with its time in
/// milliseconds from the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    points: Vec<(i64, i64, u64)>,
}

impl Track {
    /// A GPX file's track points, in order. A point's `<time>` times it;
    /// a track without times is driven at a point a second.
    pub fn parse_gpx(text: &str) -> Result<Self, String> {
        let mut points = Vec::new();
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
        Ok(Self { points })
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
        Fix {
            x,
            y,
            heading: self.heading(i),
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
