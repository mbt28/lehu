// SPDX-License-Identifier: GPL-3.0-or-later
//! The touchscreen: multitouch protocol B or single-touch, calibrated into
//! panel ten-thousandths, one frame per `SYN_REPORT`, routed to the screens
//! or to the phone. Pure: events come in as values, frames go out through
//! the sink.

use crate::evdev::{self, RawEvent};
use crate::{Contact, MAX_SLOTS, Phase, Route, Sink, TOUCH_SCALE};

/// One axis's range, as the device reports it.
#[derive(Clone, Copy, Debug)]
struct Axis {
    min: i64,
    span: i64,
}

impl Axis {
    fn new(min: i32, max: i32) -> Self {
        let min = i64::from(min);
        let span = (i64::from(max) - min).max(1);
        Self { min, span }
    }

    /// Into 0..=TOUCH_SCALE, clamped: a device may report past its range.
    fn map(&self, raw: i32) -> u16 {
        let d = (i64::from(raw) - self.min).clamp(0, self.span);
        (d * i64::from(TOUCH_SCALE) / self.span) as u16
    }
}

/// From device axes to the panel, for a panel mounted another way round.
#[derive(Clone, Copy, Debug)]
pub struct Calibration {
    x: Axis,
    y: Axis,
    swap_xy: bool,
    invert_x: bool,
    invert_y: bool,
}

impl Calibration {
    pub fn new(
        x: (i32, i32),
        y: (i32, i32),
        swap_xy: bool,
        invert_x: bool,
        invert_y: bool,
    ) -> Self {
        Self {
            x: Axis::new(x.0, x.1),
            y: Axis::new(y.0, y.1),
            swap_xy,
            invert_x,
            invert_y,
        }
    }

    pub fn map(&self, x: i32, y: i32) -> (u16, u16) {
        let (mut nx, mut ny) = (self.x.map(x), self.y.map(y));
        if self.swap_xy {
            std::mem::swap(&mut nx, &mut ny);
        }
        if self.invert_x {
            nx = TOUCH_SCALE - nx;
        }
        if self.invert_y {
            ny = TOUCH_SCALE - ny;
        }
        (nx, ny)
    }
}

/// One tracked finger. `active` = touching now, `was_active` = touching at
/// the last frame; the transition gives the phase.
#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    active: bool,
    was_active: bool,
    x: i32,
    y: i32,
}

/// The device's state as the kernel holds it, read back after it dropped
/// events: the current slot, and per slot the tracking id (negative when
/// empty) and position. Single-touch devices fill slot 0 only.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub slot: i32,
    pub ids: [i32; MAX_SLOTS],
    pub x: [i32; MAX_SLOTS],
    pub y: [i32; MAX_SLOTS],
}

impl Snapshot {
    /// Nothing touching.
    pub fn lifted() -> Self {
        Self {
            slot: 0,
            ids: [-1; MAX_SLOTS],
            x: [0; MAX_SLOTS],
            y: [0; MAX_SLOTS],
        }
    }
}

pub struct Touch {
    cal: Calibration,
    mt: bool,
    nslots: usize,
    slots: [Slot; MAX_SLOTS],
    /// The slot the next MT event is about; MAX_SLOTS when it named one
    /// the device should not have, so its events are ignored.
    cur: usize,
    dirty: bool,
    /// SYN_DROPPED seen: ignoring until the next SYN_REPORT, then resyncing.
    dropped: bool,
    route: Route,
    /// A contact belongs to the route it started on: after a route change
    /// the rest of it is swallowed, so the new side never sees a press it
    /// has no context for.
    suppress: bool,
    /// The screens saw a press; where it last was, for its release.
    screens_down: Option<Contact>,
    frame: [Contact; MAX_SLOTS],
}

impl Touch {
    /// `nslots`: the device's slot count (1 for single-touch), at most
    /// MAX_SLOTS.
    pub fn new(cal: Calibration, mt: bool, nslots: usize, route: Route) -> Self {
        Self {
            cal,
            mt,
            nslots: nslots.clamp(1, MAX_SLOTS),
            slots: [Slot::default(); MAX_SLOTS],
            cur: 0,
            dirty: false,
            dropped: false,
            route,
            suppress: false,
            screens_down: None,
            frame: [Contact::default(); MAX_SLOTS],
        }
    }

    pub fn is_mt(&self) -> bool {
        self.mt
    }

