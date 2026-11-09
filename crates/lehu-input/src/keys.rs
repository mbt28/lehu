// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Keys from Bluetooth keyboards and remotes: to the phone as its buttons,
//! or to the screens as navigation and typed characters, with the repeats,
//! the volume keys, the wheel as a knob, the camera key and key learning.
//! Pure: events, the clock and the main thread's controls come in as
//! values; what results goes out through the sink.

use crate::evdev::{self, RawEvent};
use crate::keymap::{self, VolumeKey};
use crate::{Nav, PhoneButton, Route, Sink, Value};

/// Repeats are synthesised here rather than taken from the kernel, whose
/// 250 ms then 33 ms would flood the phone's write queue with a held arrow;
/// Android Auto shares that queue with its video acknowledgements.
pub const REPEAT_DELAY_MS: u64 = 400;
pub const REPEAT_SCREENS_MS: u64 = 120;
pub const REPEAT_PHONE_MS: u64 = 150;
pub const REPEAT_VOLUME_MS: u64 = 150;
/// A held volume key repeats this long at most (0 to 100 % takes about 3 s)
/// and then needs a fresh press: a release lost over the air must not drive
/// the volume to its end.
pub const VOLUME_REPEAT_FOR_MS: u64 = 4000;
/// A wheel's turns reach the phone one detent per report, as CarPlay and
/// Android Auto expect, but no faster than this, for the same write queue.
/// A spin ahead of the pace keeps at most MAX_TURNS_PENDING steps and the
/// rest is dropped: a knob that keeps turning after the hand has stopped is
/// worse than one that skipped.
pub const TURN_PHONE_MS: u64 = 30;
pub const MAX_TURNS_PENDING: i8 = 4;
/// One event carries at most this many detents, whatever the device claims.
pub const MAX_STEPS_PER_EVENT: i32 = 8;
/// More keys down at once than any hand has: the oldest makes room.
const MAX_HELD: usize = 16;
/// A device that keeps overflowing logs once in this long.
const DROP_LOG_MS: u64 = 10_000;

/// One keyboard's own state; the reader keeps one per open device.
#[derive(Clone, Copy, Debug, Default)]
pub struct Keyboard {
    /// The device's descriptor: held keys are matched by it.
    pub id: i32,
    shift_l: bool,
    shift_r: bool,
    caps: bool,
    /// Ctrl, Alt or Meta held: no typed characters.
    other_mods: u8,
    hat_x: i8,
    hat_y: i8,
    /// SYN_DROPPED seen: skipping to the next SYN_REPORT.
    dropped: bool,
    drop_logged_ms: u64,
}

impl Keyboard {
    pub fn new(id: i32) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }
}

/// What the main thread controls, read before each batch of events; a
/// learned key turns `learning` off here, and the reader writes that back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Controls {
    /// The camera key, 0 for none.
    pub camera_key: u16,
    /// The next key is learned instead of acting (Esc and Back still act).
    pub learning: bool,
}

#[derive(Clone, Copy, Debug)]
enum Target {
    Screens {
        nav: Option<Nav>,
        ch: Option<char>,
    },
    Phone {
        button: PhoneButton,
        /// Select went out: its release is owed.
        select_sent: bool,
    },
    /// On no route: a route change leaves it alone.
    Volume {
        step: i8,
        until: u64,
    },
}

/// A pressed key, remembered with the route it was pressed on: its repeats
/// and its release go there.
#[derive(Clone, Copy, Debug)]
struct Held {
    dev: i32,
    code: u16,
    target: Target,
    /// 0: no repeat.
    period: u64,
    next_repeat: u64,
}

pub struct Keys {
    held: Vec<Held>,
    route: Route,
    /// Wheel steps owed to the phone: positive next, negative previous.
    turns_pending: i8,
    last_turn: u64,
}

impl Keys {
    pub fn new(route: Route) -> Self {
        Self {
            held: Vec::with_capacity(MAX_HELD),
            route,
            turns_pending: 0,
            last_turn: 0,
        }
    }

    /// One event from one keyboard. True when the device must be read back
    /// with `resync`: the kernel dropped events, a release may be among them.
    pub fn event(
        &mut self,
        kb: &mut Keyboard,
        ev: &RawEvent,
        now: u64,
        ctl: &mut Controls,
        sink: &mut dyn Sink,
    ) -> bool {
        match ev.kind {
            evdev::EV_SYN => match ev.code {
                evdev::SYN_DROPPED => kb.dropped = true,
                evdev::SYN_REPORT if kb.dropped => {
                    kb.dropped = false;
                    return true;
                }
                _ => {}
            },
            // The rest of a cut-short packet: ignored until SYN_REPORT.
            _ if kb.dropped => {}
            evdev::EV_KEY => self.on_key(kb, ev.code, ev.value, now, ctl, sink),
            evdev::EV_ABS if ev.code == evdev::ABS_HAT0X || ev.code == evdev::ABS_HAT0Y => {
                self.on_hat(kb, ev.code, ev.value, now, ctl, sink);
            }
            evdev::EV_REL if ev.code == evdev::REL_WHEEL => self.on_wheel(ev.value, now, sink),
            _ => {}
        }
        false
    }

