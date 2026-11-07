//! Input, as the main loop receives it: the input thread calls the `Sink`
//! and returns at once; each call becomes one fixed-size `Event` in a
//! bounded queue and a wake of the loop, which drains the queue on its next
//! turn. Nothing here allocates per event.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lehu_core::poll::Wake;
use lehu_core::queue::Queue;
use lehu_input::{Contact, MAX_SLOTS, Nav, PhoneButton, Route, Sink, Value};

/// One input event for the loop. Touch frames carry every finger of one
/// evdev report, so the phone gets its multitouch whole.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Touch {
        route: Route,
        count: u8,
        contacts: [Contact; MAX_SLOTS],
    },
    Key {
        nav: Option<Nav>,
        ch: Option<char>,
        value: Value,
    },
    Phone {
        button: PhoneButton,
        value: Value,
    },
    Volume(i8),
    Mute,
    CameraToggle,
    KeyLearned(u16),
    /// From the USB watcher thread, not the input thread; it rides the same
    /// queue so the loop has one wake to drain.
    Usb(lehu_usb::Event),
    /// From the Bluetooth manager's thread: a phone connected or left.
    BtPhone(lehu_bt::Address, bool),
}

/// The queue's depth: a burst of touch frames between two loop turns is a
/// handful; keys and buttons are rarer still. Over that, the oldest touch
/// frame goes (the next one supersedes it), never a key.
const DEPTH: usize = 64;

/// The loop's end of the input: the queue, the wake, and the one fact the
/// input thread needs synchronously, whether a phone session takes buttons.
#[derive(Clone)]
pub struct Hub {
    queue: Arc<Queue<Event>>,
    wake: Arc<Wake>,
    phone_takes: Arc<AtomicBool>,
}

impl Hub {
    pub fn new(wake: Arc<Wake>) -> Self {
        Self {
            queue: Arc::new(Queue::new(DEPTH)),
            wake,
            phone_takes: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Tell the input thread whether a phone session with video focus is
    /// there to take buttons; read by `Sink::phone` on its thread.
    pub fn set_phone_takes(&self, takes: bool) {
        self.phone_takes.store(takes, Ordering::Release);
    }

    /// The next event, if any; the loop calls this until `None` after a
    /// wake.
    pub fn pop(&self) -> Option<Event> {
        self.queue.pop()
    }

    pub fn dropped(&self) -> u32 {
        self.queue.drops()
    }

    /// The USB watcher's events, from its own thread.
    pub fn usb(&self, event: lehu_usb::Event) {
        self.push(Event::Usb(event));
    }

    /// The Bluetooth manager's phone events, from its own thread.
    pub fn bt_phone(&self, address: lehu_bt::Address, connected: bool) {
        self.push(Event::BtPhone(address, connected));
    }

    fn push(&self, event: Event) {
        // The queue is far deeper than any burst between two loop turns; a
        // drop means the loop stalled, and the oldest event is the one whose
        // moment has passed.
        if !self.queue.push_drop_oldest(event) {
            log::debug!("input: the loop fell behind, an event was dropped");
        }
        self.wake.wake();
    }
}

impl Sink for Hub {
    fn touch(&mut self, route: Route, contacts: &[Contact]) {
        let mut frame = [Contact::default(); MAX_SLOTS];
        let count = contacts.len().min(MAX_SLOTS);
        frame[..count].copy_from_slice(&contacts[..count]);
        self.push(Event::Touch {
            route,
            count: count as u8,
            contacts: frame,
        });
    }

    fn key(&mut self, nav: Option<Nav>, ch: Option<char>, value: Value) {
        self.push(Event::Key { nav, ch, value });
    }

    fn phone(&mut self, button: PhoneButton, value: Value) -> bool {
        let takes = self.phone_takes.load(Ordering::Acquire);
        if takes {
            self.push(Event::Phone { button, value });
        }
        takes
    }

    fn volume(&mut self, step: i8) {
        self.push(Event::Volume(step));
    }

    fn mute(&mut self) {
        self.push(Event::Mute);
    }

    fn camera_toggle(&mut self) {
        self.push(Event::CameraToggle);
    }

    fn key_learned(&mut self, code: u16) {
        self.push(Event::KeyLearned(code));
    }
}
