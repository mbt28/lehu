// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Hotplug without udev: inotify on /dev/input, and the names of its nodes
//! kept without allocating.

use std::ffi::CStr;
use std::fmt;

/// One inotify event, as parsed: the watch, the mask and the name without
/// its padding.
pub struct Notice<'a> {
    pub wd: i32,
    pub mask: u32,
    pub name: &'a [u8],
}

const HEADER: usize = 16; // wd, mask, cookie, len: four u32-sized fields

/// Every whole event in a buffer read from the inotify descriptor. A torn
/// tail is ignored; the kernel never writes one.
pub fn parse(buf: &[u8], mut f: impl FnMut(Notice<'_>)) {
    let mut off = 0;
    while let Some(head) = buf.get(off..off + HEADER) {
        let word = |i: usize| -> [u8; 4] { head[i..i + 4].try_into().unwrap_or([0; 4]) };
        let wd = i32::from_ne_bytes(word(0));
        let mask = u32::from_ne_bytes(word(4));
        let len = u32::from_ne_bytes(word(12)) as usize;
        let start = off + HEADER;
        let Some(padded) = start.checked_add(len).and_then(|end| buf.get(start..end)) else {
            return;
        };
        let end = padded.iter().position(|&b| b == 0).unwrap_or(len);
        f(Notice {
            wd,
            mask,
            name: &padded[..end],
        });
        off = start + len;
    }
}

const NAME_MAX: usize = 31;
const DIR: &[u8] = b"/dev/input/";
/// Room for the directory, the longest name and the NUL.
pub const PATH_BUF: usize = DIR.len() + NAME_MAX + 1;

/// A node's file name under /dev/input, without the directory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NodeName {
    bytes: [u8; NAME_MAX],
    len: u8,
}

impl NodeName {
    /// None for an empty name, one too long or one with a NUL in it.
    pub fn new(name: &[u8]) -> Option<Self> {
        if name.is_empty() || name.len() > NAME_MAX || name.contains(&0) {
            return None;
        }
        let mut bytes = [0; NAME_MAX];
        bytes[..name.len()].copy_from_slice(name);
        Some(Self {
            bytes,
            len: name.len() as u8,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// An `eventN` node, N up to five digits.
    pub fn is_event(&self) -> bool {
        let Some(digits) = self.as_bytes().strip_prefix(b"event") else {
            return false;
        };
        !digits.is_empty() && digits.len() <= 5 && digits.iter().all(u8::is_ascii_digit)
    }

    /// `/dev/input/<name>`, NUL-terminated, in the caller's buffer.
    pub fn path<'a>(&self, buf: &'a mut [u8; PATH_BUF]) -> Option<&'a CStr> {
        let name = self.as_bytes();
        let n = DIR.len() + name.len();
        buf[..DIR.len()].copy_from_slice(DIR);
        buf[DIR.len()..n].copy_from_slice(name);
        buf[n] = 0;
        CStr::from_bytes_with_nul(&buf[..=n]).ok()
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::str::from_utf8(self.as_bytes()).unwrap_or("?"))
    }
}

impl fmt::Debug for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(wd: i32, mask: u32, name: &[u8], pad_to: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&wd.to_ne_bytes());
        v.extend_from_slice(&mask.to_ne_bytes());
        v.extend_from_slice(&0u32.to_ne_bytes());
        v.extend_from_slice(&(pad_to as u32).to_ne_bytes());
        v.extend_from_slice(name);
        v.resize(v.len() + pad_to - name.len(), 0);
        v
    }

    #[test]
    fn parses_whole_events_and_ignores_a_torn_tail() {
        let mut buf = event(3, libc::IN_CREATE, b"event5", 16);
        buf.extend(event(2, libc::IN_IGNORED, b"", 0));
        buf.extend(event(3, libc::IN_DELETE, b"event12", 16));
        buf.truncate(buf.len() - 4); // the kernel never does this, but a parser must cope
        let mut seen = Vec::new();
        parse(&buf, |n| seen.push((n.wd, n.mask, n.name.to_vec())));
        assert_eq!(
            seen,
            vec![
                (3, libc::IN_CREATE, b"event5".to_vec()),
                (2, libc::IN_IGNORED, Vec::new()),
            ]
        );
    }

    #[test]
    fn node_names() {
        let n = NodeName::new(b"event12").expect("fits");
        assert!(n.is_event());
        assert_eq!(n.to_string(), "event12");
        let mut buf = [0u8; PATH_BUF];
        assert_eq!(
            n.path(&mut buf).map(CStr::to_bytes),
            Some(&b"/dev/input/event12"[..])
        );
        assert!(!NodeName::new(b"mice").expect("fits").is_event());
        assert!(!NodeName::new(b"event").expect("fits").is_event());
        assert!(!NodeName::new(b"event1a").expect("fits").is_event());
        assert!(!NodeName::new(b"event123456").expect("fits").is_event());
        assert!(NodeName::new(b"").is_none());
        assert!(NodeName::new(&[b'x'; 32]).is_none());
        assert!(NodeName::new(b"ev\0ent").is_none());
        let longest = NodeName::new(&[b'y'; NAME_MAX]).expect("fits");
        assert!(longest.path(&mut buf).is_some());
    }
}
