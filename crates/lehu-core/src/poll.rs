// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The main loop's one wait: `poll()` over a few descriptors, with an
//! eventfd other threads use to wake it and a timerfd for the nearest
//! deadline. Nothing here allocates after construction, and nothing spins:
//! an idle loop sleeps in the kernel until a descriptor is ready.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// Which direction a descriptor is watched for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interest {
    Read,
    Write,
    ReadWrite,
}

impl Interest {
    fn events(self) -> libc::c_short {
        match self {
            Interest::Read => libc::POLLIN,
            Interest::Write => libc::POLLOUT,
            Interest::ReadWrite => libc::POLLIN | libc::POLLOUT,
        }
    }
}

/// A fixed set of descriptors polled together. Capacity is set at creation.
pub struct Poller {
    fds: Vec<libc::pollfd>,
}

impl Poller {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            fds: Vec::with_capacity(n),
        }
    }

    /// Watch `fd`; the returned index names it in `ready()`. False when the
    /// set is full.
    pub fn add(&mut self, fd: RawFd, interest: Interest) -> Option<usize> {
        if self.fds.len() == self.fds.capacity() {
            return None;
        }
        self.fds.push(libc::pollfd {
            fd,
            events: interest.events(),
            revents: 0,
        });
        Some(self.fds.len() - 1)
    }

    /// Stop watching the descriptor at `index` (it keeps its index; `ready`
    /// never reports it again). Used when a device goes away.
    pub fn remove(&mut self, index: usize) {
        if let Some(p) = self.fds.get_mut(index) {
            p.fd = -1;
            p.revents = 0;
        }
    }

    /// Watch a new descriptor at an index freed by `remove`.
    pub fn replace(&mut self, index: usize, fd: RawFd, interest: Interest) -> bool {
        match self.fds.get_mut(index) {
            Some(p) if p.fd < 0 => {
                *p = libc::pollfd {
                    fd,
                    events: interest.events(),
                    revents: 0,
                };
                true
            }
            _ => false,
        }
    }

    /// Wait until something is ready or `timeout` passes (`None`: forever).
    /// Returns how many descriptors are ready; an interrupted wait counts as
    /// none ready.
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
        let ms: libc::c_int = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(i32::MAX as u128) as libc::c_int,
        };
        for p in &mut self.fds {
            p.revents = 0;
        }
        // SAFETY: the pointer and the length describe this Vec's live storage.
        let n = unsafe { libc::poll(self.fds.as_mut_ptr(), self.fds.len() as libc::nfds_t, ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }

    /// Whether the descriptor at `index` was reported ready for reading (or
    /// hung up or in error, which a read will tell apart).
    pub fn readable(&self, index: usize) -> bool {
        self.fds.get(index).is_some_and(|p| {
            p.fd >= 0 && p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        })
    }

    pub fn writable(&self, index: usize) -> bool {
        self.fds
            .get(index)
            .is_some_and(|p| p.fd >= 0 && p.revents & libc::POLLOUT != 0)
    }

    /// The descriptor went away (hang-up or error).
    pub fn gone(&self, index: usize) -> bool {
        self.fds.get(index).is_some_and(|p| {
            p.fd >= 0 && p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
        })
    }

    pub fn len(&self) -> usize {
        self.fds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }
}

/// An eventfd: any thread calls `wake()`, the main loop polls it and
/// `drain()`s. Many wakes before a drain cost one wake-up.
pub struct Wake {
    fd: OwnedFd,
}

impl Wake {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the result is checked before use.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Callable from any thread, never blocks.
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a live u64 to our own descriptor.
        unsafe {
            libc::write(self.fd.as_raw_fd(), (&one as *const u64).cast(), 8);
        }
    }

    /// Clear the wake; how many wakes it held.
    pub fn drain(&self) -> u64 {
        let mut count: u64 = 0;
        // SAFETY: reading 8 bytes into a live u64 from our own descriptor.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), (&mut count as *mut u64).cast(), 8) };
        if n == 8 { count } else { 0 }
    }
}

