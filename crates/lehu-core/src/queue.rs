//! Bounded queues between threads.
//!
//! A producer on the wire must never block or allocate: it pushes and is
//! told whether something was dropped. A consumer sleeps until there is
//! work. The capacity is fixed when the queue is made and nothing is
//! allocated after that; what happens when it is full is the producer's
//! choice, per push:
//!
//! - `push_discard`: the new item is dropped. Right for status that will be
//!   sent again.
//! - `push_drop_oldest`: the oldest item makes room. Right for video, where
//!   a dropped frame costs latency that a keyframe recovers, and for audio.
//! - `push_wait_drop_oldest`: wait a while for the consumer first, so that
//!   back-pressure reaches the phone (CarPlay video over TCP), then drop the
//!   oldest rather than wedge the producer.
//!
//! `close` wakes every waiting consumer and makes waits return at once;
//! pushes still land, so a stream can be closed and reopened without losing
//! what arrived meanwhile, as FastCarPlay's AtomicQueue did.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Queue<T> {
    items: Mutex<VecDeque<T>>,
    /// Signalled when an item arrives or the queue closes.
    ready: Condvar,
    /// Signalled when an item leaves.
    space: Condvar,
    capacity: usize,
    drops: AtomicU32,
    closed: AtomicBool,
}

