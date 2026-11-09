// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The session's clocks as pure state, so they are tested with a clock of
//! the test's choosing: the wired start's resend schedule with its one USB
//! reset, and the silence watch over a live control connection.

/// After CarPlayStartSession went out, the phone should open the control
/// connection within moments. It is told again at 0.5 s, then every
/// second, ten times in all; a phone that has not come after 8 s has its
/// USB reset, once per plug: a phone stuck between its modes starts over
/// on the reset, and a second reset would only loop.
pub const RESEND_FIRST_MS: u64 = 500;
pub const RESEND_EVERY_MS: u64 = 1000;
pub const RESEND_TRIES: u32 = 10;
pub const STUCK_AFTER_MS: u64 = 8000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing due; the next check is this many milliseconds away.
    Wait(u64),
    /// Send CarPlayStartSession again.
    Resend,
    /// Reset the phone's USB port; the session ends with it.
    Reset,
    /// Every try is spent.
    GiveUp,
}

pub struct StartSchedule {
    started: u64,
    resends: u32,
    next_resend: u64,
    reset_allowed: bool,
    arrived: bool,
}

impl StartSchedule {
    /// Begins when the start message is first sent, at `now`.
    pub fn new(now: u64, reset_allowed: bool) -> Self {
        Self {
            started: now,
            resends: 0,
            next_resend: now + RESEND_FIRST_MS,
            reset_allowed,
            arrived: false,
        }
    }

    /// The control connection came: nothing more is due.
    pub fn arrived(&mut self) {
        self.arrived = true;
    }

    pub fn tick(&mut self, now: u64) -> Action {
        if self.arrived {
            return Action::Wait(u64::MAX);
        }
        if self.reset_allowed && now >= self.started + STUCK_AFTER_MS {
            self.reset_allowed = false;
            return Action::Reset;
        }
        if now >= self.next_resend {
            if self.resends >= RESEND_TRIES {
                return Action::GiveUp;
            }
            self.resends += 1;
            self.next_resend = now + RESEND_EVERY_MS;
            return Action::Resend;
        }
        let mut next = self.next_resend;
        if self.reset_allowed {
            next = next.min(self.started + STUCK_AFTER_MS);
        }
        Action::Wait(next.saturating_sub(now))
    }
}

/// A phone that stops talking over the control channel and stops
/// answering the timing sync is gone, whatever TCP thinks: the session
/// ends after this long without either.
pub const SILENT_AFTER_MS: u64 = 10_000;

#[derive(Debug)]
pub struct Silence {
    last_request: u64,
    last_timing: Option<u64>,
}

impl Silence {
    pub fn new(now: u64) -> Self {
        Self {
            last_request: now,
            last_timing: None,
        }
    }

    pub fn request(&mut self, now: u64) {
        self.last_request = now;
    }

    pub fn timing(&mut self, now: u64) {
        self.last_timing = Some(now);
    }

    /// Only once the timing sync has answered at all: before that the
    /// session is still being set up and has its own timeouts.
    pub fn silent(&self, now: u64) -> bool {
        let Some(t) = self.last_timing else {
            return false;
        };
        now.saturating_sub(t.max(self.last_request)) > SILENT_AFTER_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resends_then_resets_once_then_gives_up() {
        let mut s = StartSchedule::new(1000, true);
        assert_eq!(s.tick(1000), Action::Wait(500));
        assert_eq!(s.tick(1400), Action::Wait(100));
        assert_eq!(s.tick(1500), Action::Resend);
        assert_eq!(s.tick(1500), Action::Wait(1000));
        assert_eq!(s.tick(2500), Action::Resend);
        assert_eq!(s.tick(3500), Action::Resend);
        // At 8 s without a connection the reset comes, before a resend due
        // at the same moment, and only once.
        assert_eq!(s.tick(9000), Action::Reset);
        assert_eq!(s.tick(9000), Action::Resend);
        let mut without = StartSchedule::new(0, false);
        let mut resends = 0;
        let mut t = 0;
        loop {
            match without.tick(t) {
                Action::Resend => resends += 1,
                Action::Wait(ms) => t += ms,
                Action::Reset => panic!("no reset without permission"),
                Action::GiveUp => break,
            }
        }
        assert_eq!(resends, RESEND_TRIES);
        assert_eq!(t, RESEND_FIRST_MS + RESEND_TRIES as u64 * RESEND_EVERY_MS);
    }

    #[test]
    fn an_arrival_ends_the_schedule() {
        let mut s = StartSchedule::new(0, true);
        assert_eq!(s.tick(600), Action::Resend);
        s.arrived();
        assert_eq!(s.tick(20_000), Action::Wait(u64::MAX));
    }

    #[test]
    fn silence_needs_the_timing_sync_first() {
        let mut s = Silence::new(0);
        assert!(!s.silent(60_000));
        s.timing(1000);
        assert!(!s.silent(11_000));
        assert!(s.silent(11_001));
        s.request(11_000);
        assert!(!s.silent(21_000));
        s.timing(30_000);
        assert!(!s.silent(40_000));
        assert!(s.silent(40_001));
    }
}
