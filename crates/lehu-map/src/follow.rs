// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A route followed by a position from elsewhere: a phone's, or a drive
//! replayed in its place. Each position is placed on the route, near where
//! the last one was and going its way; the guidance is then the route's at
//! that place. One that stays off the route while moving means the driver
//! took another road: the route is to be found anew from there.

use crate::track::{Fix, Track, ground_px};

/// Further off the route than this, metres, is off it: a phone's position
/// is good to 5 to 15 m, and most roads beside each other are further
/// apart.
pub const OFF_M: f64 = 40.0;
/// Off the route this long while moving: the route is found anew. A turn
/// missed is then 50 to 120 m behind, at town speeds.
pub const OFF_MS: u64 = 5000;
/// Within this many metres of the route's end, on it: arrived.
pub const ARRIVE_M: f64 = 30.0;
/// Within this many metres of the destination itself (or the route's end)
/// in a straight line, from any side: arrived too. The route ends on the
/// road nearest the destination, which may not be the one the driver
/// takes to it.
pub const ARRIVE_NEAR_M: f64 = 50.0;
/// Within this many metres of the destination, off the route is not
/// found anew: the last streets are the driver's choice.
const NEAR_GOAL_M: f64 = 150.0;
/// Going the wrong way along the route this long while moving: found anew.
pub const WRONG_WAY_MS: u64 = 8000;
/// Slower than this, km/h, is standing: its heading is not trusted, and
/// its time off the route does not count (a position drifts at a stop).
const MOVING_KMH: u32 = 5;
/// The steps looked at first: a few behind the last place, and about two
/// minutes of the drive ahead of it.
const BEHIND: usize = 3;
const AHEAD: usize = 150;

/// What the last position said about the route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Following {
    /// On it (or off it only for a moment, or standing).
    On,
    /// Off it for `OFF_MS` while moving: find the route anew.
    Off,
    /// At its end.
    Arrived,
}

/// A route followed: where on it the position was last placed.
#[derive(Clone, Debug, Default)]
pub struct Follow {
    step: usize,
    ms: u64,
    off_m: f64,
    off_since: Option<u64>,
    goal: Option<(i64, i64)>,
}

impl Follow {
    /// A route to wherever its end is.
    pub fn new() -> Self {
        Self::default()
    }

    /// A route to `goal` (world pixels at zoom 22): the destination, which
    /// the route ends on the road nearest to.
    pub fn to(goal: (i64, i64)) -> Self {
        Self {
            goal: Some(goal),
            ..Self::default()
        }
    }

    /// Milliseconds into the route where the position was last on it: the
    /// guidance is the route's there.
    pub fn ms(&self) -> u64 {
        self.ms
    }

    /// Metres off the route the position last was.
    pub fn off_m(&self) -> f64 {
        self.off_m
    }

    /// The position `fix` at `now_ms`, placed on `route`.
    pub fn update(&mut self, route: &Track, fix: &Fix, now_ms: u64) -> Following {
        let moving = fix.kmh >= MOVING_KMH && fix.heading != (0, 0);
        let pos = (fix.x, fix.y);
        let end = route.at(route.duration());
        let to_goal = [self.goal, Some((end.x, end.y))]
            .into_iter()
            .flatten()
            .map(|g| ground_px(pos, g))
            .fold(f64::INFINITY, f64::min);
        if route.ends() && to_goal <= ARRIVE_NEAR_M {
            return Following::Arrived;
        }
        // Placed by distance alone, near the last place first; anywhere on
        // the route when not near it (a stretch with no position, a
        // tunnel).
        let near = route
            .locate(
                pos,
                None,
                self.step.saturating_sub(BEHIND),
                self.step + AHEAD,
            )
            .filter(|l| l.off_m <= OFF_M)
            .or_else(|| route.locate(pos, None, 1, route.len()));
        let on = near.filter(|l| l.off_m <= OFF_M);
        self.off_m = near.map_or(f64::INFINITY, |l| l.off_m);
        // Going against the route there: the wrong way, or turned round.
        let wrong_way = on.is_some_and(|l| moving && against(fix.heading, l.way));
        if let Some(l) = on
            && !wrong_way
        {
            self.off_since = None;
            // On ahead only: standing a little behind is not going back.
            if l.ms >= self.ms || l.step + BEHIND < self.step || l.step > self.step + AHEAD {
                (self.step, self.ms) = (l.step, l.ms);
            }
            return if route.ends() && f64::from(route.metres()) - l.metres <= ARRIVE_M {
                Following::Arrived
            } else {
                Following::On
            };
        }
        if !moving || to_goal <= NEAR_GOAL_M {
            return Following::On;
        }
        // Off by distance soon; the wrong way, near the route, only when it
        // lasts (a heading from positions alone swings at a crawl).
        let wait = if wrong_way { WRONG_WAY_MS } else { OFF_MS };
        let since = *self.off_since.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= wait {
            self.off_since = None;
            Following::Off
        } else {
            Following::On
        }
    }
}

/// `heading` goes against `way` by more than 120 degrees (both about 1024
/// long).
fn against(heading: (i32, i32), way: (i32, i32)) -> bool {
    let dot = i64::from(heading.0) * i64::from(way.0) + i64::from(heading.1) * i64::from(way.1);
    way != (0, 0) && dot < -(1024 * 1024 / 2)
}
