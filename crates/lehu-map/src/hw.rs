// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The video engine's JPEG decoder (csrc/jpeg.c), when the kernel has one:
//! a tile in about a millisecond, against a quarter of a second in
//! software on this CPU.
//!
//! The engine writes NV12 in 32 x 32 tiles to uncached memory. The picture
//! is copied out whole (one burst copy), then untiled into a linear NV12
//! tile, its full-range JPEG levels mapped to the limited range the
//! display's colour conversion expects.

use std::ffi::{CStr, c_char, c_int};
use std::io;
use std::ptr::NonNull;

use crate::nv12::{FULL_TO_LIMITED_C, FULL_TO_LIMITED_Y};

#[repr(C)]
struct Raw {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn lehu_jpeg_open(
        width: u32,
        height: u32,
        max_input: u32,
        path: *mut c_char,
        path_len: usize,
        err: *mut c_int,
    ) -> *mut Raw;
    fn lehu_jpeg_close(j: *mut Raw);
    fn lehu_jpeg_luma_size(j: *const Raw) -> u32;
    fn lehu_jpeg_decode(
        j: *mut Raw,
        data: *const u8,
        len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> c_int;
}

/// Why a tile did not come out of the engine.
#[derive(Debug)]
pub enum HwError {
    /// The engine would not take this file (not baseline 4:2:0): decode
    /// it in software.
    Refused,
    Io(io::Error),
}

/// A session with the engine for square tiles of one size.
pub struct HwJpeg {
    raw: NonNull<Raw>,
    side: usize,
    luma: usize,
    /// The picture, copied out of the uncached buffer.
    staging: Vec<u8>,
    /// The levels left as the engine made them (full range: the display
    /// is told so), not mapped to the limited range while untiling.
    full_range: bool,
}

// SAFETY: the session is a descriptor and two mappings, used by one owner
// at a time (`&mut self`); nothing in it is tied to the opening thread.
unsafe impl Send for HwJpeg {}

impl HwJpeg {
    /// The first device that decodes JPEG, set up for `side` x `side`
    /// tiles of up to `max_input` bytes, with its path. `NotFound` when the
    /// kernel has none.
    pub fn open(side: u32, max_input: u32) -> io::Result<(Self, String)> {
        // The chroma, half as tall, in whole 32-row blocks too.
        if side == 0 || !side.is_multiple_of(64) {
            return Err(io::Error::other("tiles must be a multiple of 64 pixels"));
        }
        let mut path = [0 as c_char; 64];
        let mut err: c_int = 0;
        // SAFETY: `path` and `err` outlive the call, which writes at most
        // `path.len()` bytes into `path`, NUL-terminated.
        let raw = unsafe {
            lehu_jpeg_open(
                side,
                side,
                max_input,
                path.as_mut_ptr(),
                path.len(),
                &mut err,
            )
        };
        let Some(raw) = NonNull::new(raw) else {
            return Err(if err == -libc::ENODEV {
                io::Error::from(io::ErrorKind::NotFound)
            } else {
                io::Error::from_raw_os_error(-err)
            });
        };
        // SAFETY: NUL-terminated by the shim (an empty string at worst).
        let path = unsafe { CStr::from_ptr(path.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: a session just opened.
        let luma = unsafe { lehu_jpeg_luma_size(raw.as_ptr()) } as usize;
        let side = side as usize;
        if luma != side * side {
            // SAFETY: opened above, closed once.
            unsafe { lehu_jpeg_close(raw.as_ptr()) };
            return Err(io::Error::other("unexpected picture layout"));
        }
        let hw = Self {
            raw,
            side,
            luma,
            staging: vec![0; luma + luma / 2],
            full_range: false,
        };
        Ok((hw, path))
    }

    /// Leave the tiles full range (the display converts them so), or map
    /// them to the limited range while untiling (the default).
    pub fn set_full_range(&mut self, full: bool) {
        self.full_range = full;
    }

    /// Decode `data` into `dst`: a linear NV12 tile, limited range,
    /// `side * side * 3 / 2` bytes.
    pub fn decode(&mut self, data: &[u8], dst: &mut [u8]) -> Result<(), HwError> {
        let mut out: *const u8 = std::ptr::null();
        let mut out_len = 0usize;
        // SAFETY: the session is ours (`&mut self`); `data` is read for the
        // call only; `out` and `out_len` outlive it.
        let r = unsafe {
            lehu_jpeg_decode(
                self.raw.as_ptr(),
                data.as_ptr(),
                data.len(),
                &mut out,
                &mut out_len,
            )
        };
        if r == -libc::EBADMSG {
            return Err(HwError::Refused);
        }
        if r < 0 {
            return Err(HwError::Io(io::Error::from_raw_os_error(-r)));
        }
        if out.is_null() || out_len < self.staging.len() {
            return Err(HwError::Io(io::Error::other("short picture")));
        }
        // SAFETY: the shim's capture mapping, `out_len` bytes, not touched
        // by the engine until the next decode, which needs `&mut self`.
        let picture = unsafe { std::slice::from_raw_parts(out, self.staging.len()) };
        // One copy out of the uncached buffer, in bursts; every read after
        // it is from cached memory.
        self.staging.copy_from_slice(picture);
        if self.full_range {
            untile_with(
                &self.staging,
                self.side,
                self.luma,
                dst,
                &IDENTITY,
                &IDENTITY,
            );
        } else {
            untile(&self.staging, self.side, self.luma, dst);
        }
        Ok(())
    }
}

impl Drop for HwJpeg {
    fn drop(&mut self) {
        // SAFETY: opened in `open`, closed once, here.
        unsafe { lehu_jpeg_close(self.raw.as_ptr()) };
    }
}

/// ST12 (NV12 in 32 x 32 tiles, the tiles row by row) of a `side` x
/// `side` picture whose luma is `luma` bytes, to linear NV12, limited
/// range.
pub fn untile(src: &[u8], side: usize, luma: usize, dst: &mut [u8]) {
    untile_with(src, side, luma, dst, &FULL_TO_LIMITED_Y, &FULL_TO_LIMITED_C);
}

/// Each level as it is.
const IDENTITY: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i as u8;
        i += 1;
    }
    t
};

/// `untile`, the luma and chroma levels mapped through `y_map` and
/// `c_map`.
fn untile_with(
    src: &[u8],
    side: usize,
    luma: usize,
    dst: &mut [u8],
    y_map: &[u8; 256],
    c_map: &[u8; 256],
) {
    let (dst_y, dst_c) = dst.split_at_mut(side * side);
    plane(&src[..luma], side, side, dst_y, y_map);
    plane(&src[luma..], side, side / 2, dst_c, c_map);
}

/// One plane of `width` bytes x `rows`, its tiles 32 bytes x 32 rows.
fn plane(src: &[u8], width: usize, rows: usize, dst: &mut [u8], map: &[u8; 256]) {
    let across = width / 32;
    for y in 0..rows {
        let (ty, ry) = (y / 32, y % 32);
        let line = &mut dst[y * width..(y + 1) * width];
        for (tx, out) in line.as_chunks_mut::<32>().0.iter_mut().enumerate() {
            let at = (ty * across + tx) * 1024 + ry * 32;
            for (o, &v) in out.iter_mut().zip(&src[at..at + 32]) {
                *o = map[usize::from(v)];
            }
        }
    }
}
