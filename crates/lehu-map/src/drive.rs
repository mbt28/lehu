// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A drive the map follows by itself, in step with the panel: where it is
//! is worked out for each picture shown, not handed over by the loop, so
//! the map moves the same distance every two refreshes.

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