    fn on_key(
        &mut self,
        kb: &mut Keyboard,
        code: u16,
        value: i32,
        now: u64,
        ctl: &mut Controls,
        sink: &mut dyn Sink,
    ) {
        if usize::from(code) >= evdev::KEY_CNT {
            return;
        }
        let down = value != 0;
        match code {
            evdev::KEY_LEFTSHIFT => {
                kb.shift_l = down;
                return;
            }
            evdev::KEY_RIGHTSHIFT => {
                kb.shift_r = down;
                return;
            }
            evdev::KEY_LEFTCTRL
            | evdev::KEY_RIGHTCTRL
            | evdev::KEY_LEFTALT
            | evdev::KEY_RIGHTALT
            | evdev::KEY_LEFTMETA
            | evdev::KEY_RIGHTMETA => {
                let bit = modifier_bit(code);
                if down {
                    kb.other_mods |= bit;
                } else {
                    kb.other_mods &= !bit;
                }
                return;
            }
            evdev::KEY_CAPSLOCK => {
                if value == 1 {
                    kb.caps = !kb.caps;
                }
                return;
            }
            _ => {}
        }
        let code = keymap::normalize(code);
        match value {
            1 => self.press(kb, code, now, ctl, sink),
            0 => self.release(kb.id, code, sink),
            // 2 is the kernel's own repeat; ours is throttled (REPEAT_*).
            _ => {}
        }
    }

    fn press(
        &mut self,
        kb: &Keyboard,
        code: u16,
        now: u64,
        ctl: &mut Controls,
        sink: &mut dyn Sink,
    ) {
        // Settings > Video > Camera key: the next key is learned, not used
        // (Esc and Back still cancel the question). The learned key then
        // switches to the camera and back from anywhere, and does nothing
        // else. Neither press leaves a held entry, so the release is dropped.
        if ctl.learning && code != evdev::KEY_ESC && code != evdev::KEY_BACK {
            ctl.learning = false;
            sink.key_learned(code);
            return;
        }
        if code != 0 && code == ctl.camera_key {
            sink.camera_toggle();
            return;
        }
        if keymap::phone_only(code) {
            if let Some(button) = keymap::phone_button(code) {
                sink.phone(button, Value::Press);
            }
            return;
        }
        // A second press without a release in between: the old entry must
        // not outlive this one and keep repeating.
        self.release(kb.id, code, sink);
        if let Some(v) = keymap::volume_key(code) {
            // Any volume key first stops a repeat still running, from a key
            // whose release got lost (link loss, a remote that drops
            // key-ups), which would otherwise drive the volume to its end.
            self.held
                .retain(|h| !matches!(h.target, Target::Volume { .. }));
            let step = match v {
                VolumeKey::Mute => return sink.mute(),
                VolumeKey::Up => 1,
                VolumeKey::Down => -1,
            };
            sink.volume(step);
            self.push_held(
                Held {
                    dev: kb.id,
                    code,
                    target: Target::Volume {
                        step,
                        until: now + VOLUME_REPEAT_FOR_MS,
                    },
                    period: REPEAT_VOLUME_MS,
                    next_repeat: now + REPEAT_DELAY_MS,
                },
                sink,
            );
            return;
        }
        let (target, period) = match self.route {
            Route::Screens => {
                let shift = kb.shift_l || kb.shift_r;
                let nav = keymap::nav_key(code);
                let ch = if kb.other_mods != 0 {
                    None
                } else {
                    keymap::typed_char(code, shift, kb.caps)
                };
                if nav.is_none() && ch.is_none() {
                    return;
                }
                sink.key(nav, ch, Value::Press);
                // Arrows repeat (the screens ignore a repeated Left = Back,
                // the editor's keyboard ring wants them), and so does
                // Backspace.
                let repeats = matches!(
                    nav,
                    Some(Nav::Up | Nav::Down | Nav::Left | Nav::Right | Nav::Erase)
                );
                (
                    Target::Screens { nav, ch },
                    if repeats { REPEAT_SCREENS_MS } else { 0 },
                )
            }
            Route::Phone => {
                let Some(button) = keymap::phone_button(code) else {
                    return;
                };
                // One answer for both: a release is owed exactly when the
                // press went out.
                let sent = sink.phone(button, Value::Press);
                // Menu left the phone's video: the screens come up on Home
                // (as Home, not Menu: on Home, Menu would go straight back).
                if button == PhoneButton::Menu && sent {
                    sink.key(Some(Nav::Home), None, Value::Press);
                }
                let select_sent = button == PhoneButton::Select && sent;
                let repeats = matches!(
                    button,
                    PhoneButton::Up | PhoneButton::Down | PhoneButton::Left | PhoneButton::Right
                );
                if !select_sent && !repeats {
                    return; // nothing to do at its release
                }
                (
                    Target::Phone {
                        button,
                        select_sent,
                    },
                    if repeats { REPEAT_PHONE_MS } else { 0 },
                )
            }
        };
        self.push_held(
            Held {
                dev: kb.id,
                code,
                target,
                period,
                next_repeat: if period > 0 { now + REPEAT_DELAY_MS } else { 0 },
            },
            sink,
        );
    }

