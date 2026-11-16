// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A mouse, or a keyboard's touchpad that reports as one, as a pointer on
//! the panel: its moves move the pointer, its left button touches where it
//! points (one contact, routed like a finger), its right button is Back.
//! Its wheel and middle button stay the knob's (`keys`). Every mouse moves
//! the same pointer.
//!
//! A press belongs to the route it went down on, as a finger does: a route
//! change lifts it there, and the button does nothing more until it is let
//! go.

use crate::evdev::{self, RawEvent};
use crate::{Contact, Nav, Phase, PhoneButton, Route, Sink, TOUCH_SCALE, Value};

/// The contact's slot: the first finger's, the one every phone protocol
/// takes (CarPlay's touch report has two).
pub(crate) const MOUSE_SLOT: u8 = 0;

pub(crate) struct Mouse {
    /// The panel in pixels, and the pointer on it.
    panel: (i32, i32),
    x: i32,
    y: i32,
    /// Since the last report: moved, the buttons as they are now.
    moved: bool,
    left: bool,
    right: bool,
    reported: bool,
    /// The route the left (right) button went down on, while held; a
    /// button lifted by a route change is dead until let go.
    left_on: Option<Route>,
    left_dead: bool,
    right_on: Option<Route>,
    right_dead: bool,
    route: Route,
}

impl Mouse {
    pub(crate) fn new(panel: (u32, u32), route: Route) -> Self {
        let w = i32::try_from(panel.0.max(1)).unwrap_or(800);
        let h = i32::try_from(panel.1.max(1)).unwrap_or(480);
        Self {
            panel: (w, h),
            x: w / 2,
            y: h / 2,
            moved: false,
            left: false,
            right: false,
            reported: true,
            left_on: None,
            left_dead: false,
            right_on: None,
            right_dead: false,
            route,
        }
    }

    /// The events a mouse's pointer takes: moves and the two buttons.
    pub(crate) fn takes(ev: &RawEvent) -> bool {
        match ev.kind {
            evdev::EV_REL => ev.code == evdev::REL_X || ev.code == evdev::REL_Y,
            evdev::EV_KEY => ev.code == evdev::BTN_LEFT || ev.code == evdev::BTN_RIGHT,
            _ => false,
        }
    }

    /// One event of `takes`, or a `SYN_REPORT`, which reports what changed.
    pub(crate) fn event(&mut self, ev: &RawEvent, sink: &mut dyn Sink) {
        match (ev.kind, ev.code) {
            (evdev::EV_REL, evdev::REL_X) => {
                self.x = (self.x + ev.value).clamp(0, self.panel.0 - 1);
                self.moved = true;
            }
            (evdev::EV_REL, evdev::REL_Y) => {
                self.y = (self.y + ev.value).clamp(0, self.panel.1 - 1);
                self.moved = true;
            }
            (evdev::EV_KEY, evdev::BTN_LEFT) => {
                self.left = ev.value != 0;
                self.reported = false;
            }
            (evdev::EV_KEY, evdev::BTN_RIGHT) => {
                self.right = ev.value != 0;
                self.reported = false;
            }
            (evdev::EV_SYN, evdev::SYN_REPORT) => self.report(sink),
            _ => {}
        }
    }

    /// Where the pointer is, in ten-thousandths of the panel.
    fn at(&self) -> (u16, u16) {
        let scale = |v: i32, size: i32| (v * i32::from(TOUCH_SCALE) / size) as u16;
        (scale(self.x, self.panel.0), scale(self.y, self.panel.1))
    }

    fn contact(&self, phase: Phase) -> [Contact; 1] {
        let (x, y) = self.at();
        [Contact {
            slot: MOUSE_SLOT,
            x,
            y,
            phase,
        }]
    }

    fn report(&mut self, sink: &mut dyn Sink) {
        if !self.moved && self.reported {
            return;
        }
        let (x, y) = self.at();
        sink.pointer(x, y);
        // The left button: a finger where the pointer is.
        match (self.left, self.left_on) {
            (true, None) if !self.left_dead => {
                self.left_on = Some(self.route);
                sink.touch(self.route, &self.contact(Phase::Down));
            }
            (true, Some(route)) if self.moved => {
                sink.touch(route, &self.contact(Phase::Move));
            }
            (false, Some(route)) => {
                self.left_on = None;
                sink.touch(route, &self.contact(Phase::Up));
            }
            (false, None) => self.left_dead = false,
            _ => {}
        }
        // The right button: Back.
        match (self.right, self.right_on) {
            (true, None) if !self.right_dead => {
                self.right_on = Some(self.route);
                back(self.route, Value::Press, sink);
            }
            (false, Some(route)) => {
                self.right_on = None;
                back(route, Value::Release, sink);
            }
            (false, None) => self.right_dead = false,
            _ => {}
        }
        self.moved = false;
        self.reported = true;
    }

    /// From now on presses go to `route`; what is held is let go where it
    /// went down.
    pub(crate) fn set_route(&mut self, route: Route, sink: &mut dyn Sink) {
        if route != self.route {
            self.release(sink);
            self.route = route;
        }
    }

    /// Lift what is held, where it went down; the buttons are dead until
    /// let go (the mouse went, or the pointer was turned off).
    pub(crate) fn release(&mut self, sink: &mut dyn Sink) {
        if let Some(route) = self.left_on.take() {
            sink.touch(route, &self.contact(Phase::Up));
            self.left_dead = true;
        }
        if let Some(route) = self.right_on.take() {
            back(route, Value::Release, sink);
            self.right_dead = true;
        }
    }
}

