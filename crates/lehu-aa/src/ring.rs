// SPDX-License-Identifier: GPL-3.0-or-later
//! The slot ring between the USB read thread and the frame reader: a fixed
//! set of transfer buffers that cycle from free to in flight to filled to
//! consumed, with no copy and no allocation after the ring is built. The
//! read thread takes free buffers and commits filled ones; the reader
//! consumes them byte-exact, waiting when none is filled.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use nusb::transfer::Buffer;

struct State {
    free: Vec<Buffer>,
    /// Filled buffers in arrival order; `head_off` is how much of the
    /// front one was consumed.
    filled: VecDeque<Buffer>,
    head_off: usize,
}

pub struct Ring {
    state: Mutex<State>,
    /// A buffer was filled, or the ring closed.
    filled: Condvar,
    /// A buffer was freed, or the ring closed.
    freed: Condvar,
    closed: AtomicBool,
}

impl Ring {
    /// `count` buffers of `size` bytes each; the size is what one bulk IN
    /// transfer asks for, so it is a multiple of the packet size.
    pub fn new(count: usize, size: usize) -> Self {
        let count = count.max(2);
        Self {
            state: Mutex::new(State {
                free: (0..count).map(|_| Buffer::new(size)).collect(),
                filled: VecDeque::with_capacity(count),
                head_off: 0,
            }),
            filled: Condvar::new(),
            freed: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// A free buffer for the next transfer, waiting while the reader is
    /// behind; `None` once the ring is closed.
    pub fn take_free(&self) -> Option<Buffer> {
        let mut s = self.lock();
        loop {
            if self.is_closed() {
                return None;
            }
            if let Some(b) = s.free.pop() {
                return Some(b);
            }
            s = self
                .freed
                .wait_timeout(s, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// A transfer completed into `buf`, whose length is what arrived. An
    /// empty one goes straight back to the free set.
    pub fn commit(&self, buf: Buffer) {
        let mut s = self.lock();
        if buf.is_empty() {
            s.free.push(buf);
            drop(s);
            self.freed.notify_one();
            return;
        }
        s.filled.push_back(buf);
        drop(s);
        self.filled.notify_one();
    }

    /// Fill `dst` exactly; false when the ring closed first.
    pub fn read_exact(&self, dst: &mut [u8]) -> bool {
        let mut done = 0;
        let mut s = self.lock();
        while done < dst.len() {
            let Some(front) = s.filled.front() else {
                if self.is_closed() {
                    return false;
                }
                s = self.filled.wait(s).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let off = s.head_off;
            let avail = &front[off..];
            let n = avail.len().min(dst.len() - done);
            dst[done..done + n].copy_from_slice(&avail[..n]);
            done += n;
            if off + n == front.len() {
                let mut b = s.filled.pop_front().unwrap_or_else(|| Buffer::new(0));
                b.clear();
                s.free.push(b);
                s.head_off = 0;
                self.freed.notify_one();
            } else {
                s.head_off = off + n;
            }
        }
        true
    }

    /// Filled buffers waiting, for the log.
    pub fn backlog(&self) -> usize {
        self.lock().filled.len()
    }

    /// End every wait, now and later.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _guard = self.lock();
        self.filled.notify_all();
        self.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn filled(bytes: &[u8]) -> Buffer {
        let mut b = Buffer::new(bytes.len().max(1));
        b.extend_from_slice(bytes);
        b
    }

    #[test]
    fn bytes_come_out_exact_across_buffers() {
        let ring = Ring::new(2, 8);
        let a = ring.take_free().unwrap();
        let b = ring.take_free().unwrap();
        drop((a, b));
        ring.commit(filled(&[1, 2, 3]));
        ring.commit(filled(&[4, 5, 6, 7]));
        let mut out = [0u8; 2];
        assert!(ring.read_exact(&mut out));
        assert_eq!(out, [1, 2]);
        let mut out = [0u8; 4];
        assert!(ring.read_exact(&mut out));
        assert_eq!(out, [3, 4, 5, 6]);
        assert_eq!(ring.backlog(), 1);
        let mut out = [0u8; 1];
        assert!(ring.read_exact(&mut out));
        assert_eq!(out, [7]);
        assert_eq!(ring.backlog(), 0);
        // Consumed buffers are free again.
        assert!(ring.take_free().is_some());
        assert!(ring.take_free().is_some());
    }

    #[test]
    fn empty_completions_are_recycled_and_close_ends_waits() {
        let ring = Arc::new(Ring::new(2, 8));
        let a = ring.take_free().unwrap();
        ring.commit(a);
        assert_eq!(ring.backlog(), 0);
        let r = Arc::clone(&ring);
        let waiter = thread::spawn(move || {
            let mut out = [0u8; 4];
            r.read_exact(&mut out)
        });
        thread::sleep(Duration::from_millis(20));
        ring.close();
        assert!(!waiter.join().unwrap());
        assert!(ring.take_free().is_none());
    }

    #[test]
    fn the_read_thread_waits_for_a_free_buffer() {
        let ring = Arc::new(Ring::new(2, 4));
        let _held = ring.take_free().unwrap();
        let mut b = ring.take_free().unwrap();
        b.extend_fill(4, 1);
        ring.commit(b);
        let r = Arc::clone(&ring);
        let producer = thread::spawn(move || r.take_free().is_some());
        thread::sleep(Duration::from_millis(20));
        let mut out = [0u8; 4];
        assert!(ring.read_exact(&mut out));
        assert!(producer.join().unwrap());
    }
}
