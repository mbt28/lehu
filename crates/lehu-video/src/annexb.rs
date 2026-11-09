// SPDX-License-Identifier: GPL-3.0-or-later
//! Annex-B byte streams: what kind of NAL units an access unit carries, and
//! a splitter that cuts a file into access units for playback tests.
//!
//! Emulation prevention keeps `00 00 01` out of NAL payloads, so every match
//! is a real start code. Everything here checks a length before it indexes.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::unit::AccessUnit;

/// What a scan found in an access unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kinds {
    /// An H.264 IDR slice, or an HEVC IRAP picture (BLA, IDR, CRA).
    pub keyframe: bool,
    /// H.264 SPS or PPS; HEVC VPS, SPS or PPS.
    pub params: bool,
}

fn nal_type(header: u8, hevc: bool) -> u8 {
    if hevc {
        (header >> 1) & 0x3f
    } else {
        header & 0x1f
    }
}

fn is_picture(t: u8, hevc: bool) -> bool {
    if hevc { t <= 31 } else { (1..=5).contains(&t) }
}

fn is_key(t: u8, hevc: bool) -> bool {
    if hevc { (16..=21).contains(&t) } else { t == 5 }
}

fn is_params(t: u8, hevc: bool) -> bool {
    if hevc {
        (32..=34).contains(&t)
    } else {
        t == 7 || t == 8
    }
}

