// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! One monotonic clock for the whole program: milliseconds since it started.
//! Timers and timestamps across threads compare these numbers directly; they
//! never go backwards and will not wrap in the life of a head unit.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the first call (made at start-up), counted from 1, so
/// that 0 is never a time and can mean "never" in a `Deadline`.
pub fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Make the first call, so `now_ms()` counts from here.
pub fn start() {
    now_ms();
}

/// A moment in `now_ms` time that something is due; 0 means never.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deadline(pub u64);

impl Deadline {
    pub const NONE: Deadline = Deadline(0);

    pub fn after_ms(ms: u64) -> Self {
        Deadline(now_ms() + ms)
    }

    pub fn is_set(self) -> bool {
        self.0 != 0
    }

    pub fn due(self) -> bool {
        self.is_set() && now_ms() >= self.0
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_up() {
        let a = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(now_ms() >= a + 4);
    }

    #[test]
    fn deadlines() {
        let mut d = Deadline::NONE;
        assert!(!d.is_set());
        assert!(!d.due());
        d = Deadline::after_ms(0);
        assert!(d.is_set());
        assert!(d.due());
        d = Deadline::after_ms(60_000);
        assert!(!d.due());
        d.clear();
        assert!(!d.is_set());
    }
}
