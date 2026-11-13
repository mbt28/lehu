// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A drive the map follows by itself, in step with the panel: where it is
//! is worked out for each picture shown, not handed over by the loop, so
//! the map moves the same distance every refresh while it moves.

use std::sync::Arc;
use std::time::Instant;

use crate::track::Track;
use crate::view::{View, WORLD_Z};

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

/// The map held still while the arrow moves on it (`pointer_lead`): until
/// the arrow is `lead` pixels from its place, or the way up has turned
/// `HOLD_TURN_DEG`, then it glides after the arrow for `GLIDE_MS` and
/// holds again. The turn is a little less than a frame shown meanwhile may
/// be off (8 degrees), so the frame held is shown while the new one is
/// turned.
pub const HOLD_TURN_DEG: f64 = 6.0;
pub const GLIDE_MS: u64 = 400;

#[derive(Clone, Copy, Debug, Default)]
pub struct Hold {
    /// The view held, and the way up with it.
    view: Option<View>,
    up: Option<(i32, i32)>,
    /// Gliding after the arrow: from where, since when.
    glide: Option<(View, u64)>,
}

impl Hold {
    /// The view and the way up the map shows for the drive at `car` going
    /// `up`, `now_ms`; `lead` 0: the drive's own.
    pub fn step(
        &mut self,
        car: View,
        up: Option<(i32, i32)>,
        lead: u32,
        now_ms: u64,
    ) -> (View, Option<(i32, i32)>) {
        let held = match (self.view, lead) {
            (Some(v), 1..) if v.zoom == car.zoom => v,
            // Off, the first look, or another zoom: the drive's.
            _ => {
                *self = Self {
                    view: (lead > 0).then_some(car),
                    up,
                    glide: None,
                };
                return (car, up);
            }
        };
        if let Some((from, since)) = self.glide {
            let t = now_ms.saturating_sub(since) as f64 / GLIDE_MS as f64;
            if t < 1.0 {
                let e = t * t * (3.0 - 2.0 * t);
                let lerp = |a: i64, b: i64| a + ((b - a) as f64 * e) as i64;
                let v = View {
                    zoom: car.zoom,
                    x: lerp(from.x, car.x),
                    y: lerp(from.y, car.y),
                };
                return (v, self.up);
            }
            self.glide = None;
            self.view = Some(car);
            return (car, self.up);
        }
        let s = WORLD_Z - car.zoom.min(WORLD_Z);
        let (dx, dy) = ((car.x - held.x) >> s, (car.y - held.y) >> s);
        let away = ((dx * dx + dy * dy) as f64).sqrt();
        let turned = match (self.up, up) {
            (Some(a), Some(b)) => angle(a, b) > HOLD_TURN_DEG,
            (None, Some(_)) => true,
            _ => false,
        };
        if away > f64::from(lead) || turned {
            self.glide = Some((held, now_ms));
            if up.is_some() {
                self.up = up;
            }
        }
        (held, self.up)
    }
}

/// The angle between two ways, degrees.
fn angle(a: (i32, i32), b: (i32, i32)) -> f64 {
    let (a, b) = (
        f64::from(a.1).atan2(f64::from(a.0)),
        f64::from(b.1).atan2(f64::from(b.0)),
    );
    let d = (b - a).rem_euclid(std::f64::consts::TAU);
    d.min(std::f64::consts::TAU - d).to_degrees()
}

/// The view the map shows while following: the drive's position and way
/// up, smoothed, so that the picture never jerks where the track does (a
/// position from a GPS is a straight piece a second, its heading a jump
/// at each fix). Each follows the drive like a critically damped spring:
/// it starts and stops gently and never overshoots. The arrow keeps the
/// drive's own position and way: it may sit a little off its place in a
/// bend, and turn before the map does.
#[derive(Clone, Copy, Debug, Default)]
pub struct Camera {
    /// Where the view is, world pixels, and how fast it moves (a second).
    at: Option<(f64, f64)>,
    vel: (f64, f64),
    /// The way up, radians from x right towards y down, and how fast it
    /// turns (a second).
    up: Option<f64>,
    turn: f64,
}

/// How long the view takes, about, to catch up with the drive: its
/// position, and its way up.
/// A spring settling in `t` trails a steady move by about `t`. The map
/// turns slowly on purpose, slower than the arrow (the owner, 2026-11-13):
/// the arrow shows the car's way at once, the map comes round after it.
pub const CAMERA_POS_S: f64 = 0.25;
pub const CAMERA_UP_S: f64 = 0.6;
/// The way up turns no faster than this, degrees a second: a right-angle
/// corner comes round over two seconds and a half, however the fixes
/// jump; a whole frame turned every 35 ms on the board then steps by
/// under a degree and a half.
pub const CAMERA_MAX_DPS: f64 = 40.0;
/// Further than this from the drive, world pixels (about 250 m at 47
/// degrees north), the view jumps to it: a drive begun again, a jump.
const CAMERA_JUMP_PX: f64 = 16_000.0;

impl Camera {
    /// How fast the way up is turning, degrees a second (signed).
    pub fn turn_dps(&self) -> f64 {
        self.turn.to_degrees()
    }

