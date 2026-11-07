//! The album art's pictures: JPEG (baseline and progressive) and PNG,
//! decoded straight to the size they show at.
//!
//! The bytes come from a phone, so they are read as hostile: every length
//! is checked against what is left, every size against a bound before
//! anything is allocated for it, and a picture that breaks a rule is
//! refused rather than guessed at. Integers only (the board has no FPU),
//! and the full-size picture is never held: rows go through a box filter
//! as they are decoded, and only the picture at its shown size stays.

mod jpeg;
mod png;
mod scale;

use std::fmt;

/// The most compressed bytes taken: art is tens of kilobytes.
pub const MAX_INPUT: usize = 512 * 1024;

/// A side longer than this is refused.
const MAX_SIDE: u32 = 4096;

/// Pixels a picture may have: bounds the work a header can ask for (a
/// PNG compresses a thousand to one).
const MAX_PIXELS: u64 = 4 * 1024 * 1024;

/// A decoded picture: RGB, three bytes a pixel, rows top to bottom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Past `MAX_INPUT`, or a size past the bounds.
    TooBig,
    /// Neither a JPEG nor a PNG.
    Format,
    /// A kind of JPEG or PNG this decoder does not read (arithmetic
    /// coding, 12-bit samples, CMYK, interlacing).
    Unsupported,
    /// The data ends before the picture does.
    Truncated,
    /// A value the format does not allow.
    Corrupt,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::TooBig => "too big",
            Error::Format => "not a JPEG or PNG",
            Error::Unsupported => "a kind not supported",
            Error::Truncated => "truncated",
            Error::Corrupt => "corrupt",
        })
    }
}

impl std::error::Error for Error {}

/// Decode `data` to fit a `side` x `side` box, its longest side `side`
/// pixels; `bg` is what a transparent PNG is laid on.
pub fn decode(data: &[u8], side: u32, bg: [u8; 3]) -> Result<Picture, Error> {
    if data.len() > MAX_INPUT || side == 0 || side > MAX_SIDE {
        return Err(Error::TooBig);
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        jpeg::decode(data, side)
    } else if data.starts_with(&png::SIGNATURE) {
        png::decode(data, side, bg)
    } else {
        Err(Error::Format)
    }
}

/// Refuse a picture too large to decode.
fn check_size(width: u32, height: u32) -> Result<(), Error> {
    if width == 0 || height == 0 {
        return Err(Error::Corrupt);
    }
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::TooBig);
    }
    Ok(())
}

/// The size a `width` x `height` picture shows at in a `side` box: the
/// longest side fills it, the other keeps the proportion.
pub fn fit(width: u32, height: u32, side: u32) -> (u32, u32) {
    let (long, short) = (width.max(height), width.min(height));
    if long == 0 {
        return (0, 0);
    }
    let other = ((u64::from(short) * u64::from(side) + u64::from(long) / 2) / u64::from(long))
        .max(1) as u32;
    if width >= height {
        (side, other)
    } else {
        (other, side)
    }
}

#[cfg(test)]
mod tests;
