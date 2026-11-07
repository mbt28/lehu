//! Framebuffers for the pictures, keyed by the first plane's dma-buf fd.
//!
//! The decoder's pool cycles a small, stable set of buffers, so each
//! framebuffer is built once instead of an AddFB2 and an RmFB per frame
//! (costly on the ARM926). Size and format are part of the entry: an fd
//! number the kernel hands out again after a pool rebuild can never put a
//! stale framebuffer on the plane. The GEM handles are kept so an entry can
//! be released completely. RmFB alone leaves them holding references on the
//! decoder's buffers, and the pool's CMA leaks at every rebuild; the 24 MB
//! of this board fit one pool.
//!
//! The cache is bounded. When it is full, the entry shown longest ago is
//! released. In normal use it never fills (a pool is a dozen buffers, the
//! camera four); the bound only keeps a leak from growing.

use crate::frame::MAX_PLANES;

/// Releases what an entry holds: the device, or a test double.
pub trait Gem {
    fn rm_fb(&self, fb: u32);
    fn close_handle(&self, handle: u32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The first plane's dma-buf fd.
    pub key: i32,
    pub fb: u32,
    pub handles: [u32; MAX_PLANES],
    pub handle_count: usize,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
}

pub struct FbCache {
    /// Each entry with the tick of its last use.
    entries: Vec<(Entry, u64)>,
    capacity: usize,
    tick: u64,
}

impl FbCache {
    /// Room for `capacity` entries, allocated now and never grown.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
            tick: 0,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The framebuffer cached under `key` when it is this very buffer. An
    /// entry under the same key with another size or format is the old
    /// buffer whose fd number came back: it is released, and None says to
    /// import again.
    pub fn lookup(
        &mut self,
        key: i32,
        width: u32,
        height: u32,
        fourcc: u32,
        gem: &impl Gem,
    ) -> Option<u32> {
        let i = self.entries.iter().position(|(e, _)| e.key == key)?;
        let entry = self.entries[i].0;
        if (entry.width, entry.height, entry.fourcc) == (width, height, fourcc) {
            self.tick += 1;
            self.entries[i].1 = self.tick;
            return Some(entry.fb);
        }
        release(&entry, gem);
        self.entries.swap_remove(i);
        None
    }

    /// Keep a framebuffer just made. An entry already under its key goes
    /// first; when the cache is full, the entry used longest ago goes.
    pub fn insert(&mut self, entry: Entry, gem: &impl Gem) {
        if let Some(i) = self.entries.iter().position(|(e, _)| e.key == entry.key) {
            release(&self.entries[i].0, gem);
            self.entries.swap_remove(i);
        }
        if self.entries.len() >= self.capacity {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, tick))| *tick)
                .map(|(i, _)| i);
            if let Some(i) = oldest {
                release(&self.entries[i].0, gem);
                self.entries.swap_remove(i);
            }
        }
        self.tick += 1;
        self.entries.push((entry, self.tick));
    }

    /// Release every entry: before the owner frees its buffers.
    pub fn flush(&mut self, gem: &impl Gem) {
        for (entry, _) in self.entries.drain(..) {
            release(&entry, gem);
        }
    }
}

/// RmFB, then the GEM handles.
fn release(entry: &Entry, gem: &impl Gem) {
    gem.rm_fb(entry.fb);
    close_handles(&entry.handles[..entry.handle_count.min(MAX_PLANES)], gem);
}

