// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The decoder's line every 5 s, in the format FastCarPlay printed so the
//! two binaries compare on the same log:
//!
//! `[Cedrus] in 60 fps, decode 60 fps (avg 7 ms, max 12 ms), shown 59 fps,
//! 1 superseded, 0 dropped, queue peak 3 (5.0s)`
//!
//! No floating point: the span is printed as whole seconds and a tenth.

pub const EVERY_MS: u64 = 5000;

/// Counts over one window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub input: u32,
    pub decoded: u32,
    pub shown: u32,
    pub superseded: u32,
    pub dropped: u32,
    pub decode_us: u64,
    pub decode_us_max: u32,
    pub depth_max: u32,
}

impl Window {
    /// A decode took `us`.
    pub fn decoded(&mut self, us: u32) {
        self.decoded += 1;
        self.decode_us += u64::from(us);
        self.decode_us_max = self.decode_us_max.max(us);
    }

    /// The line for a window of `span_ms`; None when nothing happened.
    pub fn line(&self, span_ms: u64) -> Option<String> {
        if self.input == 0 && self.dropped == 0 {
            return None;
        }
        let span = span_ms.max(1);
        let per_s = |n: u32| u64::from(n) * 1000 / span;
        let avg_ms = if self.decoded > 0 {
            self.decode_us / u64::from(self.decoded) / 1000
        } else {
            0
        };
        Some(format!(
            "[Cedrus] in {} fps, decode {} fps (avg {} ms, max {} ms), shown {} fps, {} superseded, {} dropped, queue peak {} ({}.{}s)",
            per_s(self.input),
            per_s(self.decoded),
            avg_ms,
            self.decode_us_max / 1000,
            per_s(self.shown),
            self.superseded,
            self.dropped,
            self.depth_max,
            span_ms / 1000,
            (span_ms % 1000) / 100,
        ))
    }
}

/// The window and when it began.
pub struct Reporter {
    pub window: Window,
    since_ms: u64,
}

impl Reporter {
    pub fn new(now_ms: u64) -> Self {
        Self {
            window: Window::default(),
            since_ms: now_ms,
        }
    }

    /// Once `EVERY_MS` have passed: the line (if anything happened) and a
    /// fresh window.
    pub fn tick(&mut self, now_ms: u64) -> Option<String> {
        let span = now_ms.saturating_sub(self.since_ms);
        if span < EVERY_MS {
            return None;
        }
        let line = self.window.line(span);
        self.window = Window::default();
        self.since_ms = now_ms;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_has_fastcarplays_shape() {
        let w = Window {
            input: 300,
            decoded: 300,
            shown: 295,
            superseded: 5,
            dropped: 0,
            decode_us: 300 * 7_400,
            decode_us_max: 12_900,
            depth_max: 3,
        };
        assert_eq!(
            w.line(5000).as_deref(),
            Some(
                "[Cedrus] in 60 fps, decode 60 fps (avg 7 ms, max 12 ms), shown 59 fps, 5 superseded, 0 dropped, queue peak 3 (5.0s)"
            )
        );
        assert_eq!(
            w.line(5230).as_deref().map(|l| &l[l.len() - 6..]),
            Some("(5.2s)")
        );
    }

    #[test]
    fn a_quiet_window_prints_nothing_but_drops_alone_do() {
        assert_eq!(Window::default().line(5000), None);
        let w = Window {
            dropped: 2,
            ..Window::default()
        };
        assert!(w.line(5000).is_some_and(|l| l.contains("2 dropped")));
        assert!(w.line(0).is_some(), "a zero span does not divide");
    }

    #[test]
    fn decode_times_accumulate() {
        let mut w = Window::default();
        w.decoded(5_000);
        w.decoded(15_000);
        assert_eq!(w.decoded, 2);
        assert_eq!(w.decode_us_max, 15_000);
        assert!(w.line(1000).is_none(), "no input counted");
        w.input = 2;
        assert!(
            w.line(1000)
                .is_some_and(|l| l.contains("(avg 10 ms, max 15 ms)"))
        );
    }

    #[test]
    fn the_reporter_fires_every_five_seconds_and_resets() {
        let mut r = Reporter::new(1000);
        r.window.input = 10;
        assert_eq!(r.tick(5999), None);
        let line = r.tick(6000).expect("due");
        assert!(line.starts_with("[Cedrus] in 2 fps"));
        assert_eq!(r.window, Window::default());
        assert_eq!(r.tick(11000), None, "nothing happened");
        assert_eq!(r.tick(500), None, "a clock going back fires nothing");
    }
}