    fn push_held(&mut self, h: Held, sink: &mut dyn Sink) {
        if self.held.len() >= MAX_HELD {
            let oldest = self.held.remove(0);
            deliver_release(&oldest, sink);
        }
        self.held.push(h);
    }

    fn release(&mut self, dev: i32, code: u16, sink: &mut dyn Sink) {
        if let Some(i) = self
            .held
            .iter()
            .position(|h| h.dev == dev && h.code == code)
        {
            let h = self.held.remove(i);
            deliver_release(&h, sink);
        }
    }

    /// Terminal release for held keys, all or one device's: a select held
    /// on the phone is let go there, a key held on a screen gets its
    /// release. Their own later releases then find nothing. A held volume
    /// key belongs to no route: a route change (`keep_volume`) leaves it.
    fn release_held(&mut self, dev: Option<i32>, keep_volume: bool, sink: &mut dyn Sink) {
        let mut i = self.held.len();
        while i > 0 {
            i -= 1;
            let h = self.held[i];
            if dev.is_some_and(|d| d != h.dev)
                || (keep_volume && matches!(h.target, Target::Volume { .. }))
            {
                continue;
            }
            self.held.remove(i);
            deliver_release(&h, sink);
        }
    }

    /// Hat switches (most gamepad d-pads) as arrow presses and releases.
    fn on_hat(
        &mut self,
        kb: &mut Keyboard,
        axis: u16,
        value: i32,
        now: u64,
        ctl: &mut Controls,
        sink: &mut dyn Sink,
    ) {
        let dir = value.signum() as i8;
        let (negative, positive) = if axis == evdev::ABS_HAT0X {
            (evdev::KEY_LEFT, evdev::KEY_RIGHT)
        } else {
            (evdev::KEY_UP, evdev::KEY_DOWN)
        };
        let cur = if axis == evdev::ABS_HAT0X {
            kb.hat_x
        } else {
            kb.hat_y
        };
        if dir == cur {
            return;
        }
        if cur != 0 {
            self.release(kb.id, if cur < 0 { negative } else { positive }, sink);
        }
        if axis == evdev::ABS_HAT0X {
            kb.hat_x = dir;
        } else {
            kb.hat_y = dir;
        }
        if dir != 0 {
            self.press(
                kb,
                if dir < 0 { negative } else { positive },
                now,
                ctl,
                sink,
            );
        }
    }

    /// A wheel as a rotary knob, one step per detent. evdev counts away from
    /// the user as positive, like scrolling up: that is "previous".
    fn on_wheel(&mut self, value: i32, now: u64, sink: &mut dyn Sink) {
        let steps = value
            .saturating_neg()
            .clamp(-MAX_STEPS_PER_EVENT, MAX_STEPS_PER_EVENT) as i8;
        if steps == 0 {
            return;
        }
        match self.route {
            Route::Screens => {
                let nav = if steps > 0 { Nav::Next } else { Nav::Prev };
                for _ in 0..steps.unsigned_abs() {
                    sink.key(Some(nav), None, Value::Press);
                }
            }
            Route::Phone => {
                // A turn the other way drops what is still owed.
                if self.turns_pending != 0 && (steps > 0) != (self.turns_pending > 0) {
                    self.turns_pending = 0;
                }
                self.turns_pending = (i16::from(self.turns_pending) + i16::from(steps))
                    .clamp(-i16::from(MAX_TURNS_PENDING), i16::from(MAX_TURNS_PENDING))
                    as i8;
                self.send_turn(now, sink);
            }
        }
    }

