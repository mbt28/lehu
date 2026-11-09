// SPDX-License-Identifier: GPL-3.0-or-later
//! Touch and keys from the input threads to the loop thread, where Slint
//! wants them. The pointer is last-state atomics -- a position and whether
//! it is down, plus a flag for a tap that fit between two ticks -- because
//! only the latest state matters; keys are discrete events, so a small FIFO:
//! several can land within one tick. `Osd::tick` drains both.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use crate::keys::OsdKey;

/// Keys queued between two ticks. A full queue drops the newest: the UI is
/// not ticking (or not keeping up), and replaying a burst later would only
/// surprise.
pub(crate) const KEY_QUEUE: usize = 32;

/// One key event as the reader saw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyEvent {
    pub nav: OsdKey,
    /// The typed character, if the key has one.
    pub ch: Option<char>,
    /// The evdev value: 1 press, 2 repeat, 0 release.
    pub value: i8,
}

/// The pointer as the last tick found it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pointer {
    /// A press happened since the last tick (even if it is up again).
    pub seen: bool,
    pub down: bool,
    pub x: i32,
    pub y: i32,
}

struct Keys {
    events: [KeyEvent; KEY_QUEUE],
    count: usize,
}

struct Shared {
    x: AtomicI32,
    y: AtomicI32,
    pressed: AtomicBool,
    /// A press shorter than one tick still delivers a press and a release.
    press_seen: AtomicBool,
    keys: Mutex<Keys>,
}

/// The input side of the screens: cheap to clone, safe to use from any
/// thread. Everything it records is applied by the next `Osd::tick`.
#[derive(Clone)]
pub struct Input(Arc<Shared>);

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Input {
    pub fn new() -> Self {
        Self(Arc::new(Shared {
            x: AtomicI32::new(0),
            y: AtomicI32::new(0),
            pressed: AtomicBool::new(false),
            press_seen: AtomicBool::new(false),
            keys: Mutex::new(Keys {
                events: [KeyEvent::default(); KEY_QUEUE],
                count: 0,
            }),
        }))
    }

    /// The touch point, in display pixels, and whether it is down.
    pub fn pointer(&self, x: i32, y: i32, pressed: bool) {
        // Position first, then the state, then the catch-up flag: tick()
        // clears the flag before it reads the state, so a press can never be
        // seen by the catch-up branch while the state still reads up (which
        // would deliver the tap twice).
        self.0.x.store(x, Ordering::SeqCst);
        self.0.y.store(y, Ordering::SeqCst);
        self.0.pressed.store(pressed, Ordering::SeqCst);
        if pressed {
            self.0.press_seen.store(true, Ordering::SeqCst);
        }
    }

    /// A key: its navigation meaning, the typed character (if any) and the
    /// evdev value (1 press, 2 repeat, 0 release). Queued in order.
    pub fn key(&self, nav: OsdKey, ch: Option<char>, value: i32) {
        let mut keys = self.lock();
        let count = keys.count;
        if count < KEY_QUEUE {
            keys.events[count] = KeyEvent {
                nav,
                ch,
                value: value.clamp(0, 2) as i8,
            };
            keys.count = count + 1;
        }
    }

    /// The pointer state, clearing the catch-up flag before reading the
    /// rest (see `pointer`).
    pub(crate) fn take_pointer(&self) -> Pointer {
        let seen = self.0.press_seen.swap(false, Ordering::SeqCst);
        let down = self.0.pressed.load(Ordering::SeqCst);
        Pointer {
            seen,
            down,
            x: self.0.x.load(Ordering::SeqCst),
            y: self.0.y.load(Ordering::SeqCst),
        }
    }

    /// The queued keys, copied into `out`; how many there are.
    pub(crate) fn take_keys(&self, out: &mut [KeyEvent; KEY_QUEUE]) -> usize {
        let mut keys = self.lock();
        let count = keys.count;
        out[..count].copy_from_slice(&keys.events[..count]);
        keys.count = 0;
        count
    }

    /// Drop the queued keys (the UI was hidden: they are stale).
    pub(crate) fn clear_keys(&self) {
        self.lock().count = 0;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keys> {
        // A panic while holding it left plain data behind: still usable.
        self.0.keys.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_queue_in_order_and_drop_the_newest_when_full() {
        let input = Input::new();
        for i in 0..40 {
            input.key(OsdKey::Down, Some(char::from(b'a' + (i % 26) as u8)), 1);
        }
        let mut out = [KeyEvent::default(); KEY_QUEUE];
        assert_eq!(input.take_keys(&mut out), KEY_QUEUE);
        assert_eq!(out[0].ch, Some('a'));
        assert_eq!(out[KEY_QUEUE - 1].ch, Some(char::from(b'a' + 31 % 26)));
        assert_eq!(input.take_keys(&mut out), 0);
    }

    #[test]
    fn a_tap_between_ticks_is_still_seen() {
        let input = Input::new();
        input.pointer(10, 20, true);
        input.pointer(10, 20, false);
        let p = input.take_pointer();
        assert!(p.seen && !p.down);
        assert_eq!((p.x, p.y), (10, 20));
        assert!(!input.take_pointer().seen);
    }

    #[test]
    fn hidden_clears_the_queue() {
        let input = Input::new();
        input.key(OsdKey::Home, None, 1);
        input.clear_keys();
        let mut out = [KeyEvent::default(); KEY_QUEUE];
        assert_eq!(input.take_keys(&mut out), 0);
    }
}