impl<T> Queue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a queue holds at least one item");
        Self {
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            ready: Condvar::new(),
            space: Condvar::new(),
            capacity,
            drops: AtomicU32::new(0),
            closed: AtomicBool::new(false),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// At least `n` items queued.
    pub fn has(&self, n: usize) -> bool {
        self.len() >= n
    }

    /// Items dropped by full-queue pushes since creation.
    pub fn drops(&self) -> u32 {
        self.drops.load(Ordering::Relaxed)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Push; when full, the new item is dropped. False when it was.
    pub fn push_discard(&self, item: T) -> bool {
        let mut items = self.lock();
        if items.len() >= self.capacity {
            drop(items);
            self.drops.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        items.push_back(item);
        drop(items);
        self.ready.notify_one();
        true
    }

    /// Push; when full, the oldest item makes room. False when one did.
    pub fn push_drop_oldest(&self, item: T) -> bool {
        let mut items = self.lock();
        let dropped = if items.len() >= self.capacity {
            items.pop_front();
            true
        } else {
            false
        };
        items.push_back(item);
        drop(items);
        if dropped {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
        self.ready.notify_one();
        !dropped
    }

    /// Wait up to `timeout` while `depth` or more items are queued, then
    /// push, the oldest making room if the queue is still full. False when
    /// something was dropped.
    pub fn push_wait_drop_oldest(&self, item: T, depth: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut items = self.lock();
        // A closed queue has no consumer to wait for: fall through to the
        // drop, so a stream reader is never held 500 ms per frame after a
        // disconnect.
        while items.len() >= depth.max(1) && !self.is_closed() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .space
                .wait_timeout(items, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            items = guard;
        }
        let dropped = if items.len() >= self.capacity {
            items.pop_front();
            true
        } else {
            false
        };
        items.push_back(item);
        drop(items);
        if dropped {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
        self.ready.notify_one();
        !dropped
    }

    /// The oldest item, if any, without waiting.
    pub fn pop(&self) -> Option<T> {
        let item = self.lock().pop_front();
        if item.is_some() {
            self.space.notify_one();
        }
        item
    }

    /// The oldest item, waiting up to `timeout` for one. `None` on timeout
    /// or when the queue is closed and empty.
    pub fn pop_wait(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut items = self.lock();
        loop {
            if let Some(item) = items.pop_front() {
                drop(items);
                self.space.notify_one();
                return Some(item);
            }
            if self.is_closed() {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self
                .ready
                .wait_timeout(items, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            items = guard;
        }
    }

    /// Look at the oldest item without taking it.
    pub fn peek<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        f(self.lock().front())
    }

    /// Drop everything queued; how many there were.
    pub fn clear(&self) -> usize {
        let mut items = self.lock();
        let n = items.len();
        items.clear();
        drop(items);
        if n > 0 {
            self.space.notify_all();
        }
        n
    }

    /// Wake every waiting consumer and make waits return at once. Pushes
    /// still land.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_all();
        self.release_waiters();
    }

    pub fn reopen(&self) {
        self.closed.store(false, Ordering::Release);
    }

    /// Wake a producer waiting in `push_wait_drop_oldest` (used by `close`).
    fn release_waiters(&self) {
        self.space.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<T>> {
        // A panic while holding the lock leaves the deque consistent: every
        // operation here is a single push or pop.
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn discard_drops_the_new_item() {
        let q = Queue::new(2);
        assert!(q.push_discard(1));
        assert!(q.push_discard(2));
        assert!(!q.push_discard(3));
        assert_eq!(q.drops(), 1);
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn drop_oldest_keeps_the_new_item() {
        let q = Queue::new(2);
        q.push_drop_oldest(1);
        q.push_drop_oldest(2);
        assert!(!q.push_drop_oldest(3));
        assert_eq!(q.drops(), 1);
        assert_eq!(q.len(), 2);
        assert!(q.has(2));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
    }

    #[test]
    fn wait_push_waits_for_the_consumer() {
        let q = Arc::new(Queue::new(4));
        q.push_drop_oldest(1);
        q.push_drop_oldest(2);
        let consumer = {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                q.pop()
            })
        };
        let start = Instant::now();
        assert!(q.push_wait_drop_oldest(3, 2, Duration::from_secs(2)));
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "it waited for the pop"
        );
        assert_eq!(consumer.join().unwrap(), Some(1));
        assert_eq!(q.drops(), 0);
    }

    #[test]
    fn wait_push_gives_up_and_drops_the_oldest() {
        let q = Queue::new(2);
        q.push_drop_oldest(1);
        q.push_drop_oldest(2);
        assert!(!q.push_wait_drop_oldest(3, 2, Duration::from_millis(20)));
        assert_eq!(q.drops(), 1);
        assert_eq!(q.pop(), Some(2));
    }

    #[test]
    fn pop_wait_times_out_and_wakes_on_push() {
        let q = Arc::new(Queue::<u8>::new(1));
        let start = Instant::now();
        assert_eq!(q.pop_wait(Duration::from_millis(30)), None);
        assert!(start.elapsed() >= Duration::from_millis(25));
        let producer = {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(30));
                q.push_discard(7);
            })
        };
        assert_eq!(q.pop_wait(Duration::from_secs(2)), Some(7));
        producer.join().unwrap();
    }

    #[test]
    fn close_wakes_a_waiting_consumer_and_pushes_still_land() {
        let q = Arc::new(Queue::<u8>::new(2));
        let waiter = {
            let q = Arc::clone(&q);
            thread::spawn(move || q.pop_wait(Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(30));
        q.close();
        assert_eq!(waiter.join().unwrap(), None);
        assert!(q.push_discard(9));
        assert_eq!(q.pop_wait(Duration::from_millis(10)), Some(9));
        q.reopen();
        assert!(!q.is_closed());
    }

    #[test]
    fn wait_push_does_not_wait_on_a_closed_queue() {
        let q = Queue::new(2);
        q.push_drop_oldest(1);
        q.push_drop_oldest(2);
        q.close();
        let start = Instant::now();
        assert!(!q.push_wait_drop_oldest(3, 2, Duration::from_secs(2)));
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "it fell through at once"
        );
        assert_eq!(q.pop(), Some(2));
    }

    #[test]
    fn peek_and_clear() {
        let q = Queue::new(3);
        q.push_discard("a");
        q.push_discard("b");
        assert_eq!(q.peek(|h| h.copied()), Some("a"));
        assert_eq!(q.clear(), 2);
        assert!(q.is_empty());
        assert_eq!(q.peek(|h| h.copied()), None);
    }
}