    /// One owed wheel step to the phone, once the pace allows it.
    fn send_turn(&mut self, now: u64, sink: &mut dyn Sink) {
        if self.turns_pending == 0 || now.saturating_sub(self.last_turn) < TURN_PHONE_MS {
            return;
        }
        let button = if self.turns_pending > 0 {
            PhoneButton::RotateNext
        } else {
            PhoneButton::RotatePrev
        };
        sink.phone(button, Value::Press);
        self.turns_pending -= self.turns_pending.signum();
        self.last_turn = now;
    }

    /// Repeats and owed wheel turns that are due. Returns when the next one
    /// is, in `now` time; None when nothing is pending.
    pub fn tick(&mut self, now: u64, sink: &mut dyn Sink) -> Option<u64> {
        for h in &mut self.held {
            if h.period == 0 || now < h.next_repeat {
                continue;
            }
            match h.target {
                Target::Volume { step, until } => {
                    if now >= until {
                        h.period = 0; // held too long: wait for a fresh press
                        continue;
                    }
                    sink.volume(step);
                }
                Target::Screens { nav, ch } => sink.key(nav, ch, Value::Repeat),
                Target::Phone { button, .. } => {
                    sink.phone(button, Value::Repeat);
                }
            }
            h.next_repeat = now + h.period;
        }
        self.send_turn(now, sink);
        let mut next: Option<u64> = None;
        let mut consider = |deadline: u64| next = Some(next.map_or(deadline, |d| d.min(deadline)));
        for h in &self.held {
            if h.period > 0 {
                consider(h.next_repeat);
            }
        }
        if self.turns_pending != 0 {
            consider(self.last_turn + TURN_PHONE_MS);
        }
        next
    }

    /// Where keys go. A change releases what the old route holds, and
    /// forgets the wheel turns owed to the phone.
    pub fn set_route(&mut self, route: Route, sink: &mut dyn Sink) {
        if route == self.route {
            return;
        }
        self.route = route;
        self.release_held(None, true, sink);
        self.turns_pending = 0;
    }

    /// A device went away: everything it held is released.
    pub fn device_gone(&mut self, dev: i32, sink: &mut dyn Sink) {
        self.release_held(Some(dev), false, sink);
    }

    /// Shutting down: nothing stays pressed anywhere.
    pub fn release_all(&mut self, sink: &mut dyn Sink) {
        self.release_held(None, false, sink);
    }

    /// After SYN_DROPPED (the kernel's buffer overflowed while this thread
    /// was not scheduled): a release may be among the lost events. Let go
    /// of everything the device held and take its modifiers from `state`
    /// (EVIOCGKEY); a key still down counts again from its next press.
    pub fn resync(&mut self, kb: &mut Keyboard, state: &[usize], now: u64, sink: &mut dyn Sink) {
        self.release_held(Some(kb.id), false, sink);
        kb.shift_l = evdev::has(state, evdev::KEY_LEFTSHIFT);
        kb.shift_r = evdev::has(state, evdev::KEY_RIGHTSHIFT);
        kb.other_mods = 0;
        for code in [
            evdev::KEY_LEFTCTRL,
            evdev::KEY_RIGHTCTRL,
            evdev::KEY_LEFTALT,
            evdev::KEY_RIGHTALT,
            evdev::KEY_LEFTMETA,
            evdev::KEY_RIGHTMETA,
        ] {
            if evdev::has(state, code) {
                kb.other_mods |= modifier_bit(code);
            }
        }
        kb.hat_x = 0;
        kb.hat_y = 0;
        if kb.drop_logged_ms == 0 || now.saturating_sub(kb.drop_logged_ms) >= DROP_LOG_MS {
            log::warn!(
                "input: device {}: events dropped; released its held keys",
                kb.id
            );
            kb.drop_logged_ms = now.max(1);
        }
    }
}

fn modifier_bit(code: u16) -> u8 {
    match code {
        evdev::KEY_LEFTCTRL => 1 << 0,
        evdev::KEY_RIGHTCTRL => 1 << 1,
        evdev::KEY_LEFTALT => 1 << 2,
        evdev::KEY_RIGHTALT => 1 << 3,
        evdev::KEY_LEFTMETA => 1 << 4,
        _ => 1 << 5,
    }
}

