// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The speeding chime (map.speed-warning): a soft two-tone, made in code
//! once, played through the prompt input of the mixer when the speed stays
//! over the road's limit; and when to play it.
//!
//! It goes through the same input as the spoken prompts, so it waits while
//! the phone speaks, ducks the music, and plays at the navigation level.

use lehu_audio::{Format, Segment};

/// Over the limit by more than this, km/h: a speedometer reads a little
/// high, and a chime at every bump over the limit would be noise.
const OVER_KMH: u32 = 5;
/// For this long, ms, so that passing a sign at speed or a moment's
/// overshoot is not chimed at.
const OVER_MS: u64 = 2000;

/// The mixer's rate: the chime needs no resampling there.
const RATE: u32 = 48_000;
/// The two notes, Hz, and how long each lasts, ms: a falling fifth, as a
/// door chime, short enough not to be mistaken for a prompt.
const NOTES: [(u32, u32); 2] = [(880, 120), (587, 130)];
/// Each note rises over this long, then fades to nothing by its end.
const ATTACK_MS: u32 = 4;
/// The peak, of full scale: soft, under the spoken prompts' 0.7.
const PEAK: i64 = 11_000;

/// When to chime: once a stretch over the limit, again only after the
/// speed came back to the limit or the limit changed.
#[derive(Debug, Default)]
pub struct SpeedWatch {
    /// Since when the speed is over (by more than `OVER_KMH`).
    over_since: Option<u64>,
    /// This stretch has had its chime.
    chimed: bool,
    /// The limit the stretch is over.
    limit: Option<u16>,
}

impl SpeedWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// With each new position: the speed, km/h, and the road's limit where
    /// it is known, at `now_ms`. True once when the chime is due.
    pub fn tick(&mut self, now_ms: u64, kmh: u32, limit: Option<u16>) -> bool {
        if limit != self.limit {
            // Another limit: a stretch of its own.
            self.limit = limit;
            self.over_since = None;
            self.chimed = false;
        }
        let Some(limit) = limit.map(u32::from) else {
            return false;
        };
        if kmh <= limit {
            // Back to the limit: the next stretch over it chimes again.
            self.over_since = None;
            self.chimed = false;
            return false;
        }
        if kmh <= limit + OVER_KMH {
            // A little over: not yet a stretch, nor the end of one.
            self.over_since = None;
            return false;
        }
        let since = *self.over_since.get_or_insert(now_ms);
        if !self.chimed && now_ms.saturating_sub(since) >= OVER_MS {
            self.chimed = true;
            return true;
        }
        false
    }
}

/// The chime's samples: mono at the mixer's rate, about a quarter of a
/// second. Made once; each note a sine from a two-term recurrence in
/// fixed point (two cosines for its constant, worked out here once, the
/// samples in integers).
pub fn samples() -> Vec<i16> {
    let total: u32 = NOTES.iter().map(|&(_, ms)| RATE * ms / 1000).sum();
    let mut out = Vec::with_capacity(total as usize);
    let attack = (RATE * ATTACK_MS / 1000) as i64;
    for (hz, ms) in NOTES {
        let n = i64::from(RATE * ms / 1000);
        let w = std::f64::consts::TAU * f64::from(hz) / f64::from(RATE);
        // y[i] = k y[i-1] - y[i-2], k = 2 cos w in Q30; y in Q16.
        let k = (2.0 * w.cos() * f64::from(1u32 << 30)) as i64;
        let (mut y2, mut y1) = (0i64, (w.sin() * (PEAK << 16) as f64) as i64);
        for i in 0..n {
            let envelope = if i < attack {
                (i << 15) / attack
            } else {
                ((n - i) << 15) / (n - attack)
            };
            out.push((((y2 >> 16) * envelope) >> 15) as i16);
            let y = ((k * y1) >> 30) - y2;
            (y2, y1) = (y1, y);
        }
    }
    out
}

/// The chime as the mixer takes it.
pub fn segment(samples: &[i16]) -> Segment {
    Segment::from_slice(Format::new(RATE, 1), samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticks every 200 ms from `from` to `to` ms at a steady speed: the
    /// times a chime was due.
    fn run(w: &mut SpeedWatch, from: u64, to: u64, kmh: u32, limit: Option<u16>) -> Vec<u64> {
        (from..to)
            .step_by(200)
            .filter(|&t| w.tick(t, kmh, limit))
            .collect()
    }

    #[test]
    fn a_stretch_over_the_limit_chimes_once_after_two_seconds() {
        let mut w = SpeedWatch::new();
        assert!(
            run(&mut w, 0, 5000, 50, Some(50)).is_empty(),
            "at the limit"
        );
        // A little over is within a speedometer's error.
        assert!(run(&mut w, 5000, 10_000, 55, Some(50)).is_empty());
        // Over by more: once, two seconds in, then quiet however long.
        assert_eq!(run(&mut w, 10_000, 30_000, 58, Some(50)), [12_000]);
        // Easing to a little over does not end the stretch.
        assert!(run(&mut w, 30_000, 32_000, 54, Some(50)).is_empty());
        assert!(run(&mut w, 32_000, 40_000, 60, Some(50)).is_empty());
        // Back to the limit re-arms it.
        assert!(run(&mut w, 40_000, 41_000, 50, Some(50)).is_empty());
        assert_eq!(run(&mut w, 41_000, 50_000, 60, Some(50)), [43_000]);
    }

    #[test]
    fn a_moment_over_is_not_chimed_at() {
        let mut w = SpeedWatch::new();
        assert!(run(&mut w, 0, 1800, 70, Some(50)).is_empty());
        assert!(run(&mut w, 1800, 2400, 50, Some(50)).is_empty());
        assert!(run(&mut w, 2400, 4200, 70, Some(50)).is_empty());
    }

    #[test]
    fn a_new_limit_is_a_new_stretch_and_no_limit_no_chime() {
        let mut w = SpeedWatch::new();
        assert_eq!(run(&mut w, 0, 5000, 80, Some(50)), [2000]);
        // Into a 30 zone at the same speed: chimed again.
        assert_eq!(run(&mut w, 5000, 10_000, 80, Some(30)), [7000]);
        // A road with no known limit says nothing.
        assert!(run(&mut w, 10_000, 20_000, 150, None).is_empty());
        // And coming back to a known one starts afresh.
        assert_eq!(run(&mut w, 20_000, 25_000, 80, Some(30)), [22_000]);
    }

    #[test]
    fn the_chime_is_short_soft_and_ends_in_silence() {
        let s = samples();
        let ms = s.len() as u32 * 1000 / RATE;
        assert!((240..=260).contains(&ms), "{ms} ms");
        let peak = s.iter().map(|v| i32::from(*v).abs()).max().unwrap();
        assert!((9000..=12_000).contains(&peak), "peak {peak}");
        assert_eq!(s[0], 0);
        assert!(s[s.len() - 1].abs() < 200, "fades out");
        // Both notes sound: loud in the middle of each.
        let first = RATE as usize * 60 / 1000;
        let second = RATE as usize * (120 + 40) / 1000;
        for at in [first, second] {
            let loud = s[at..at + 200].iter().map(|v| v.abs()).max().unwrap();
            assert!(loud > 3000, "at {at}: {loud}");
        }
        let seg = segment(&s);
        assert_eq!(seg.format, Format::new(RATE, 1));
        assert_eq!(seg.len(), s.len());
    }
}