impl AsRawFd for Wake {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// A timerfd carrying the loop's nearest deadline. Re-arm it whenever the
/// deadline list changes; read it when it fires.
pub struct Timer {
    fd: OwnedFd,
}

impl Timer {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the result is checked before use.
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Fire once after `after`; a zero duration fires at once. `None`
    /// disarms.
    pub fn arm(&self, after: Option<Duration>) -> io::Result<()> {
        let spec = match after {
            None => zero_spec(),
            Some(d) => {
                let mut s = zero_spec();
                // A zero timespec disarms, so "now" becomes one nanosecond.
                s.it_value.tv_sec = d.as_secs() as libc::time_t;
                s.it_value.tv_nsec =
                    (d.subsec_nanos().max(if d.as_secs() == 0 { 1 } else { 0 })) as libc::c_long;
                s
            }
        };
        // SAFETY: both pointers refer to live itimerspec values.
        let r =
            unsafe { libc::timerfd_settime(self.fd.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Acknowledge a firing; how many expirations it held (0 if none).
    pub fn read(&self) -> u64 {
        let mut count: u64 = 0;
        // SAFETY: reading 8 bytes into a live u64 from our own descriptor.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), (&mut count as *mut u64).cast(), 8) };
        if n == 8 { count } else { 0 }
    }
}

impl AsRawFd for Timer {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn zero_spec() -> libc::itimerspec {
    libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn wake_from_another_thread_ends_the_wait() {
        let wake = std::sync::Arc::new(Wake::new().unwrap());
        let mut poller = Poller::with_capacity(2);
        let i = poller.add(wake.as_raw_fd(), Interest::Read).unwrap();
        let w = wake.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            w.wake();
            w.wake();
        });
        let start = Instant::now();
        let n = poller.wait(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(n, 1);
        assert!(poller.readable(i));
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert_eq!(wake.drain(), 2);
        assert_eq!(wake.drain(), 0);
        t.join().unwrap();
    }

    #[test]
    fn timer_fires_once_and_can_be_disarmed() {
        let timer = Timer::new().unwrap();
        let mut poller = Poller::with_capacity(1);
        let i = poller.add(timer.as_raw_fd(), Interest::Read).unwrap();
        timer.arm(Some(Duration::from_millis(20))).unwrap();
        let start = Instant::now();
        assert_eq!(poller.wait(Some(Duration::from_secs(2))).unwrap(), 1);
        assert!(poller.readable(i));
        assert!(start.elapsed() >= Duration::from_millis(15));
        assert_eq!(timer.read(), 1);
        assert_eq!(timer.read(), 0);
        // Disarmed: a short wait times out.
        timer.arm(Some(Duration::from_millis(500))).unwrap();
        timer.arm(None).unwrap();
        assert_eq!(poller.wait(Some(Duration::from_millis(30))).unwrap(), 0);
        // A zero delay fires at once.
        timer.arm(Some(Duration::ZERO)).unwrap();
        assert_eq!(poller.wait(Some(Duration::from_millis(200))).unwrap(), 1);
    }

    #[test]
    fn timeout_without_events() {
        let wake = Wake::new().unwrap();
        let mut poller = Poller::with_capacity(1);
        poller.add(wake.as_raw_fd(), Interest::Read).unwrap();
        let start = Instant::now();
        assert_eq!(poller.wait(Some(Duration::from_millis(20))).unwrap(), 0);
        assert!(start.elapsed() >= Duration::from_millis(15));
    }

    #[test]
    fn capacity_remove_and_replace() {
        let a = Wake::new().unwrap();
        let b = Wake::new().unwrap();
        let mut poller = Poller::with_capacity(1);
        let i = poller.add(a.as_raw_fd(), Interest::Read).unwrap();
        assert_eq!(poller.add(b.as_raw_fd(), Interest::Read), None);
        poller.remove(i);
        a.wake();
        assert_eq!(
            poller.wait(Some(Duration::from_millis(10))).unwrap(),
            0,
            "a removed fd is not polled"
        );
        assert!(!poller.readable(i));
        assert!(poller.replace(i, b.as_raw_fd(), Interest::Read));
        assert!(
            !poller.replace(i, a.as_raw_fd(), Interest::Read),
            "only a freed slot is replaced"
        );
        b.wake();
        assert_eq!(poller.wait(Some(Duration::from_millis(100))).unwrap(), 1);
        assert!(poller.readable(i));
    }
}
