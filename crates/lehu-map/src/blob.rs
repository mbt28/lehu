// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A data file read in place: mapped from the card, never copied, so the
//! routing graph costs page cache the kernel can drop rather than memory
//! of our own. The files are little-endian 32-bit words (written by
//! scripts/map-graph.py), so they are looked at as words where they lie.

use std::path::Path;

#[cfg(target_endian = "big")]
compile_error!("the routing files are little endian, as the boards LEHU runs on");

/// The words of a file: mapped, or (for tests) owned.
pub(crate) enum Blob {
    Mapped { ptr: *const u32, len: usize },
    Owned(Vec<u32>),
}

// SAFETY: the mapping is read-only and lives as long as the Blob; nothing
// writes through it.
unsafe impl Send for Blob {}
unsafe impl Sync for Blob {}

impl Blob {
    /// The file mapped read-only. Its length must be a whole number of
    /// words.
    pub(crate) fn map(path: &Path) -> Result<Self, String> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len() as usize;
        if len == 0 || !len.is_multiple_of(4) {
            return Err(format!("{}: not a whole number of words", path.display()));
        }
        // SAFETY: a fresh read-only private mapping of a file we hold open;
        // the result is checked before it is used.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "{}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self::Mapped {
            ptr: ptr.cast(),
            len: len / 4,
        })
    }

    /// Bytes as the file has them (a test's own data).
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
            return Err("not a whole number of words".into());
        }
        Ok(Self::Owned(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .collect(),
        ))
    }

    pub(crate) fn words(&self) -> &[u32] {
        match self {
            // SAFETY: the mapping holds `len` words, is aligned to a page,
            // and lives until drop.
            Self::Mapped { ptr, len } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
            Self::Owned(v) => v,
        }
    }

    /// The bytes from word `at`, `len` of them: a section of text.
    pub(crate) fn bytes(&self, at: usize, len: usize) -> Option<&[u8]> {
        let words = self.words().get(at..)?;
        // SAFETY: a u32 slice seen as bytes: same memory, any bit pattern
        // is a valid u8.
        let bytes =
            unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 4) };
        bytes.get(..len)
    }
}

impl Drop for Blob {
    fn drop(&mut self) {
        if let Self::Mapped { ptr, len } = *self {
            // SAFETY: our own mapping, unmapped once.
            unsafe {
                libc::munmap(ptr.cast_mut().cast(), len * 4);
            }
        }
    }
}

/// A short string in a text section: a length byte, then UTF-8.
pub(crate) fn short_str(text: &[u8], at: u32) -> &str {
    let at = at as usize;
    let Some(&len) = text.get(at) else {
        return "";
    };
    text.get(at + 1..at + 1 + usize::from(len))
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
}

/// The same, as bytes.
pub(crate) fn short_bytes(text: &[u8], at: u32) -> &[u8] {
    let at = at as usize;
    let Some(&len) = text.get(at) else {
        return &[];
    };
    text.get(at + 1..at + 1 + usize::from(len)).unwrap_or(&[])
}