/// Close each handle once: the planes of an NV12 picture in one dma-buf
/// import to the same handle, and a zero is an unused slot.
pub fn close_handles(handles: &[u32], gem: &impl Gem) {
    for (i, &h) in handles.iter().enumerate() {
        if h != 0 && !handles[..i].contains(&h) {
            gem.close_handle(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Log(RefCell<Vec<(&'static str, u32)>>);

    impl Gem for Log {
        fn rm_fb(&self, fb: u32) {
            self.0.borrow_mut().push(("rmfb", fb));
        }
        fn close_handle(&self, handle: u32) {
            self.0.borrow_mut().push(("close", handle));
        }
    }

    impl Log {
        fn take(&self) -> Vec<(&'static str, u32)> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
    }

    const NV12: u32 = 0x3231_564e;

    fn entry(key: i32, fb: u32, handles: &[u32]) -> Entry {
        let mut hs = [0u32; MAX_PLANES];
        hs[..handles.len()].copy_from_slice(handles);
        Entry {
            key,
            fb,
            handles: hs,
            handle_count: handles.len(),
            width: 800,
            height: 480,
            fourcc: NV12,
        }
    }

    #[test]
    fn a_hit_returns_the_framebuffer_without_touching_the_device() {
        let gem = Log::default();
        let mut cache = FbCache::with_capacity(4);
        assert!(cache.is_empty());
        assert_eq!(cache.lookup(7, 800, 480, NV12, &gem), None);
        cache.insert(entry(7, 100, &[1, 1]), &gem);
        assert_eq!(cache.lookup(7, 800, 480, NV12, &gem), Some(100));
        assert_eq!(cache.len(), 1);
        assert!(gem.take().is_empty());
    }

    #[test]
    fn a_recycled_fd_number_retires_the_stale_entry() {
        let gem = Log::default();
        let mut cache = FbCache::with_capacity(4);
        cache.insert(entry(7, 100, &[1, 1]), &gem);
        // Same fd, another size: the old buffer is gone.
        assert_eq!(cache.lookup(7, 640, 480, NV12, &gem), None);
        assert_eq!(gem.take(), vec![("rmfb", 100), ("close", 1)]);
        assert!(cache.is_empty());
        cache.insert(entry(7, 101, &[2, 2]), &gem);
        // Same size, another format: also gone.
        assert_eq!(cache.lookup(7, 800, 480, 0x3131_3131, &gem), None);
        assert_eq!(gem.take(), vec![("rmfb", 101), ("close", 2)]);
    }

    #[test]
    fn handles_are_closed_once_and_zeros_skipped() {
        let gem = Log::default();
        let mut cache = FbCache::with_capacity(4);
        cache.insert(entry(7, 100, &[5, 0, 5, 6]), &gem);
        cache.flush(&gem);
        assert_eq!(gem.take(), vec![("rmfb", 100), ("close", 5), ("close", 6)]);
        assert!(cache.is_empty());
        // A count beyond the array is never indexed.
        let mut e = entry(8, 200, &[9]);
        e.handle_count = 40;
        cache.insert(e, &gem);
        cache.flush(&gem);
        assert_eq!(gem.take(), vec![("rmfb", 200), ("close", 9)]);
    }

    #[test]
    fn insert_over_an_existing_key_releases_the_old_entry() {
        let gem = Log::default();
        let mut cache = FbCache::with_capacity(4);
        cache.insert(entry(7, 100, &[1]), &gem);
        cache.insert(entry(7, 101, &[2]), &gem);
        assert_eq!(gem.take(), vec![("rmfb", 100), ("close", 1)]);
        assert_eq!(cache.lookup(7, 800, 480, NV12, &gem), Some(101));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_full_cache_drops_the_entry_used_longest_ago() {
        let gem = Log::default();
        let mut cache = FbCache::with_capacity(2);
        cache.insert(entry(1, 100, &[1]), &gem);
        cache.insert(entry(2, 200, &[2]), &gem);
        // Entry 1 is used again, so entry 2 is the oldest.
        assert_eq!(cache.lookup(1, 800, 480, NV12, &gem), Some(100));
        cache.insert(entry(3, 300, &[3]), &gem);
        assert_eq!(gem.take(), vec![("rmfb", 200), ("close", 2)]);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.lookup(1, 800, 480, NV12, &gem), Some(100));
        assert_eq!(cache.lookup(3, 800, 480, NV12, &gem), Some(300));
        assert_eq!(cache.lookup(2, 800, 480, NV12, &gem), None);
    }

    #[test]
    fn capacity_is_fixed_at_creation() {
        let gem = Log::default();
        let mut cache = FbCache::with_capacity(3);
        let room = cache.entries.capacity();
        for i in 0..20 {
            cache.insert(entry(i, 100 + i as u32, &[1]), &gem);
        }
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.entries.capacity(), room);
        // A zero capacity still holds one entry.
        assert_eq!(FbCache::with_capacity(0).capacity, 1);
    }
}
