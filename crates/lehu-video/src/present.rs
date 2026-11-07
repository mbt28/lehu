//! Which decoded pictures are on the plane, waiting for it, or free to go
//! back to the decoder. Pure bookkeeping over numbered slots, so it is
//! tested with a fake display and fake slots.
//!
//! Every picture is decoded (each one is a reference) but only the newest
//! is shown. A picture the display cannot take yet (`Busy`: the previous
//! flip is pending, or the camera has the plane) waits as the pending one,
//! where a newer picture supersedes it, and is offered again after the flip.
//!
//! The last three shown pictures stay held. Non-blocking flips leave commit
//! N scanning while N+1 is queued, and the sun4i CRTC signals a commit's
//! flip one frame early when it starts before the previous one has latched
//! (`sun4i_crtc_atomic_begin` publishes the new event before it waits for
//! the previous LOADCTL, so the vblank that latches N also completes N+1).
//! In continuous motion that state persists and the picture two commits
//! back can still be on screen; a buffer returned to the decoder then is
//! written mid-scan, a torn frame. Three deep covers it.
//!
//! The retry may run on the main thread (after `Display::handle_events`),
//! but the decoder's references are only touched on the decode thread: a
//! slot the retry frees is marked stale and released at the decode thread's
//! next turn (`settle`).

use std::sync::atomic::{AtomicU32, Ordering};

use lehu_drm::{Display, Frame, Show};

use crate::ffi::{SLOTS, Slots};

/// Shown pictures kept referenced.
pub const HELD: usize = 3;
const _: () = assert!(SLOTS == HELD + 1, "three held and one pending");

/// Where a picture can be shown: the display, or a test double.
pub trait Screen {
    fn show(&self, frame: &Frame) -> Show;
}

impl Screen for Display {
    fn show(&self, frame: &Frame) -> Show {
        self.show_video(frame)
    }
}

/// Counts shared with whoever reads the statistics; the presenter adds to
/// them from either thread.
#[derive(Default)]
pub struct Counts {
    pub shown: AtomicU32,
    pub superseded: AtomicU32,
}

pub struct Presenter {
    /// What each slot holds; Some means the decoder's reference is held.
    slots: [Option<Frame>; SLOTS],
    /// Slots freed by a retry on another thread, to be released on the
    /// decode thread.
    stale: [bool; SLOTS],
    /// The last shown pictures, in order; `ring_next` is the oldest.
    ring: [Option<usize>; HELD],
    ring_next: usize,
    pending: Option<usize>,
}