/// Back on `route`: a key for the screens (pressed and released), the
/// phone's Back button (pressed only, as the phone's buttons are).
fn back(route: Route, value: Value, sink: &mut dyn Sink) {
    match route {
        Route::Screens => sink.key(Some(Nav::Back), None, value),
        Route::Phone => {
            if value == Value::Press {
                sink.phone(PhoneButton::Back, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev::{BTN_LEFT, BTN_RIGHT, EV_KEY, EV_REL, EV_SYN, REL_X, REL_Y, SYN_REPORT};
    use crate::test_sink::{Out, Rec};

    fn feed(m: &mut Mouse, s: &mut Rec, evs: &[(u16, u16, i32)]) {
        for &(kind, code, value) in evs {
            m.event(&RawEvent::new(kind, code, value), s);
        }
        m.event(&RawEvent::new(EV_SYN, SYN_REPORT, 0), s);
    }

    fn c(x: u16, y: u16, phase: Phase) -> Contact {
        Contact {
            slot: MOUSE_SLOT,
            x,
            y,
            phase,
        }
    }

    #[test]
    fn moves_move_the_pointer_and_stop_at_the_edges() {
        let (mut m, mut s) = (Mouse::new((800, 480), Route::Screens), Rec::new(false));
        // It starts in the middle.
        feed(&mut m, &mut s, &[(EV_REL, REL_X, 40), (EV_REL, REL_Y, -24)]);
        assert_eq!(s.take(), vec![Out::Pointer(5500, 4500)]);
        feed(
            &mut m,
            &mut s,
            &[(EV_REL, REL_X, -5000), (EV_REL, REL_Y, 5000)],
        );
        assert_eq!(s.take(), vec![Out::Pointer(0, 9979)]);
        // A report with nothing new says nothing.
        feed(&mut m, &mut s, &[]);
        assert!(s.take().is_empty());
    }

    #[test]
    fn the_left_button_is_a_finger_where_the_pointer_is() {
        let (mut m, mut s) = (Mouse::new((800, 480), Route::Screens), Rec::new(false));
        feed(&mut m, &mut s, &[(EV_KEY, BTN_LEFT, 1)]);
        assert_eq!(
            s.take(),
            vec![
                Out::Pointer(5000, 5000),
                Out::Touch(Route::Screens, vec![c(5000, 5000, Phase::Down)])
            ]
        );
        feed(&mut m, &mut s, &[(EV_REL, REL_X, 80)]);
        assert_eq!(
            s.take(),
            vec![
                Out::Pointer(6000, 5000),
                Out::Touch(Route::Screens, vec![c(6000, 5000, Phase::Move)])
            ]
        );
        feed(&mut m, &mut s, &[(EV_KEY, BTN_LEFT, 0)]);
        assert_eq!(
            s.take(),
            vec![
                Out::Pointer(6000, 5000),
                Out::Touch(Route::Screens, vec![c(6000, 5000, Phase::Up)])
            ]
        );
    }

    #[test]
    fn a_press_stays_on_its_route_and_the_right_button_is_back() {
        let (mut m, mut s) = (Mouse::new((800, 480), Route::Screens), Rec::new(true));
        feed(&mut m, &mut s, &[(EV_KEY, BTN_LEFT, 1)]);
        s.take();
        // The phone takes the panel: the press is lifted on the screens,
        // and the button does nothing until let go.
        m.set_route(Route::Phone, &mut s);
        assert_eq!(
            s.take(),
            vec![Out::Touch(Route::Screens, vec![c(5000, 5000, Phase::Up)])]
        );
        feed(&mut m, &mut s, &[(EV_REL, REL_X, 8)]);
        assert_eq!(s.take(), vec![Out::Pointer(5100, 5000)]);
        feed(&mut m, &mut s, &[(EV_KEY, BTN_LEFT, 0)]);
        s.take();
        feed(&mut m, &mut s, &[(EV_KEY, BTN_LEFT, 1)]);
        assert_eq!(
            s.take(),
            vec![
                Out::Pointer(5100, 5000),
                Out::Touch(Route::Phone, vec![c(5100, 5000, Phase::Down)])
            ]
        );
        feed(&mut m, &mut s, &[(EV_KEY, BTN_LEFT, 0)]);
        s.take();
        // Right: the phone's Back, pressed only.
        feed(&mut m, &mut s, &[(EV_KEY, BTN_RIGHT, 1)]);
        feed(&mut m, &mut s, &[(EV_KEY, BTN_RIGHT, 0)]);
        assert_eq!(
            s.take(),
            vec![
                Out::Pointer(5100, 5000),
                Out::Phone(PhoneButton::Back, Value::Press),
                Out::Pointer(5100, 5000),
            ]
        );
        // On the screens: Back pressed and released.
        m.set_route(Route::Screens, &mut s);
        feed(&mut m, &mut s, &[(EV_KEY, BTN_RIGHT, 1)]);
        feed(&mut m, &mut s, &[(EV_KEY, BTN_RIGHT, 0)]);
        assert_eq!(
            s.take(),
            vec![
                Out::Pointer(5100, 5000),
                Out::Key(Some(Nav::Back), None, Value::Press),
                Out::Pointer(5100, 5000),
                Out::Key(Some(Nav::Back), None, Value::Release),
            ]
        );
    }
}