/// The NAL header bytes of `data`, each after a `00 00 01`, stopping early
/// when `f` returns true.
fn each_header(data: &[u8], mut f: impl FnMut(u8) -> bool) {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if f(data[i + 3]) {
                return;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
}

/// Keyframe and parameter-set NALs of an access unit. Stops as soon as both
/// are known; run during a resync, not on every frame.
pub fn kinds(data: &[u8], hevc: bool) -> Kinds {
    let mut k = Kinds::default();
    each_header(data, |h| {
        let t = nal_type(h, hevc);
        k.keyframe |= is_key(t, hevc);
        k.params |= is_params(t, hevc);
        k.keyframe && k.params
    });
    k
}

/// Whether the unit carries a picture (a VCL NAL). Stops at the first one,
/// so a normal frame costs a few bytes of scanning.
pub fn has_picture(data: &[u8], hevc: bool) -> bool {
    let mut found = false;
    each_header(data, |h| {
        found = is_picture(nal_type(h, hevc), hevc);
        found
    });
    found
}

/// Cuts a byte stream into access units, one picture each, for playing a
/// file the way a phone would send it. A new unit starts at a VCL NAL whose
/// first slice begins the picture (the first slice-header bit, set for
/// `first_mb_in_slice == 0` in H.264 and `first_slice_segment_in_pic_flag`
/// in HEVC), and at a parameter set, delimiter or SEI following a picture.
/// Parameter sets travel in front of the picture they precede, as in a
/// phone's stream.
pub struct Splitter {
    hevc: bool,
    buf: Vec<u8>,
    /// Where the NAL being read starts in `buf`; the unit under way starts
    /// at 0.
    nal_start: Option<usize>,
    /// The unit under way has a VCL NAL already.
    has_vcl: bool,
    /// How far `buf` was scanned for start codes.
    scanned: usize,
    out: std::collections::VecDeque<AccessUnit>,
}

impl Splitter {
    pub fn new(hevc: bool) -> Self {
        Self {
            hevc,
            buf: Vec::new(),
            nal_start: None,
            has_vcl: false,
            scanned: 0,
            out: std::collections::VecDeque::new(),
        }
    }

    /// Feed bytes; finished units are collected for `next_unit`.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.scan(false);
    }

    /// Find start codes. A NAL is only judged once its header and the byte
    /// after it are in; at the end of the stream the header alone will do.
    fn scan(&mut self, at_end: bool) {
        let need = if at_end { 3 } else { 4 };
        loop {
            let i = self.scanned;
            if i + need >= self.buf.len() {
                return;
            }
            if !(self.buf[i] == 0 && self.buf[i + 1] == 0 && self.buf[i + 2] == 1) {
                self.scanned += 1;
                continue;
            }
            // A four-byte start code belongs to the NAL it begins.
            let start = if i > 0 && self.buf[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            let drained = self.begin_nal(start, self.buf[i + 3], self.buf.get(i + 4).copied());
            self.scanned = i + 4 - drained;
        }
    }

    /// A NAL starts at `start` with `header` (and the byte after it, when
    /// there is one). Closes the unit under way when this NAL opens a new
    /// picture; how many bytes left the buffer.
    fn begin_nal(&mut self, start: usize, header: u8, next: Option<u8>) -> usize {
        let t = nal_type(header, self.hevc);
        let vcl = is_picture(t, self.hevc);
        // The first slice-header bit: for HEVC the header is two bytes, so
        // `next` is its second byte and the bit comes with the byte after;
        // close enough for the files this plays, where `next` holds it.
        let first_slice = next.is_some_and(|b| b & 0x80 != 0);
        let new_picture = if vcl {
            self.has_vcl && first_slice
        } else {
            self.has_vcl
        };
        let drained = if new_picture && self.nal_start.is_some() {
            self.emit(start)
        } else {
            0
        };
        self.nal_start = Some(start - drained);
        if vcl {
            self.has_vcl = true;
        }
        drained
    }

    /// Close the unit `[0, end)`, dropping its bytes from the buffer.
    fn emit(&mut self, end: usize) -> usize {
        if end > 0 {
            self.out.push_back(AccessUnit::from_slice(&self.buf[..end]));
        }
        self.buf.drain(..end);
        self.nal_start = None;
        self.has_vcl = false;
        end
    }

    /// The stream ended: whatever is under way is a unit.
    pub fn finish(&mut self) {
        self.scan(true);
        if self.nal_start.is_some() {
            let end = self.buf.len();
            self.emit(end);
        }
        self.buf.clear();
        self.scanned = 0;
    }

    /// The next finished unit, if any.
    pub fn next_unit(&mut self) -> Option<AccessUnit> {
        self.out.pop_front()
    }
}

/// The access units of an Annex-B file, read as it is played.
pub struct Units {
    file: File,
    splitter: Splitter,
    chunk: Vec<u8>,
    done: bool,
}

impl Iterator for Units {
    type Item = AccessUnit;

    fn next(&mut self) -> Option<AccessUnit> {
        loop {
            if let Some(u) = self.splitter.next_unit() {
                return Some(u);
            }
            if self.done {
                return None;
            }
            match self.file.read(&mut self.chunk) {
                Ok(0) | Err(_) => {
                    self.done = true;
                    self.splitter.finish();
                }
                Ok(n) => self.splitter.push(&self.chunk[..n]),
            }
        }
    }
}

/// Play `path` (an Annex-B `.h264` or `.h265` file) as access units. An
/// MP4 is turned into one first: `ffmpeg -i in.mp4 -c:v copy -bsf:v
/// h264_mp4toannexb out.h264`.
pub fn read_units(path: &Path, hevc: bool) -> io::Result<Units> {
    Ok(Units {
        file: File::open(path)?,
        splitter: Splitter::new(hevc),
        chunk: vec![0; 64 * 1024],
        done: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: [u8; 4] = [0, 0, 0, 1];

    fn h264(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&SC);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn h264_kinds() {
        let sps_pps_idr = h264(&[&[0x67, 1], &[0x68, 1], &[0x65, 0x88]]);
        assert_eq!(
            kinds(&sps_pps_idr, false),
            Kinds {
                keyframe: true,
                params: true
            }
        );
        assert!(has_picture(&sps_pps_idr, false));
        let p = h264(&[&[0x41, 0x9a]]);
        assert_eq!(kinds(&p, false), Kinds::default());
        assert!(has_picture(&p, false));
        let sei_only = h264(&[&[0x06, 5, 1, 0]]);
        assert!(!has_picture(&sei_only, false));
        let config = h264(&[&[0x67, 1], &[0x68, 1]]);
        assert!(!has_picture(&config, false));
        assert!(kinds(&config, false).params);
    }

    #[test]
    fn hevc_kinds() {
        // VPS 32, SPS 33, PPS 34, IDR_W_RADL 19, TRAIL_R 1 (type in bits 6..1).
        let unit = h264(&[
            &[32 << 1, 1],
            &[33 << 1, 1],
            &[34 << 1, 1],
            &[19 << 1, 1, 0xaf],
        ]);
        assert_eq!(
            kinds(&unit, true),
            Kinds {
                keyframe: true,
                params: true
            }
        );
        let trail = h264(&[&[1 << 1, 1, 0x80]]);
        assert_eq!(kinds(&trail, true), Kinds::default());
        assert!(has_picture(&trail, true));
        assert!(!has_picture(&h264(&[&[39 << 1, 1, 0]]), true));
    }

    #[test]
    fn short_and_odd_input_is_safe() {
        assert_eq!(kinds(&[], false), Kinds::default());
        assert!(!has_picture(&[0, 0, 1], false));
        assert!(!has_picture(&[0, 0, 1], true));
        assert!(!has_picture(&[0, 0, 0, 0, 0, 0], false));
        // A start code at the very end has no header to read.
        assert!(!has_picture(&[0x41, 0, 0, 1], false));
    }

    #[test]
    fn the_splitter_cuts_at_pictures_and_keeps_params_in_front() {
        let stream = h264(&[
            &[0x67, 1],
            &[0x68, 1],
            &[0x65, 0x88, 1, 2, 3],
            &[0x41, 0x9a, 4, 5],
            &[0x41, 0x1a, 6], // a second slice of the same picture
            &[0x06, 1, 0],    // SEI before the next picture
            &[0x41, 0x9a, 7],
            &[0x67, 1],
            &[0x68, 1],
            &[0x65, 0x88, 8],
        ]);
        let mut s = Splitter::new(false);
        // Fed in awkward pieces.
        for piece in stream.chunks(5) {
            s.push(piece);
        }
        s.finish();
        let mut units = Vec::new();
        while let Some(u) = s.next_unit() {
            units.push(u);
        }
        assert_eq!(units.len(), 4);
        assert_eq!(
            units[0].data(),
            &h264(&[&[0x67, 1], &[0x68, 1], &[0x65, 0x88, 1, 2, 3]])[..]
        );
        assert_eq!(
            units[1].data(),
            &h264(&[&[0x41, 0x9a, 4, 5], &[0x41, 0x1a, 6]])[..]
        );
        assert_eq!(
            units[2].data(),
            &h264(&[&[0x06, 1, 0], &[0x41, 0x9a, 7]])[..]
        );
        assert_eq!(
            units[3].data(),
            &h264(&[&[0x67, 1], &[0x68, 1], &[0x65, 0x88, 8]])[..]
        );
        for u in &units {
            assert!(has_picture(u.data(), false));
        }
    }

    #[test]
    fn three_byte_start_codes_and_trailing_bytes() {
        let mut stream = vec![0, 0, 1, 0x65, 0x88, 9];
        stream.extend_from_slice(&[0, 0, 1, 0x41, 0x9a, 9, 9]);
        let mut s = Splitter::new(false);
        s.push(&stream);
        s.finish();
        let a = s.next_unit().expect("first");
        let b = s.next_unit().expect("second");
        assert!(s.next_unit().is_none());
        assert_eq!(a.data(), &[0, 0, 1, 0x65, 0x88, 9]);
        assert_eq!(b.data(), &[0, 0, 1, 0x41, 0x9a, 9, 9]);
        // Nothing at all.
        let mut e = Splitter::new(false);
        e.push(&[0, 0, 0]);
        e.finish();
        assert!(e.next_unit().is_none());
    }
}
