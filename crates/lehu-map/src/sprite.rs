// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The marker on the sprite plane, above the map: the map then moves under
//! it by a commit alone, never redrawn for it. One ARGB8888 canvas holds
//! the dot and the arrow in 64 directions, each drawn the first time it is
//! wanted; the plane shows one cell of it.

use lehu_drm::{Canvas, Display, Sprite};

/// A cell a side, in pixels; the marker's middle is at its middle.
pub const CELL: u32 = 48;
/// The arrow's directions, clockwise from up.
const DIRECTIONS: usize = 64;
/// Cells a row of the canvas: the arrows, then the dot.
const COLS: usize = 9;
const ROWS: usize = (DIRECTIONS + 1).div_ceil(COLS);

/// The marker's colours (R, G, B), as `nv12::dot` and `nv12::arrow` have
/// them, and its sizes in pixels.
const FILL: [u8; 3] = [26, 115, 232];
const EDGE: [u8; 3] = [255, 255, 255];
const DOT_R: f64 = 9.0;
const RING_R: f64 = 13.0;
const ARROW_R: f64 = 15.0;
const ARROW_EDGE_R: f64 = 15.0 + 3.0 * 1.6;

pub struct Sprites {
    canvas: Canvas,
    drawn: [bool; DIRECTIONS + 1],
}

impl Sprites {
    /// None without a sprite plane, or the memory for its canvas.
    pub fn new(display: &Display) -> Option<Self> {
        if !display.has_sprite() {
            return None;
        }
        let (w, h) = (COLS as u32 * CELL, ROWS as u32 * CELL);
        match display.sprite_canvas(w, h) {
            Ok(canvas) => Some(Self {
                canvas,
                drawn: [false; DIRECTIONS + 1],
            }),
            Err(e) => {
                log::warn!("map: no memory for the marker's sprite ({e}); drawn into the map");
                None
            }
        }
    }

    /// The marker centred at (`x`, `y`) of the panel: the arrow along
    /// `heading` (x right, y down, any length), or the dot for None or
    /// (0, 0).
    pub fn at(&mut self, heading: Option<(i64, i64)>, (x, y): (i64, i64)) -> Sprite<'_> {
        let cell = match heading {
            Some((hx, hy)) if (hx, hy) != (0, 0) => direction(hx, hy),
            _ => DIRECTIONS,
        };
        if !self.drawn[cell] {
            self.draw(cell);
            self.drawn[cell] = true;
        }
        let half = i64::from(CELL / 2);
        Sprite {
            canvas: &self.canvas,
            sx: (cell % COLS) as u32 * CELL,
            sy: (cell / COLS) as u32 * CELL,
            w: CELL,
            h: CELL,
            x: (x - half).clamp(-(1 << 20), 1 << 20) as i32,
            y: (y - half).clamp(-(1 << 20), 1 << 20) as i32,
        }
    }

    pub fn into_canvas(self) -> Canvas {
        self.canvas
    }

    /// Draw a cell, 16 samples a pixel for smooth edges. The corners in
    /// floating point, the samples in integers (1/64 px): no FPU here.
    fn draw(&mut self, cell: usize) {
        const UNIT: f64 = 64.0;
        let pitch = self.canvas.pitch();
        let (cx0, cy0) = ((cell % COLS) * CELL as usize, (cell / COLS) * CELL as usize);
        let mid = f64::from(CELL) / 2.0;
        let arrow = |r: f64| {
            let t = cell as f64 * std::f64::consts::TAU / DIRECTIONS as f64;
            let (ux, uy) = (t.sin(), -t.cos());
            let at = |along: f64, across: f64| {
                (
                    ((mid + ux * along - uy * across) * UNIT) as i32,
                    ((mid + uy * along + ux * across) * UNIT) as i32,
                )
            };
            let tip = at(r, 0.0);
            let left = at(-0.8 * r, -0.75 * r);
            let right = at(-0.8 * r, 0.75 * r);
            let notch = at(-0.35 * r, 0.0);
            [[tip, left, notch], [tip, notch, right]]
        };
        let (outer, inner) = (arrow(ARROW_EDGE_R), arrow(ARROW_R));
        let is_dot = cell == DIRECTIONS;
        let inside = |tris: &[[(i32, i32); 3]; 2], p: (i32, i32)| {
            let edge = |a: (i32, i32), b: (i32, i32)| {
                (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
            };
            tris.iter().any(|t| {
                let e = [edge(t[0], t[1]), edge(t[1], t[2]), edge(t[2], t[0])];
                e.iter().all(|&v| v >= 0) || e.iter().all(|&v| v <= 0)
            })
        };
        let m = (mid * UNIT) as i32;
        let in_circle = |p: (i32, i32), r: f64| {
            let r = (r * UNIT) as i32;
            (p.0 - m) * (p.0 - m) + (p.1 - m) * (p.1 - m) <= r * r
        };
        let mut row = vec![0u8; CELL as usize * 4];
        for y in 0..CELL as i32 {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (mut out, mut fill) = (0u32, 0u32);
                for s in 0..16 {
                    let p = (x as i32 * 64 + 8 + 16 * (s % 4), y * 64 + 8 + 16 * (s / 4));
                    let (o, f) = if is_dot {
                        (in_circle(p, RING_R), in_circle(p, DOT_R))
                    } else {
                        (inside(&outer, p), inside(&inner, p))
                    };
                    out += u32::from(o || f);
                    fill += u32::from(f);
                }
                *px = if out == 0 {
                    [0; 4]
                } else {
                    // The fill over the edge, by how much of the covered
                    // part is fill; alpha by how much is covered.
                    let mix = |c: usize| {
                        ((u32::from(FILL[c]) * fill + u32::from(EDGE[c]) * (out - fill)) / out)
                            as u8
                    };
                    [mix(2), mix(1), mix(0), ((out * 255 + 8) / 16) as u8]
                };
            }
            let at = (cy0 + y as usize) * pitch + cx0 * 4;
            if let Some(dst) = self.canvas.pixels().get_mut(at..at + row.len()) {
                dst.copy_from_slice(&row);
            }
        }
    }
}

/// The arrow's cell for a heading (x right, y down): the nearest of the
/// directions, clockwise from up.
fn direction(hx: i64, hy: i64) -> usize {
    let t = (hx as f64).atan2(-(hy as f64));
    let k = (t / std::f64::consts::TAU * DIRECTIONS as f64).round() as i64;
    k.rem_euclid(DIRECTIONS as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrow_points_the_nearest_way_of_64() {
        assert_eq!(direction(0, -1024), 0);
        assert_eq!(direction(1024, 0), 16);
        assert_eq!(direction(0, 1024), 32);
        assert_eq!(direction(-1024, 0), 48);
        // Just short of up, the other way round: still up.
        assert_eq!(direction(-10, -1024), 0);
        assert_eq!(direction(724, -724), 8);
    }
}