impl Default for Presenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Presenter {
    pub fn new() -> Self {
        Self {
            slots: [None; SLOTS],
            stale: [false; SLOTS],
            ring: [None; HELD],
            ring_next: 0,
            pending: None,
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }

    /// The oldest held picture goes back to the decoder: now, or when the
    /// decode thread next settles.
    fn evict(&mut self, slots: Option<&mut dyn Slots>) {
        if let Some(old) = self.ring[self.ring_next].take() {
            match slots {
                Some(s) => {
                    s.release(old);
                    self.slots[old] = None;
                }
                None => self.stale[old] = true,
            }
        }
    }

    /// `slot` joins the ring of shown pictures.
    fn shown(&mut self, slot: usize, slots: Option<&mut dyn Slots>, counts: &Counts) {
        self.evict(slots);
        self.ring[self.ring_next] = Some(slot);
        self.ring_next = (self.ring_next + 1) % HELD;
        counts.shown.fetch_add(1, Ordering::Relaxed);
    }

    /// The pending picture never reached the screen: it is superseded.
    fn drop_pending(&mut self, slots: &mut dyn Slots, counts: &Counts) {
        if let Some(p) = self.pending.take() {
            slots.release(p);
            self.slots[p] = None;
            counts.superseded.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Release the slots a retry freed. Decode thread only.
    pub fn settle(&mut self, slots: &mut dyn Slots) {
        for i in 0..SLOTS {
            if self.stale[i] {
                self.stale[i] = false;
                slots.release(i);
                self.slots[i] = None;
            }
        }
    }

    /// A picture just decoded, in hand. Decode thread only.
    pub fn present(
        &mut self,
        frame: Frame,
        screen: &dyn Screen,
        slots: &mut dyn Slots,
        counts: &Counts,
    ) {
        self.settle(slots);
        match screen.show(&frame) {
            Show::Shown => {
                // Older than what is on screen now.
                self.drop_pending(slots, counts);
                if self.free_slot().is_none() {
                    self.evict(Some(slots));
                }
                let Some(s) = self.free_slot() else { return };
                slots.hold(s);
                self.slots[s] = Some(frame);
                self.shown(s, Some(slots), counts);
            }
            Show::Busy => {
                // The display is still flipping to an earlier picture: this
                // is the newest candidate, shown after the flip.
                self.drop_pending(slots, counts);
                let Some(s) = self.free_slot() else { return };
                slots.hold(s);
                self.slots[s] = Some(frame);
                self.pending = Some(s);
            }
            // The reference dies with the scratch frame.
            Show::Failed => {}
        }
    }

    /// Offer the pending picture again, after a flip. Any thread: with
    /// `slots` the freed buffers go back now, without them at the next
    /// `settle`.
    pub fn retry(&mut self, screen: &dyn Screen, slots: Option<&mut dyn Slots>, counts: &Counts) {
        let Some(p) = self.pending else { return };
        let Some(frame) = self.slots[p] else {
            self.pending = None;
            return;
        };
        match screen.show(&frame) {
            Show::Shown => {
                self.pending = None;
                self.shown(p, slots, counts);
            }
            Show::Busy => {}
            Show::Failed => {
                self.pending = None;
                match slots {
                    Some(s) => {
                        s.release(p);
                        self.slots[p] = None;
                    }
                    None => self.stale[p] = true,
                }
            }
        }
    }

    /// Everything back to the decoder, before it is closed.
    pub fn release_all(&mut self, slots: &mut dyn Slots) {
        for i in 0..SLOTS {
            if self.slots[i].is_some() {
                slots.release(i);
                self.slots[i] = None;
            }
            self.stale[i] = false;
        }
        self.ring = [None; HELD];
        self.ring_next = 0;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use lehu_drm::Plane;

    use super::*;

    /// A display answering a scripted sequence (the last answer repeats).
    struct Script(RefCell<Vec<Show>>, Cell<u32>);

    impl Script {
        fn new(answers: &[Show]) -> Self {
            let mut v = answers.to_vec();
            v.reverse();
            Self(RefCell::new(v), Cell::new(0))
        }
        fn calls(&self) -> u32 {
            self.1.get()
        }
    }

    impl Screen for Script {
        fn show(&self, _: &Frame) -> Show {
            self.1.set(self.1.get() + 1);
            let mut v = self.0.borrow_mut();
            if v.len() > 1 { v.pop().unwrap() } else { v[0] }
        }
    }

    /// Slots that record what the presenter did and which are referenced.
    #[derive(Default)]
    struct Fake {
        held: [bool; SLOTS],
        log: Vec<String>,
    }

    impl Slots for Fake {
        fn hold(&mut self, slot: usize) {
            assert!(!self.held[slot], "slot {slot} held twice");
            self.held[slot] = true;
            self.log.push(format!("hold {slot}"));
        }
        fn release(&mut self, slot: usize) {
            self.held[slot] = false;
            self.log.push(format!("release {slot}"));
        }
    }

    impl Fake {
        fn held_count(&self) -> usize {
            self.held.iter().filter(|&&h| h).count()
        }
    }

    fn frame(id: i32) -> Frame {
        let mut planes = [Plane::default(); lehu_drm::MAX_PLANES];
        planes[0].fd = id;
        Frame {
            fourcc: 0x3231_564e,
            width: 800,
            height: 480,
            planes,
            plane_count: 1,
            modifier: 0,
        }
    }

    #[test]
    fn three_shown_pictures_stay_held_and_the_fourth_frees_the_oldest() {
        let screen = Script::new(&[Show::Shown]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        for i in 0..3 {
            p.present(frame(i), &screen, &mut slots, &counts);
            assert_eq!(slots.held_count(), (i + 1) as usize);
        }
        p.present(frame(3), &screen, &mut slots, &counts);
        // The fourth goes into the spare slot and the oldest leaves.
        assert_eq!(slots.held_count(), HELD);
        let n = slots.log.len();
        assert_eq!(&slots.log[n - 2..], ["hold 3", "release 0"]);
        assert!(!slots.held[0] && slots.held[1] && slots.held[2] && slots.held[3]);
        assert_eq!(counts.shown.load(Ordering::Relaxed), 4);
        assert_eq!(counts.superseded.load(Ordering::Relaxed), 0);
        assert!(!p.has_pending());
    }

    #[test]
    fn a_busy_display_keeps_the_newest_as_pending_and_counts_the_superseded() {
        let screen = Script::new(&[Show::Busy]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        p.present(frame(1), &screen, &mut slots, &counts);
        assert!(p.has_pending());
        assert_eq!(slots.held_count(), 1);
        p.present(frame(2), &screen, &mut slots, &counts);
        p.present(frame(3), &screen, &mut slots, &counts);
        // One pending at a time: the earlier ones went back.
        assert_eq!(slots.held_count(), 1);
        assert_eq!(counts.superseded.load(Ordering::Relaxed), 2);
        assert_eq!(counts.shown.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_retry_on_the_decode_thread_moves_the_pending_into_the_ring() {
        let screen = Script::new(&[Show::Busy, Show::Shown]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        p.present(frame(1), &screen, &mut slots, &counts);
        p.retry(&screen, Some(&mut slots), &counts);
        assert!(!p.has_pending());
        assert_eq!(counts.shown.load(Ordering::Relaxed), 1);
        assert_eq!(slots.held_count(), 1);
        // A retry with nothing pending costs nothing.
        let before = screen.calls();
        p.retry(&screen, Some(&mut slots), &counts);
        assert_eq!(screen.calls(), before);
    }

    #[test]
    fn a_retry_on_another_thread_defers_the_release_to_settle() {
        // Three shown, then one busy: all four slots in use.
        let screen = Script::new(&[
            Show::Shown,
            Show::Shown,
            Show::Shown,
            Show::Busy,
            Show::Shown,
        ]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        for i in 0..4 {
            p.present(frame(i), &screen, &mut slots, &counts);
        }
        assert!(p.has_pending());
        assert_eq!(slots.held_count(), SLOTS);
        // The main loop's retry: the oldest leaves the ring but stays
        // referenced until the decode thread settles.
        let n = slots.log.len();
        p.retry(&screen, None, &counts);
        assert!(!p.has_pending());
        assert_eq!(slots.log.len(), n, "no slot op off the decode thread");
        assert_eq!(slots.held_count(), SLOTS);
        p.settle(&mut slots);
        assert_eq!(slots.held_count(), HELD);
        assert_eq!(slots.log.last().map(String::as_str), Some("release 0"));
        assert_eq!(counts.shown.load(Ordering::Relaxed), 4);
        // The next picture takes the freed slot and the ring stays three.
        p.present(frame(9), &screen, &mut slots, &counts);
        assert!(slots.held[0]);
        assert_eq!(slots.held_count(), HELD);
    }

    #[test]
    fn a_shown_picture_supersedes_the_pending_one() {
        let screen = Script::new(&[Show::Busy, Show::Shown]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        p.present(frame(1), &screen, &mut slots, &counts);
        p.present(frame(2), &screen, &mut slots, &counts);
        assert!(!p.has_pending());
        assert_eq!(counts.superseded.load(Ordering::Relaxed), 1);
        assert_eq!(counts.shown.load(Ordering::Relaxed), 1);
        assert_eq!(slots.held_count(), 1);
    }

    #[test]
    fn a_failed_show_holds_nothing_and_a_failed_retry_frees_the_pending() {
        let screen = Script::new(&[Show::Failed]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        p.present(frame(1), &screen, &mut slots, &counts);
        assert_eq!(slots.held_count(), 0);
        assert!(slots.log.is_empty());

        let screen = Script::new(&[Show::Busy, Show::Failed]);
        p.present(frame(2), &screen, &mut slots, &counts);
        p.retry(&screen, None, &counts);
        assert!(!p.has_pending());
        assert_eq!(slots.held_count(), 1, "released only when settled");
        p.settle(&mut slots);
        assert_eq!(slots.held_count(), 0);
    }

    #[test]
    fn release_all_returns_everything() {
        let screen = Script::new(&[Show::Shown, Show::Shown, Show::Busy]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        for i in 0..3 {
            p.present(frame(i), &screen, &mut slots, &counts);
        }
        assert_eq!(slots.held_count(), 3);
        p.release_all(&mut slots);
        assert_eq!(slots.held_count(), 0);
        assert!(!p.has_pending());
        // Usable again afterwards.
        p.present(frame(7), &Script::new(&[Show::Shown]), &mut slots, &counts);
        assert_eq!(slots.held_count(), 1);
    }

    #[test]
    fn a_long_busy_run_never_holds_more_than_the_slots() {
        let screen = Script::new(&[Show::Busy]);
        let mut slots = Fake::default();
        let counts = Counts::default();
        let mut p = Presenter::new();
        for i in 0..100 {
            p.present(frame(i), &screen, &mut slots, &counts);
            p.retry(&screen, None, &counts);
            assert!(slots.held_count() <= SLOTS);
        }
        assert_eq!(slots.held_count(), 1);
        assert_eq!(counts.superseded.load(Ordering::Relaxed), 99);
    }
}
