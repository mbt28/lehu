// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The mouse pointer on the display's cursor plane, as the loop keeps it:
//! shown where the mouse last pointed, gone after `HIDE_MS` without a move
//! when auto-hide is on, and committed again a moment later when the
//! display was busy with a flip. While it shows, the map's marker keeps off
//! the cursor (`AppHost::map_marker_plane`).

use lehu_drm::Display;

/// Auto-hide: the pointer goes after this long without a move or a press.
pub const HIDE_MS: u64 = 3000;
/// A commit the display could not take: tried again this much later. Also
/// the least time between two commits of a moving pointer, about a frame.
const RETRY_MS: u64 = 16;

#[derive(Default)]
pub struct Pointer {
    /// Where the mouse points, panel pixels; None before it first moves.
    at: Option<(i32, i32)>,
    /// When it last moved or pressed.
    last_ms: u64,
    /// As the display was last told.
    shown: bool,
    /// The display has not taken the latest place yet; when to try again.
    owed: bool,
    next_try: u64,
}

impl Pointer {
    /// The mouse moved or pressed: it points at (`x`, `y`).
    pub fn moved(&mut self, x: i32, y: i32, now: u64) {
        if self.at != Some((x, y)) || !self.shown {
            self.owed = true;
        }
        self.at = Some((x, y));
        self.last_ms = now;
    }

    /// Shown, as the display was last told: the map's marker keeps off the
    /// cursor meanwhile.
    pub fn shown(&self) -> bool {
        self.shown
    }

    fn wanted(&self, enabled: bool, autohide: bool, now: u64) -> bool {
        enabled && self.at.is_some() && !(autohide && now >= self.last_ms + HIDE_MS)
    }

    /// Each turn of the loop: shown or hidden as the settings and the
    /// clock say, and the display told what it has not taken yet.
    pub fn tick(&mut self, display: &Display, enabled: bool, autohide: bool, now: u64) {
        let want = self.wanted(enabled, autohide, now);
        if want != self.shown {
            self.shown = want;
            self.owed = true;
        }
        if !self.owed || now < self.next_try {
            return;
        }
        let at = self.at.filter(|_| self.shown);
        self.next_try = now + RETRY_MS;
        if display.set_pointer(at) {
            self.owed = false;
        }
    }

    /// When the loop should look again: a commit owed, or the pointer due
    /// to hide.
    pub fn next_due(&self, autohide: bool, now: u64) -> Option<u64> {
        let owed = self.owed.then_some(self.next_try.max(now));
        let hide = (self.shown && autohide).then_some(self.last_ms + HIDE_MS);
        owed.into_iter().chain(hide).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_hides_after_a_while_only_with_auto_hide() {
        let mut p = Pointer::default();
        assert!(!p.wanted(true, true, 0), "not before the mouse moves");
        p.moved(10, 20, 1000);
        assert!(p.wanted(true, true, 1000 + HIDE_MS - 1));
        assert!(!p.wanted(true, true, 1000 + HIDE_MS));
        assert!(p.wanted(true, false, 1_000_000));
        assert!(!p.wanted(false, false, 1000), "never with the mouse off");
    }

    #[test]
    fn the_loop_is_woken_for_a_commit_owed_and_for_hiding() {
        let mut p = Pointer::default();
        assert_eq!(p.next_due(true, 0), None);
        p.moved(1, 1, 500);
        assert_eq!(p.next_due(true, 600), Some(600), "owed: at once");
        p.owed = false;
        p.shown = true;
        assert_eq!(p.next_due(true, 600), Some(500 + HIDE_MS));
        assert_eq!(p.next_due(false, 600), None);
    }
}
