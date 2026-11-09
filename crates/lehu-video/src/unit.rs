// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! One access unit as the protocol hands it over: an Annex-B buffer, filled
//! in place, carried through the queue by value and reused.

/// Readable bytes kept after the data: libavcodec's parser reads up to
/// `AV_INPUT_BUFFER_PADDING_SIZE` (64) past the end of what it is given.
pub const PADDING: usize = 64;

/// An Annex-B access unit. The buffer is allocated once, at the size the
/// protocol chooses, and never grows: a unit that does not fit is refused
/// (`write` returns false) rather than reallocated on the wire thread. The
/// flags are hints from a protocol that knows (CarPlay's VideoConfig, an
/// Android Auto config message); the decoder checks the bytes anyway.
pub struct AccessUnit {
    /// `cap + PADDING` bytes, all allocated; `len` of them are data.
    buf: Vec<u8>,
    len: usize,
    cap: usize,
    /// The furthest byte written since the pages were last given back.
    high: usize,
    /// The protocol knows this unit holds a keyframe.
    pub keyframe: bool,
    /// The protocol knows this unit is parameter sets only.
    pub config: bool,
}

impl AccessUnit {
    /// A unit that takes up to `bytes` bytes.
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: vec![0; bytes + PADDING],
            len: 0,
            cap: bytes,
            high: 0,
            keyframe: false,
            config: false,
        }
    }

    /// A unit holding a copy of `data`, as big as it needs to be. For tests
    /// and file playback; a protocol reuses `with_capacity` units.
    pub fn from_slice(data: &[u8]) -> Self {
        let mut u = Self::with_capacity(data.len());
        u.write(data);
        u
    }

    /// Bytes this unit can hold.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Empty, flags cleared, ready to be filled again.
    pub fn clear(&mut self) {
        self.len = 0;
        self.keyframe = false;
        self.config = false;
    }

    /// Append `data`. False, and nothing written, when it does not fit.
    pub fn write(&mut self, data: &[u8]) -> bool {
        let Some(end) = self.len.checked_add(data.len()) else {
            return false;
        };
        if end > self.cap {
            return false;
        }
        self.buf[self.len..end].copy_from_slice(data);
        self.len = end;
        self.high = self.high.max(end);
        true
    }

    /// Room for `n` more bytes, to be filled by the caller (a decrypt or a
    /// reassembly that writes straight into the unit). None when they do not
    /// fit. The bytes count as data as soon as this returns.
    pub fn space(&mut self, n: usize) -> Option<&mut [u8]> {
        let end = self.len.checked_add(n)?;
        if end > self.cap {
            return None;
        }
        let start = self.len;
        self.len = end;
        self.high = self.high.max(end);
        Some(&mut self.buf[start..end])
    }

    pub fn data(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// The data with the padding behind it zeroed, for the parser.
    pub(crate) fn padded(&mut self) -> &[u8] {
        let end = self.len + PADDING;
        self.buf[self.len..end].fill(0);
        self.high = self.high.max(end);
        &self.buf[..self.len]
    }
}

impl AccessUnit {
    /// Give the pages written past the first `keep` bytes back to the
    /// system. A unit that carried a keyframe would otherwise keep its
    /// pages while it waits to be reused; written again, they come back
    /// zeroed, as the buffer started. Only whole pages inside the buffer
    /// go, so it is safe wherever the allocator put it.
    pub fn release_pages(&mut self, keep: usize) {
        if self.high <= keep {
            return;
        }
        // SAFETY: sysconf has no preconditions.
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
        let base = self.buf.as_mut_ptr() as usize;
        let start = (base + keep).next_multiple_of(page);
        let end = (base + self.high.min(self.buf.len())) & !(page - 1);
        if end > start {
            // SAFETY: [start, end) is whole pages inside this unit's own
            // buffer, which nothing else refers to while we hold `&mut
            // self`; dropped pages read back as zero, which the buffer may
            // hold anywhere past `len`.
            unsafe {
                libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED);
            }
        }
        self.high = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_pages_read_back_as_zero_and_the_unit_works_on() {
        let mut u = AccessUnit::with_capacity(256 * 1024);
        assert!(u.write(&[0xaa; 200 * 1024]));
        u.clear();
        u.release_pages(64 * 1024);
        // The bytes kept are as written; the whole pages past them, up to
        // what was written, are zero (a page at either end may stay).
        assert!(u.buf[..64 * 1024].iter().all(|&b| b == 0xaa));
        assert!(u.buf[72 * 1024..190 * 1024].iter().all(|&b| b == 0));
        assert!(u.write(&[0x55; 150 * 1024]));
        assert!(u.data().iter().all(|&b| b == 0x55));
        // Nothing past what was kept: nothing to give back.
        let mut small = AccessUnit::with_capacity(8);
        small.write(&[1]);
        small.release_pages(64 * 1024);
        assert_eq!(small.data(), &[1]);
    }

    #[test]
    fn writes_are_bounded_by_the_capacity() {
        let mut u = AccessUnit::with_capacity(4);
        assert!(u.is_empty());
        assert!(u.write(&[1, 2, 3]));
        assert!(!u.write(&[4, 5]));
        assert_eq!(u.data(), &[1, 2, 3]);
        assert!(u.write(&[4]));
        assert_eq!(u.len(), 4);
        assert!(u.space(1).is_none());
        assert_eq!(u.capacity(), 4);
    }

    #[test]
    fn space_hands_out_the_tail_and_counts_it() {
        let mut u = AccessUnit::with_capacity(8);
        u.write(&[9]);
        let s = u.space(3).expect("fits");
        s.copy_from_slice(&[1, 2, 3]);
        assert_eq!(u.data(), &[9, 1, 2, 3]);
        assert!(u.space(5).is_none());
        assert_eq!(u.len(), 4);
    }

    #[test]
    fn padding_is_there_and_zeroed() {
        let mut u = AccessUnit::from_slice(&[0xff; 10]);
        u.buf[10..].fill(0xaa);
        let data = u.padded();
        assert_eq!(data.len(), 10);
        assert!(u.buf[10..10 + PADDING].iter().all(|&b| b == 0));
        assert_eq!(u.buf.len(), 10 + PADDING);
    }

    #[test]
    fn clear_resets_the_flags_and_keeps_the_buffer() {
        let mut u = AccessUnit::from_slice(&[1, 2]);
        u.keyframe = true;
        u.config = true;
        let cap = u.buf.capacity();
        u.clear();
        assert!(u.is_empty() && !u.keyframe && !u.config);
        assert_eq!(u.buf.capacity(), cap);
        assert_eq!(u.capacity(), 2);
    }
}