    /// One event. True when the device must be read back with `resync`:
    /// the kernel dropped events and a frame's worth of state is unknown.
    pub fn event(&mut self, ev: &RawEvent, sink: &mut dyn Sink) -> bool {
        match ev.kind {
            evdev::EV_SYN => match ev.code {
                evdev::SYN_DROPPED => {
                    self.dropped = true;
                    self.dirty = false;
                }
                evdev::SYN_REPORT => {
                    if self.dropped {
                        self.dropped = false;
                        return true;
                    }
                    if self.dirty {
                        self.report(sink);
                    }
                    self.dirty = false;
                }
                _ => {}
            },
            // The rest of a cut-short packet: the device is read back instead.
            _ if self.dropped => {}
            evdev::EV_ABS if self.mt => match ev.code {
                evdev::ABS_MT_SLOT => {
                    self.cur = usize::try_from(ev.value)
                        .ok()
                        .filter(|&s| s < self.nslots)
                        .unwrap_or(MAX_SLOTS);
                }
                evdev::ABS_MT_TRACKING_ID => {
                    if let Some(s) = self.current() {
                        s.active = ev.value >= 0;
                        self.dirty = true;
                    }
                }
                evdev::ABS_MT_POSITION_X => {
                    if let Some(s) = self.current() {
                        s.x = ev.value;
                        self.dirty = true;
                    }
                }
                evdev::ABS_MT_POSITION_Y => {
                    if let Some(s) = self.current() {
                        s.y = ev.value;
                        self.dirty = true;
                    }
                }
                _ => {}
            },
            // Single-touch: everything is finger 0.
            evdev::EV_ABS => match ev.code {
                evdev::ABS_X => {
                    self.slots[0].x = ev.value;
                    self.dirty = true;
                }
                evdev::ABS_Y => {
                    self.slots[0].y = ev.value;
                    self.dirty = true;
                }
                _ => {}
            },
            evdev::EV_KEY if !self.mt && ev.code == evdev::BTN_TOUCH => {
                self.slots[0].active = ev.value != 0;
                self.dirty = true;
            }
            _ => {}
        }
        false
    }

    fn current(&mut self) -> Option<&mut Slot> {
        let cur = self.cur;
        (cur < self.nslots).then(|| &mut self.slots[cur])
    }

    /// After dropped events: take the device's state as it is now. Fingers
    /// that left meanwhile are released, new ones pressed.
    pub fn resync(&mut self, snap: &Snapshot, sink: &mut dyn Sink) {
        for (i, s) in self.slots.iter_mut().enumerate().take(self.nslots) {
            s.active = snap.ids[i] >= 0;
            // A finger that left is released where it was last seen.
            if s.active {
                s.x = snap.x[i];
                s.y = snap.y[i];
            }
        }
        self.cur = usize::try_from(snap.slot)
            .ok()
            .filter(|&s| s < self.nslots)
            .unwrap_or(MAX_SLOTS);
        self.dirty = false;
        self.report(sink);
    }

    /// Where frames go. A change gives the old side a terminal release, and
    /// swallows what is left of any contact still on the glass.
    pub fn set_route(&mut self, route: Route, sink: &mut dyn Sink) {
        if route == self.route {
            return;
        }
        let any_down = self.slots[..self.nslots].iter().any(|s| s.active);
        match self.route {
            Route::Screens => self.release_screens(sink),
            Route::Phone => {
                if any_down {
                    self.lift_phone(sink);
                }
            }
        }
        self.suppress = any_down;
        self.route = route;
    }

    /// The device went away: whatever is down is released where it went.
    pub fn device_gone(&mut self, sink: &mut dyn Sink) {
        match self.route {
            Route::Screens => self.release_screens(sink),
            Route::Phone => {
                if !self.suppress {
                    self.lift_phone(sink);
                }
            }
        }
    }

    fn release_screens(&mut self, sink: &mut dyn Sink) {
        if let Some(mut c) = self.screens_down.take() {
            c.phase = Phase::Up;
            sink.touch(Route::Screens, std::slice::from_ref(&c));
        }
    }

    /// Every finger down, as a release, to the phone: no drag is left
    /// dangling there.
    fn lift_phone(&mut self, sink: &mut dyn Sink) {
        let mut n = 0;
        for (i, s) in self.slots.iter().enumerate().take(self.nslots) {
            if !s.active {
                continue;
            }
            let (x, y) = self.cal.map(s.x, s.y);
            self.frame[n] = Contact {
                slot: i as u8,
                x,
                y,
                phase: Phase::Up,
            };
            n += 1;
        }
        if n > 0 {
            sink.touch(Route::Phone, &self.frame[..n]);
        }
    }

