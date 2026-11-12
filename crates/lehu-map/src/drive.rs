// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A drive the map follows by itself, in step with the panel: where it is
//! is worked out for each picture shown, not handed over by the loop, so
//! the map moves the same distance every refresh while it moves.

use std::sync::Arc;
use std::time::Instant;

use crate::track::Track;

/// A recorded drive, replayed from `start` (its time 0) and from the
/// start again past its end.
#[derive(Clone)]
pub struct Drive {
    pub track: Arc<Track>,
    pub start: Instant,
    /// Where the map may look: the tile set's area.
    pub bounds: [i64; 4],
    /// The closest zoom the map has.
    pub top_zoom: u8,
}

/// Closer than this to the next turn, metres, the map following the drive
/// zooms in a level.
pub const NEAR_TURN_M: u32 = 250;

/// Following a drive, the map is closer or further out by its speed: in
/// town a level closer than the zoom chosen, so it moves visibly (the
/// window moves in whole pixels: at zoom 16 and 50 km/h, nine a second),
/// and on a motorway a level further out, to see further ahead. Below
/// `SLOW_KMH` closer, above `FAST_KMH` further out; a change needs the
/// speed `MARGIN_KMH` past the boundary for `HOLD_MS`, so a speed about a
/// boundary does not flick the zoom to and fro.
pub const SLOW_KMH: u32 = 60;
pub const FAST_KMH: u32 = 100;
const MARGIN_KMH: u32 = 5;
const HOLD_MS: u64 = 3000;

/// The zoom's offset by speed, as it settles.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpeedZoom {
    /// The offset now; None before the first speed.
    offset: Option<i8>,
    /// Another offset the speed asks for, and since when.
    asked: Option<(i8, u64)>,
}

impl SpeedZoom {
    /// The offset to the chosen zoom at `kmh`, `now_ms`: +1, 0 or -1.
    pub fn step(&mut self, kmh: u32, now_ms: u64) -> i8 {
        let Some(offset) = self.offset else {
            let first = band(kmh);
            self.offset = Some(first);
            return first;
        };
        // Near a boundary: no change asked.
        let want = clear_band(kmh).unwrap_or(offset);
        if want == offset {
            self.asked = None;
            return offset;
        }
        match self.asked {
            Some((asked, since)) if asked == want => {
                if now_ms.saturating_sub(since) >= HOLD_MS {
                    self.offset = Some(want);
                    self.asked = None;
                    return want;
                }
            }
            _ => self.asked = Some((want, now_ms)),
        }
        offset
    }
}

/// The offset for `kmh`: +1 slow, 0 between, -1 fast.
fn band(kmh: u32) -> i8 {
    if kmh < SLOW_KMH {
        1
    } else if kmh <= FAST_KMH {
        0
    } else {
        -1
    }
}

/// `band`, when `kmh` is clear of both boundaries by `MARGIN_KMH`.
fn clear_band(kmh: u32) -> Option<i8> {
    let near = |b: u32| kmh.abs_diff(b) <= MARGIN_KMH;
    (!near(SLOW_KMH) && !near(FAST_KMH)).then(|| band(kmh))
}

/// The way up while following: the drive's heading, turned towards by how
/// long it has been (45% of the way each 200 ms), so a bend turns the map
/// over about a second; at once when the drive reverses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Steer {
    /// The way up, as an angle (radians, from x right towards y down).
    up: Option<f64>,
}

impl Steer {
    /// The way up `since` ms after the last step (0: at once), towards
    /// `heading` (x right, y down, any length), about 1024 long; None
    /// before the drive has moved. The angle closes in on the heading's
    /// (by 45% each 200 ms however the time is cut up); a heading more
    /// than 120 degrees off is taken at once.
    pub fn step(&mut self, heading: (i32, i32), since: u64) -> Option<(i32, i32)> {
        use std::f64::consts::{PI, TAU};
        if heading != (0, 0) {
            let target = f64::from(heading.1).atan2(f64::from(heading.0));
            self.up = Some(match self.up {
                Some(a) if since > 0 => {
                    let off = (target - a + PI).rem_euclid(TAU) - PI;
                    if off.abs() > 2.0 * PI / 3.0 {
                        target
                    } else {
                        let keep = 0.55f64.powf(since.min(1000) as f64 / 200.0);
                        a + off * (1.0 - keep)
                    }
                }
                _ => target,
            });
        }
        self.up.map(|a| {
            (
                (a.cos() * 1024.0).round() as i32,
                (a.sin() * 1024.0).round() as i32,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zoom_follows_the_speed_and_settles() {
        let mut z = SpeedZoom::default();
        // In town at once: a level closer.
        assert_eq!(z.step(45, 0), 1);
        // Onto a motorway: further out once the speed has held 3 s.
        assert_eq!(z.step(120, 1000), 1);
        assert_eq!(z.step(120, 3999), 1);
        assert_eq!(z.step(120, 4000), -1);
        // About a boundary (100 km/h, give or take 5): no change at all.
        for (i, kmh) in [97, 103, 99, 104, 96, 101].into_iter().enumerate() {
            assert_eq!(z.step(kmh, 5000 + 2000 * i as u64), -1, "{kmh}");
        }
        // Clearly below it for 3 s: the zoom chosen.
        assert_eq!(z.step(90, 20_000), -1);
        assert_eq!(z.step(90, 23_000), 0);
        // A dip that does not last changes nothing.
        assert_eq!(z.step(40, 24_000), 0);
        assert_eq!(z.step(70, 25_000), 0);
        assert_eq!(z.step(40, 26_000), 0);
        assert_eq!(z.step(40, 29_000), 1);
        // Stopped at a light: still closer.
        assert_eq!(z.step(0, 40_000), 1);
    }

    #[test]
    fn steering_turns_by_time_not_by_steps() {
        let north = (0, -1024);
        let east = (1024, 0);
        // Five steps of 40 ms turn as far as one of 200 ms.
        let mut a = Steer::default();
        a.step(north, 0);
        let one = a.step(east, 200).unwrap();
        let mut b = Steer::default();
        b.step(north, 0);
        let mut five = (0, 0);
        for _ in 0..5 {
            five = b.step(east, 40).unwrap();
        }
        assert!(
            (one.0 - five.0).abs() <= 2 && (one.1 - five.1).abs() <= 2,
            "{one:?} {five:?}"
        );
        // Reversing snaps.
        assert_eq!(a.step((0, 1024), 40), Some((0, 1024)));
    }
}