    /// The view's point and way up (about 1024 long) `dt_ms` on, following
    /// the drive at `pos` going `heading` (any length; (0, 0) standing).
    pub fn step(
        &mut self,
        pos: (i64, i64),
        heading: (i32, i32),
        dt_ms: u64,
    ) -> ((i64, i64), Option<(i32, i32)>) {
        use std::f64::consts::{PI, TAU};
        let dt = (dt_ms.min(500) as f64) / 1000.0;
        let target = (pos.0 as f64, pos.1 as f64);
        let at = match self.at {
            Some(a) if dt > 0.0 && (a.0 - target.0).hypot(a.1 - target.1) < CAMERA_JUMP_PX => {
                let (x, vx) = damp(a.0, target.0, self.vel.0, CAMERA_POS_S, dt);
                let (y, vy) = damp(a.1, target.1, self.vel.1, CAMERA_POS_S, dt);
                self.vel = (vx, vy);
                (x, y)
            }
            _ => {
                self.vel = (0.0, 0.0);
                target
            }
        };
        self.at = Some(at);
        if heading != (0, 0) {
            let want = f64::from(heading.1).atan2(f64::from(heading.0));
            self.up = Some(match self.up {
                Some(a) if dt > 0.0 => {
                    let off = (want - a + PI).rem_euclid(TAU) - PI;
                    if off.abs() > 2.0 * PI / 3.0 {
                        // Turned round: at once.
                        self.turn = 0.0;
                        want
                    } else {
                        let (mut u, mut t) = damp(a, a + off, self.turn, CAMERA_UP_S, dt);
                        let max = CAMERA_MAX_DPS.to_radians();
                        if t.abs() > max {
                            t = max.copysign(t);
                            u = a + t * dt;
                        }
                        self.turn = t;
                        u
                    }
                }
                _ => {
                    self.turn = 0.0;
                    want
                }
            });
        }
        let up = self.up.map(|a| {
            (
                (a.cos() * 1024.0).round() as i32,
                (a.sin() * 1024.0).round() as i32,
            )
        });
        ((at.0.round() as i64, at.1.round() as i64), up)
    }
}

/// One step of a critically damped spring from `cur` (moving at `vel`)
/// towards `target`, settling in about `time` seconds: the new value and
/// velocity.
fn damp(cur: f64, target: f64, vel: f64, time: f64, dt: f64) -> (f64, f64) {
    let omega = 2.0 / time;
    let x = omega * dt;
    let decay = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x);
    let change = cur - target;
    let temp = (vel + omega * change) * dt;
    (
        target + (change + temp) * decay,
        (vel - omega * temp) * decay,
    )
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
    fn the_map_holds_still_until_the_arrow_is_far_then_glides_after_it() {
        let north = Some((0, -1024));
        // Zoom 17: a pixel is 2^5 world pixels.
        let at = |px: i64| View {
            zoom: 17,
            x: 1 << 30,
            y: (1 << 30) - (px << 5),
        };
        let mut hold = Hold::default();
        // Off: the drive's own view.
        assert_eq!(hold.step(at(10), north, 0, 0), (at(10), north));
        // On: held where it was while the arrow moves 80 px.
        assert_eq!(hold.step(at(0), north, 80, 0).0, at(0));
        assert_eq!(hold.step(at(50), north, 80, 1000).0, at(0));
        assert_eq!(hold.step(at(80), north, 80, 2000).0, at(0));
        // Past it: glides after the arrow, eased, and holds there.
        assert_eq!(hold.step(at(81), north, 80, 3000).0, at(0));
        let mid = hold.step(at(90), north, 80, 3200).0;
        assert!(at(0).y > mid.y && mid.y > at(90).y, "{mid:?}");
        assert_eq!(hold.step(at(100), north, 80, 3400).0, at(100));
        assert_eq!(hold.step(at(150), north, 80, 4000).0, at(100));
        // The road turns 10 degrees: glides, and turns the map with it.
        let turned = Some((178, -1008));
        let (_, up) = hold.step(at(160), turned, 80, 5000);
        assert_eq!(up, turned);
        // Another zoom: the drive's at once.
        let closer = View {
            zoom: 18,
            ..at(170)
        };
        assert_eq!(hold.step(closer, turned, 80, 5100), (closer, turned));
    }

    #[test]
    fn the_view_follows_the_drive_smoothly_without_overshooting() {
        let east = (1024, 0);
        let mut c = Camera::default();
        // The first look: where the drive is.
        assert_eq!(c.step((1000, 0), east, 0), ((1000, 0), Some(east)));
        // A step of the drive: the view moves towards it, not all the way,
        // and never past it.
        let mut x = 1000;
        for _ in 0..60 {
            let ((nx, _), _) = c.step((1100, 0), east, 16);
            assert!(nx >= x && nx <= 1100, "{nx}");
            x = nx;
        }
        assert!((1100 - x).abs() <= 1, "settled at {x}");
        // A turn of 90 degrees: no faster than the cap, so all of it in
        // four seconds, never past it.
        let south = (0, 1024);
        let mut last = 0.0f64;
        for i in 0..250 {
            let (_, up) = c.step((1100, 0), south, 16);
            let (ux, uy) = up.unwrap();
            let a = f64::from(uy).atan2(f64::from(ux)).to_degrees();
            assert!(a >= last - 0.01 && a <= 90.01, "{i}: {a}");
            assert!(
                a <= (i + 1) as f64 * 16.0 / 1000.0 * CAMERA_MAX_DPS + 0.5,
                "{i}: {a} too fast"
            );
            assert!(c.turn_dps().abs() <= CAMERA_MAX_DPS + 0.01);
            last = a;
        }
        assert!(last > 89.0, "{last}");
        // Far away (a drive begun again): there at once.
        assert_eq!(c.step((900_000, 0), south, 16).0, (900_000, 0));
        // Turned round: at once.
        assert_eq!(c.step((900_000, 0), (0, -1024), 16).1, Some((0, -1024)));
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