    /// The frame at a SYN_REPORT: every slot that is down or just lifted.
    fn report(&mut self, sink: &mut dyn Sink) {
        let mut n = 0;
        let mut any_down = false;
        for (i, s) in self.slots.iter_mut().enumerate().take(self.nslots) {
            if !s.active && !s.was_active {
                continue;
            }
            let phase = match (s.was_active, s.active) {
                (false, true) => Phase::Down,
                (true, true) => Phase::Move,
                _ => Phase::Up,
            };
            let (x, y) = self.cal.map(s.x, s.y);
            // The pointer id is the slot, not the tracking id: the slot is
            // stable for the contact, the tracking id changes at every touch.
            self.frame[n] = Contact {
                slot: i as u8,
                x,
                y,
                phase,
            };
            n += 1;
            any_down |= s.active;
            s.was_active = s.active;
        }
        if n == 0 {
            return;
        }
        if self.suppress {
            // Contact over: the next press flows again.
            self.suppress = any_down;
            return;
        }
        match self.route {
            Route::Screens => {
                // The screens take one pointer: the first finger drives them.
                let primary = self.frame[0];
                sink.touch(Route::Screens, std::slice::from_ref(&primary));
                self.screens_down = (primary.phase != Phase::Up).then_some(primary);
            }
            Route::Phone => sink.touch(Route::Phone, &self.frame[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev::*;
    use crate::test_sink::{Out, Rec};

    fn cal() -> Calibration {
        Calibration::new((0, 4095), (0, 4095), false, false, false)
    }

    fn mt() -> Touch {
        Touch::new(cal(), true, 5, Route::Phone)
    }

    fn feed(t: &mut Touch, sink: &mut Rec, evs: &[(u16, u16, i32)]) {
        for &(kind, code, value) in evs {
            assert!(!t.event(&RawEvent::new(kind, code, value), sink));
        }
    }

    fn syn() -> (u16, u16, i32) {
        (EV_SYN, SYN_REPORT, 0)
    }

    fn c(slot: u8, x: u16, y: u16, phase: Phase) -> Contact {
        Contact { slot, x, y, phase }
    }

    #[test]
    fn protocol_b_down_move_up() {
        let (mut t, mut s) = (mt(), Rec::new(true));
        feed(
            &mut t,
            &mut s,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 4095),
                syn(),
            ],
        );
        assert_eq!(
            s.take(),
            vec![Out::Touch(Route::Phone, vec![c(0, 0, 10_000, Phase::Down)])]
        );
        feed(&mut t, &mut s, &[(EV_ABS, ABS_MT_POSITION_X, 2048), syn()]);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![c(0, 5001, 10_000, Phase::Move)]
            )]
        );
        feed(&mut t, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, -1), syn()]);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![c(0, 5001, 10_000, Phase::Up)]
            )]
        );
        // An idle slot reports nothing, and a bare SYN_REPORT nothing at all.
        feed(&mut t, &mut s, &[syn()]);
        assert!(s.take().is_empty());
    }

    #[test]
    fn two_fingers_in_one_frame_and_the_screens_take_the_first() {
        let (mut t, mut s) = (mt(), Rec::new(true));
        let two = [
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 100),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, 2),
            (EV_ABS, ABS_MT_POSITION_X, 4095),
            (EV_ABS, ABS_MT_POSITION_Y, 4095),
            syn(),
        ];
        feed(&mut t, &mut s, &two);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![
                    c(0, 244, 244, Phase::Down),
                    c(1, 10_000, 10_000, Phase::Down)
                ]
            )]
        );
        let mut t = Touch::new(cal(), true, 5, Route::Screens);
        feed(&mut t, &mut s, &two);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Screens,
                vec![c(0, 244, 244, Phase::Down)]
            )]
        );
    }

    #[test]
    fn single_touch_device() {
        let mut t = Touch::new(cal(), false, 1, Route::Phone);
        let mut s = Rec::new(true);
        feed(
            &mut t,
            &mut s,
            &[
                (EV_KEY, BTN_TOUCH, 1),
                (EV_ABS, ABS_X, 4095),
                (EV_ABS, ABS_Y, 0),
                syn(),
                (EV_ABS, ABS_X, 2048),
                syn(),
                (EV_KEY, BTN_TOUCH, 0),
                syn(),
            ],
        );
        assert_eq!(
            s.take(),
            vec![
                Out::Touch(Route::Phone, vec![c(0, 10_000, 0, Phase::Down)]),
                Out::Touch(Route::Phone, vec![c(0, 5001, 0, Phase::Move)]),
                Out::Touch(Route::Phone, vec![c(0, 5001, 0, Phase::Up)]),
            ]
        );
    }

    #[test]
    fn swap_and_invert_and_clamp() {
        let cal = Calibration::new((100, 1100), (0, 500), true, true, false);
        // x maps to 10% along its axis, y to 40%; swapped, then x inverted.
        assert_eq!(cal.map(200, 200), (10_000 - 4000, 1000));
        // Past the range: clamped, not wrapped.
        assert_eq!(cal.map(5000, -50), (10_000, 10_000));
        let plain = Calibration::new((0, 0), (0, 0), false, false, true); // degenerate range
        assert_eq!(plain.map(0, 0), (0, 10_000));
    }

    #[test]
    fn a_slot_the_device_should_not_have_is_ignored() {
        let (mut t, mut s) = (mt(), Rec::new(true));
        feed(
            &mut t,
            &mut s,
            &[
                (EV_ABS, ABS_MT_SLOT, 12),
                (EV_ABS, ABS_MT_TRACKING_ID, 3),
                (EV_ABS, ABS_MT_POSITION_X, 10),
                syn(),
                (EV_ABS, ABS_MT_SLOT, -1),
                (EV_ABS, ABS_MT_TRACKING_ID, 3),
                syn(),
            ],
        );
        assert!(s.take().is_empty());
    }

    #[test]
    fn route_change_releases_the_screens_and_swallows_the_contact() {
        let mut t = Touch::new(cal(), true, 5, Route::Screens);
        let mut s = Rec::new(true);
        feed(
            &mut t,
            &mut s,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 2048),
                (EV_ABS, ABS_MT_POSITION_Y, 2048),
                syn(),
            ],
        );
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Screens,
                vec![c(0, 5001, 5001, Phase::Down)]
            )]
        );
        t.set_route(Route::Phone, &mut s);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Screens,
                vec![c(0, 5001, 5001, Phase::Up)]
            )]
        );
        // The finger moves and lifts: neither side hears of it.
        feed(
            &mut t,
            &mut s,
            &[
                (EV_ABS, ABS_MT_POSITION_X, 3000),
                syn(),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                syn(),
            ],
        );
        assert!(s.take().is_empty());
        // The next press flows to the phone.
        feed(&mut t, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, 2), syn()]);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![c(0, 7326, 5001, Phase::Down)]
            )]
        );
        // Flipping back mid-contact lifts it on the phone, where it started;
        // its real lift is then swallowed.
        t.set_route(Route::Screens, &mut s);
        assert_eq!(
            s.take(),
            vec![Out::Touch(Route::Phone, vec![c(0, 7326, 5001, Phase::Up)])]
        );
        feed(&mut t, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, -1), syn()]);
        assert!(s.take().is_empty());
        // With nothing down a flip releases nothing.
        t.set_route(Route::Phone, &mut s);
        assert!(s.take().is_empty());
    }

    #[test]
    fn route_change_lifts_every_finger_on_the_phone() {
        let (mut t, mut s) = (mt(), Rec::new(true));
        feed(
            &mut t,
            &mut s,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, 4095),
                syn(),
            ],
        );
        s.take();
        t.set_route(Route::Screens, &mut s);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![c(0, 0, 0, Phase::Up), c(1, 10_000, 0, Phase::Up)]
            )]
        );
        t.device_gone(&mut s);
        assert!(s.take().is_empty()); // nothing the screens saw
    }

    #[test]
    fn dropped_events_resync_from_the_device() {
        let (mut t, mut s) = (mt(), Rec::new(true));
        feed(
            &mut t,
            &mut s,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 1000),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
                syn(),
            ],
        );
        s.take();
        assert!(!t.event(&RawEvent::new(EV_SYN, SYN_DROPPED, 0), &mut s));
        // The rest of the cut-short packet changes nothing.
        assert!(!t.event(&RawEvent::new(EV_ABS, ABS_MT_POSITION_X, 4095), &mut s));
        assert!(t.event(&RawEvent::new(EV_SYN, SYN_REPORT, 0), &mut s));
        assert!(s.take().is_empty());
        // The device says: slot 0 lifted, slot 1 down at the far corner.
        let mut snap = Snapshot::lifted();
        snap.slot = 1;
        snap.ids[1] = 9;
        snap.x[1] = 4095;
        snap.y[1] = 4095;
        t.resync(&snap, &mut s);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![
                    c(0, 2442, 2442, Phase::Up),
                    c(1, 10_000, 10_000, Phase::Down)
                ]
            )]
        );
        // Events now address the slot the device is on.
        feed(&mut t, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, -1), syn()]);
        assert_eq!(
            s.take(),
            vec![Out::Touch(
                Route::Phone,
                vec![c(1, 10_000, 10_000, Phase::Up)]
            )]
        );
    }

    #[test]
    fn device_gone_releases_the_screens() {
        let mut t = Touch::new(cal(), false, 1, Route::Screens);
        let mut s = Rec::new(true);
        feed(&mut t, &mut s, &[(EV_KEY, BTN_TOUCH, 1), syn()]);
        s.take();
        t.device_gone(&mut s);
        assert_eq!(
            s.take(),
            vec![Out::Touch(Route::Screens, vec![c(0, 0, 0, Phase::Up)])]
        );
    }
}
