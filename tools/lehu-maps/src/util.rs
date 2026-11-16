// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What every part shares: the error type, the cancel flag, the cache's
//! place, sizes for people.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Why a step stopped: the user cancelled it, or something went wrong
/// (said plainly, for the window).
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    Msg(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Cancelled."),
            Self::Msg(m) => f.write_str(m),
        }
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::Msg(s.to_string())
    }
}

/// An io error with what it was about.
pub fn io_err(what: impl fmt::Display, e: std::io::Error) -> Error {
    Error::Msg(format!("{what}: {e}"))
}

/// The Cancel button: set from the window, read by the worker.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(Cancelled)` once it is set.
    pub fn check(&self) -> Result<()> {
        if self.is_set() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Where downloads, the Python environment and built regions are kept:
/// `LEHU_MAPS_CACHE`, else the platform's cache folder (~/.cache/lehu-maps
/// on Linux, ~/Library/Caches/lehu-maps on macOS,
/// %LOCALAPPDATA%\lehu-maps on Windows).
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LEHU_MAPS_CACHE") {
        return PathBuf::from(dir);
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("lehu-maps")
}

/// Bytes for people: "48 MB", "1.6 GB".
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// Seconds for people: "about 3 minutes".
pub fn human_time(seconds: u64) -> String {
    if seconds < 90 {
        format!("about {seconds} s")
    } else if seconds < 90 * 60 {
        format!("about {} minutes", seconds.div_ceil(60))
    } else {
        format!("about {:.1} hours", seconds as f64 / 3600.0)
    }
}

/// Lower-case hex of a digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A plain file or folder name: no separators, not `.` or `..`, not
/// empty.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_names() {
        assert_eq!(human_size(47_945_977), "48 MB");
        assert_eq!(human_size(1_600_000_000), "1.6 GB");
        assert_eq!(human_time(200), "about 4 minutes");
        assert!(plain_name("hungary"));
        assert!(!plain_name("../etc"));
        assert!(!plain_name("a\\b"));
        assert!(!plain_name(".."));
    }
}