fn deliver_release(h: &Held, sink: &mut dyn Sink) {
    match h.target {
        Target::Screens { nav, ch } => sink.key(nav, ch, Value::Release),
        Target::Phone {
            select_sent: true, ..
        } => {
            sink.phone(PhoneButton::Select, Value::Release);
        }
        Target::Phone { .. } | Target::Volume { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev::*;
    use crate::test_sink::{Out, Rec};

    const A: u16 = 30;
    const F1: u16 = 59;

    struct Rig {
        keys: Keys,
        kb: Keyboard,
        ctl: Controls,
        sink: Rec,
    }

    fn rig(route: Route, phone_live: bool) -> Rig {
        Rig {
            keys: Keys::new(route),
            kb: Keyboard::new(7),
            ctl: Controls::default(),
            sink: Rec::new(phone_live),
        }
    }

    impl Rig {
        fn key(&mut self, code: u16, value: i32, now: u64) -> bool {
            self.keys.event(
                &mut self.kb,
                &RawEvent::new(EV_KEY, code, value),
                now,
                &mut self.ctl,
                &mut self.sink,
            )
        }
        fn ev(&mut self, kind: u16, code: u16, value: i32, now: u64) -> bool {
            self.keys.event(
                &mut self.kb,
                &RawEvent::new(kind, code, value),
                now,
                &mut self.ctl,
                &mut self.sink,
            )
        }
        fn tick(&mut self, now: u64) -> Option<u64> {
            self.keys.tick(now, &mut self.sink)
        }
        fn out(&mut self) -> Vec<Out> {
            self.sink.take()
        }
    }

    fn nav(n: Nav, v: Value) -> Out {
        Out::Key(Some(n), None, v)
    }

    #[test]
    fn navigation_press_and_release_reach_the_screens() {
        let mut r = rig(Route::Screens, false);
        r.key(KEY_UP, 1, 1000);
        r.key(KEY_UP, 0, 1050);
        assert_eq!(
            r.out(),
            vec![nav(Nav::Up, Value::Press), nav(Nav::Up, Value::Release)]
        );
        assert_eq!(r.tick(2000), None);
    }

    #[test]
    fn typed_characters_with_shift_caps_and_other_modifiers() {
        let mut r = rig(Route::Screens, false);
        r.key(A, 1, 1);
        r.key(A, 0, 2);
        r.key(KEY_LEFTSHIFT, 1, 3);
        r.key(A, 1, 4);
        r.key(A, 0, 5);
        r.key(KEY_LEFTSHIFT, 0, 6);
        r.key(KEY_CAPSLOCK, 1, 7);
        r.key(KEY_CAPSLOCK, 0, 8);
        r.key(A, 1, 9);
        r.key(A, 0, 10);
        r.key(KEY_LEFTCTRL, 1, 11);
        r.key(A, 1, 12); // Ctrl+A types nothing and means nothing
        r.key(A, 0, 13);
        r.key(KEY_LEFTCTRL, 0, 14);
        r.key(KEY_SPACE, 1, 15); // Space is Ok and types a space
        r.key(KEY_SPACE, 0, 16);
        assert_eq!(
            r.out(),
            vec![
                Out::Key(None, Some('a'), Value::Press),
                Out::Key(None, Some('a'), Value::Release),
                Out::Key(None, Some('A'), Value::Press),
                Out::Key(None, Some('A'), Value::Release),
                Out::Key(None, Some('A'), Value::Press),
                Out::Key(None, Some('A'), Value::Release),
                Out::Key(Some(Nav::Ok), Some(' '), Value::Press),
                Out::Key(Some(Nav::Ok), Some(' '), Value::Release),
            ]
        );
    }

    #[test]
    fn repeats_are_synthesised_and_the_kernels_ignored() {
        let mut r = rig(Route::Screens, false);
        r.key(KEY_DOWN, 1, 1000);
        assert_eq!(r.out(), vec![nav(Nav::Down, Value::Press)]);
        r.key(KEY_DOWN, 2, 1250); // the kernel's own repeat
        assert!(r.out().is_empty());
        assert_eq!(r.tick(1399), Some(1400));
        assert!(r.out().is_empty());
        assert_eq!(r.tick(1400), Some(1520));
        assert_eq!(r.out(), vec![nav(Nav::Down, Value::Repeat)]);
        assert_eq!(r.tick(1530), Some(1650));
        assert_eq!(r.out(), vec![nav(Nav::Down, Value::Repeat)]);
        r.key(KEY_DOWN, 0, 1600);
        assert_eq!(r.out(), vec![nav(Nav::Down, Value::Release)]);
        assert_eq!(r.tick(1700), None);
        assert!(r.out().is_empty());
        // Ok does not repeat; Backspace does.
        r.key(KEY_ENTER, 1, 2000);
        assert_eq!(r.tick(2500), None);
        r.key(KEY_ENTER, 0, 2501);
        r.key(KEY_BACKSPACE, 1, 3000);
        assert_eq!(r.tick(3000), Some(3400));
        r.out();
    }

    #[test]
    fn select_on_the_phone_is_held_and_owes_its_release_only_when_sent() {
        let mut r = rig(Route::Phone, true);
        r.key(KEY_ENTER, 1, 1);
        r.key(KEY_ENTER, 0, 2);
        assert_eq!(
            r.out(),
            vec![
                Out::Phone(PhoneButton::Select, Value::Press),
                Out::Phone(PhoneButton::Select, Value::Release),
            ]
        );
        r.sink.phone_live = false;
        r.key(KEY_OK, 1, 3);
        r.key(KEY_OK, 0, 4);
        assert_eq!(r.out(), vec![Out::Phone(PhoneButton::Select, Value::Press)]);
        // Back is one-shot; letters do nothing on the phone.
        r.sink.phone_live = true;
        r.key(KEY_ESC, 1, 5);
        r.key(KEY_ESC, 0, 6);
        r.key(A, 1, 7);
        r.key(A, 0, 8);
        assert_eq!(r.out(), vec![Out::Phone(PhoneButton::Back, Value::Press)]);
    }

    #[test]
    fn phone_arrows_repeat_at_their_own_pace() {
        let mut r = rig(Route::Phone, true);
        r.key(KEY_LEFT, 1, 1000);
        assert_eq!(r.tick(1000), Some(1400));
        assert_eq!(r.tick(1400), Some(1550));
        assert_eq!(r.tick(1550), Some(1700));
        assert_eq!(
            r.out(),
            vec![
                Out::Phone(PhoneButton::Left, Value::Press),
                Out::Phone(PhoneButton::Left, Value::Repeat),
                Out::Phone(PhoneButton::Left, Value::Repeat),
            ]
        );
    }

    #[test]
    fn menu_on_the_phone_brings_the_screens_up_on_home() {
        let mut r = rig(Route::Phone, true);
        r.key(KEY_COMPOSE, 1, 1);
        r.key(KEY_COMPOSE, 0, 2);
        assert_eq!(
            r.out(),
            vec![
                Out::Phone(PhoneButton::Menu, Value::Press),
                nav(Nav::Home, Value::Press),
            ]
        );
        r.sink.phone_live = false;
        r.key(KEY_MENU, 1, 3);
        assert_eq!(r.out(), vec![Out::Phone(PhoneButton::Menu, Value::Press)]);
    }

    #[test]
    fn media_keys_reach_the_phone_from_the_screens() {
        let mut r = rig(Route::Screens, true);
        r.key(KEY_NEXTSONG, 1, 1);
        r.key(KEY_NEXTSONG, 0, 2);
        r.key(KEY_PLAYPAUSE, 1, 3);
        r.key(KEY_VOICECOMMAND, 1, 4);
        assert_eq!(
            r.out(),
            vec![
                Out::Phone(PhoneButton::NextTrack, Value::Press),
                Out::Phone(PhoneButton::PlayPause, Value::Press),
                Out::Phone(PhoneButton::Siri, Value::Press),
            ]
        );
    }

    #[test]
    fn the_wheel_is_a_knob_capped_per_event() {
        let mut r = rig(Route::Screens, false);
        r.ev(EV_REL, REL_WHEEL, -3, 1); // towards the user: next
        assert_eq!(r.out(), vec![nav(Nav::Next, Value::Press); 3]);
        r.ev(EV_REL, REL_WHEEL, 20, 2); // a wild spin: eight at most
        assert_eq!(r.out(), vec![nav(Nav::Prev, Value::Press); 8]);
        r.ev(EV_REL, REL_WHEEL, 0, 3);
        assert!(r.out().is_empty());
    }

    #[test]
    fn wheel_turns_reach_the_phone_paced_and_bounded() {
        let mut r = rig(Route::Phone, true);
        r.ev(EV_REL, REL_WHEEL, -6, 1000); // six detents at once
        assert_eq!(
            r.out(),
            vec![Out::Phone(PhoneButton::RotateNext, Value::Press)]
        );
        assert_eq!(r.tick(1010), Some(1030));
        assert!(r.out().is_empty());
        // Four were kept: three more go out, one every 30 ms; two were dropped.
        assert_eq!(r.tick(1030), Some(1060));
        assert_eq!(r.tick(1060), Some(1090));
        assert_eq!(r.tick(1090), None);
        assert_eq!(
            r.out(),
            vec![Out::Phone(PhoneButton::RotateNext, Value::Press); 3]
        );
        // A turn the other way drops what was owed.
        r.ev(EV_REL, REL_WHEEL, -4, 2000);
        r.ev(EV_REL, REL_WHEEL, 1, 2001);
        assert_eq!(
            r.out(),
            vec![Out::Phone(PhoneButton::RotateNext, Value::Press)]
        );
        assert_eq!(r.tick(2030), None);
        assert_eq!(
            r.out(),
            vec![Out::Phone(PhoneButton::RotatePrev, Value::Press)]
        );
    }

    #[test]
    fn the_wheel_click_is_enter() {
        let mut r = rig(Route::Screens, false);
        r.key(BTN_MIDDLE, 1, 1);
        r.key(BTN_MIDDLE, 0, 2);
        assert_eq!(
            r.out(),
            vec![nav(Nav::Ok, Value::Press), nav(Nav::Ok, Value::Release)]
        );
    }

    #[test]
    fn volume_keys_repeat_while_held_and_give_up_after_a_while() {
        let mut r = rig(Route::Screens, false);
        r.key(KEY_VOLUMEUP, 1, 1000);
        assert_eq!(r.out(), vec![Out::Volume(1)]);
        assert_eq!(r.tick(1400), Some(1550));
        assert_eq!(r.out(), vec![Out::Volume(1)]);
        // A route change leaves a held volume key alone.
        r.keys.set_route(Route::Phone, &mut r.sink);
        assert_eq!(r.tick(1550), Some(1700));
        assert_eq!(r.out(), vec![Out::Volume(1)]);
        // Held past its time: no more until a fresh press.
        assert_eq!(r.tick(5000), None);
        assert!(r.out().is_empty());
        r.key(KEY_VOLUMEUP, 0, 5001);
        // Down while up still repeats: up's repeat is stopped.
        r.key(KEY_VOLUMEUP, 1, 6000);
        r.key(KEY_VOLUMEDOWN, 1, 6100);
        assert_eq!(r.tick(6500), Some(6650));
        assert_eq!(
            r.out(),
            vec![Out::Volume(1), Out::Volume(-1), Out::Volume(-1)]
        );
        r.key(KEY_MUTE, 1, 7000);
        assert_eq!(r.out(), vec![Out::Mute]);
        assert_eq!(r.tick(7000), None); // mute stopped the repeat and holds nothing
    }

    #[test]
    fn the_camera_key_toggles_the_camera_and_nothing_else() {
        let mut r = rig(Route::Screens, true);
        r.ctl.camera_key = KEY_UP;
        r.key(KEY_UP, 1, 1);
        r.key(KEY_UP, 0, 2);
        assert_eq!(r.out(), vec![Out::Camera]);
        assert_eq!(r.tick(1000), None);
        r.keys.set_route(Route::Phone, &mut r.sink);
        r.key(KEY_UP, 1, 3);
        assert_eq!(r.out(), vec![Out::Camera]);
        // No camera key configured: 0 never matches a key.
        r.ctl.camera_key = 0;
        r.key(KEY_UP, 1, 4);
        assert_eq!(r.out(), vec![Out::Phone(PhoneButton::Up, Value::Press)]);
    }

    #[test]
    fn learning_takes_the_next_key_but_esc_and_back_cancel() {
        let mut r = rig(Route::Screens, false);
        r.ctl.learning = true;
        r.key(KEY_ESC, 1, 1); // passes through: the screens close the question
        r.key(KEY_ESC, 0, 2);
        assert!(r.ctl.learning);
        assert_eq!(
            r.out(),
            vec![nav(Nav::Back, Value::Press), nav(Nav::Back, Value::Release)]
        );
        r.key(F1, 1, 3);
        r.key(F1, 0, 4);
        assert!(!r.ctl.learning);
        assert_eq!(r.out(), vec![Out::Learned(F1)]);
        // A gamepad button is learned as the key it stands for.
        r.ctl.learning = true;
        r.key(BTN_SOUTH, 1, 5);
        assert_eq!(r.out(), vec![Out::Learned(KEY_ENTER)]);
    }

    #[test]
    fn a_route_change_releases_what_the_old_route_holds() {
        let mut r = rig(Route::Phone, true);
        r.key(KEY_ENTER, 1, 1);
        r.ev(EV_REL, REL_WHEEL, -3, 2);
        r.out();
        r.keys.set_route(Route::Screens, &mut r.sink);
        assert_eq!(
            r.out(),
            vec![Out::Phone(PhoneButton::Select, Value::Release)]
        );
        assert_eq!(r.tick(100), None); // the owed turns are forgotten
        r.key(KEY_ENTER, 0, 3); // its own release finds nothing
        assert!(r.out().is_empty());
        r.key(KEY_UP, 1, 4);
        r.out();
        r.keys.set_route(Route::Phone, &mut r.sink);
        assert_eq!(r.out(), vec![nav(Nav::Up, Value::Release)]);
        assert_eq!(r.tick(1000), None);
    }

    #[test]
    fn dropped_events_release_the_device_and_reread_its_modifiers() {
        let mut r = rig(Route::Screens, false);
        r.key(KEY_UP, 1, 1);
        r.out();
        assert!(!r.ev(EV_SYN, SYN_DROPPED, 0, 2));
        r.key(KEY_DOWN, 1, 3); // the rest of the cut-short packet
        assert!(r.ev(EV_SYN, SYN_REPORT, 0, 4));
        assert!(r.out().is_empty());
        let mut state = [0usize; KEY_WORDS];
        let w = usize::BITS as usize;
        state[usize::from(KEY_LEFTSHIFT) / w] |= 1 << (usize::from(KEY_LEFTSHIFT) % w);
        r.keys.resync(&mut r.kb, &state, 5, &mut r.sink);
        assert_eq!(r.out(), vec![nav(Nav::Up, Value::Release)]);
        assert_eq!(r.tick(1000), None);
        r.key(A, 1, 6);
        assert_eq!(r.out(), vec![Out::Key(None, Some('A'), Value::Press)]);
    }

    #[test]
    fn a_device_that_goes_away_releases_its_keys_only() {
        let mut r = rig(Route::Screens, false);
        let mut other = Keyboard::new(8);
        r.key(KEY_UP, 1, 1);
        r.keys.event(
            &mut other,
            &RawEvent::new(EV_KEY, KEY_DOWN, 1),
            2,
            &mut r.ctl,
            &mut r.sink,
        );
        r.out();
        r.keys.device_gone(7, &mut r.sink);
        assert_eq!(r.out(), vec![nav(Nav::Up, Value::Release)]);
        r.keys.release_all(&mut r.sink);
        assert_eq!(r.out(), vec![nav(Nav::Down, Value::Release)]);
    }

    #[test]
    fn hats_are_arrows() {
        let mut r = rig(Route::Screens, false);
        r.ev(EV_ABS, ABS_HAT0Y, -1, 1);
        r.ev(EV_ABS, ABS_HAT0Y, -1, 2); // unchanged: nothing
        r.ev(EV_ABS, ABS_HAT0Y, 1, 3); // straight across: release, press
        r.ev(EV_ABS, ABS_HAT0Y, 0, 4);
        r.ev(EV_ABS, ABS_HAT0X, 1, 5);
        assert_eq!(
            r.out(),
            vec![
                nav(Nav::Up, Value::Press),
                nav(Nav::Up, Value::Release),
                nav(Nav::Down, Value::Press),
                nav(Nav::Down, Value::Release),
                nav(Nav::Right, Value::Press),
            ]
        );
    }

    #[test]
    fn a_second_press_without_a_release_replaces_the_first() {
        let mut r = rig(Route::Screens, false);
        r.key(KEY_UP, 1, 1);
        r.key(KEY_UP, 1, 2);
        assert_eq!(
            r.out(),
            vec![
                nav(Nav::Up, Value::Press),
                nav(Nav::Up, Value::Release),
                nav(Nav::Up, Value::Press),
            ]
        );
        assert_eq!(r.tick(401), Some(402)); // the second press set the time
        assert!(r.out().is_empty());
        assert_eq!(r.tick(402), Some(522)); // one entry repeats, not two
        assert_eq!(r.out(), vec![nav(Nav::Up, Value::Repeat)]);
    }

    #[test]
    fn the_held_table_is_bounded() {
        let mut r = rig(Route::Screens, false);
        let codes: Vec<u16> = (16..=27).chain(30..=34).collect(); // qwertyuiop[] asdfg
        assert_eq!(codes.len(), MAX_HELD + 1);
        for &code in &codes {
            r.key(code, 1, 1);
        }
        let out = r.out();
        assert_eq!(out.len(), MAX_HELD + 2); // the presses, plus one eviction
        assert_eq!(out[MAX_HELD], Out::Key(None, Some('g'), Value::Press));
        assert_eq!(out[MAX_HELD + 1], Out::Key(None, Some('q'), Value::Release));
    }

    #[test]
    fn codes_past_the_table_are_ignored() {
        let mut r = rig(Route::Screens, false);
        r.key(0x3ff, 1, 1);
        r.key(KEY_UP, 3, 2); // not a value evdev defines
        assert!(r.out().is_empty());
    }
}
