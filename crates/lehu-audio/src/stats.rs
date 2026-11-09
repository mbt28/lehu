// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The mixer's line every 5 s while the device is open, in the format
//! FastCarPlay printed so the two binaries compare on the same log:
//!
//! `[Audio] music queued 480-520 ms (buffer 500 ms), 0 dry, 0 ms skipped,
//! 0 underruns, 0 dropped (5.0s)`
//!
//! Gaps are otherwise recovered silently; this is where they show.

pub const EVERY_MS: u64 = 5000;

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    start_ms: u64,
    /// Music ran dry and refilled: a gap heard.
    pub dry: u32,
    /// A backlog skipped to the live edge.
    pub skipped_ms: u32,
    /// The DAC ran out (-EPIPE).
    pub underruns: u32,
    depth_min: u32,
    depth_max: u32,
    /// Queue overflow drops so far, both queues, at the window's start.
    drops: u32,
    fresh: bool,
}

impl Stats {
    pub fn new() -> Self {
        Stats {
            fresh: true,
            ..Default::default()
        }
    }

    /// Start a new window at the next `tick`.
    pub fn reset(&mut self) {
        self.fresh = true;
    }

    /// Once a period, with the music queued now and the queues' drop count:
    /// the line when the window is over, which starts the next one.
    pub fn tick(
        &mut self,
        now_ms: u64,
        depth_ms: u32,
        drops: u32,
        buffer_ms: u32,
    ) -> Option<String> {
        if self.fresh {
            *self = Stats {
                start_ms: now_ms,
                depth_min: depth_ms,
                depth_max: depth_ms,
                drops,
                ..Default::default()
            };
            return None;
        }
        self.depth_min = self.depth_min.min(depth_ms);
        self.depth_max = self.depth_max.max(depth_ms);
        let span = now_ms.saturating_sub(self.start_ms);
        if span < EVERY_MS {
            return None;
        }
        let line = format!(
            "[Audio] music queued {}-{} ms (buffer {} ms), {} dry, {} ms skipped, {} underruns, {} dropped ({}.{}s)",
            self.depth_min,
            self.depth_max,
            buffer_ms,
            self.dry,
            self.skipped_ms,
            self.underruns,
            drops.wrapping_sub(self.drops),
            span / 1000,
            (span % 1000) / 100,
        );
        self.fresh = true;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_keeps_the_format() {
        let mut s = Stats::new();
        assert!(s.tick(1000, 500, 3, 500).is_none());
        s.dry = 1;
        s.skipped_ms = 1200;
        s.underruns = 2;
        assert!(s.tick(3000, 480, 3, 500).is_none());
        assert!(s.tick(5999, 530, 5, 500).is_none());
        let line = s.tick(6100, 510, 7, 500).expect("5 s passed");
        assert_eq!(
            line,
            "[Audio] music queued 480-530 ms (buffer 500 ms), 1 dry, 1200 ms skipped, 2 underruns, 4 dropped (5.1s)"
        );
        // The next window starts clean.
        assert!(s.tick(6200, 100, 7, 300).is_none());
        let line = s.tick(11300, 200, 7, 300).expect("another 5 s");
        assert_eq!(
            line,
            "[Audio] music queued 100-200 ms (buffer 300 ms), 0 dry, 0 ms skipped, 0 underruns, 0 dropped (5.1s)"
        );
    }
}
