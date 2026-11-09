// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A decoded picture as the decoder or the camera hands it over: dma-buf
//! planes that stay open and unchanged while the picture is on the plane.

use std::fmt;

use drm::buffer::DrmFourcc;

/// DRM's limit on the planes of one framebuffer.
pub const MAX_PLANES: usize = 4;

/// `DRM_FORMAT_MOD_INVALID`: "no modifier known". No framebuffer can be made
/// with it, and the drm crate asserts on it, so it is refused up front.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Larger than any panel or decoder output this runs on. Keeps the 16.16
/// source rectangle and the pitch arithmetic far from overflow.
const MAX_SIDE: u32 = 16384;

/// One plane of a picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    /// The dma-buf. Planes of one picture may share it (NV12 in one buffer).
    pub fd: i32,
    pub pitch: u32,
    pub offset: u32,
}

/// A picture to put on the video plane. The whole buffer is shown, scaled to
/// the panel by the display engine.
///
/// The dma-bufs belong to their owner (the decoder's pool, the camera): they
/// are never closed here, and the owner keeps them open and unchanged while
/// the picture is shown. After `Show::Shown` the buffer stays on scanout
/// until a later picture replaces it, and on the sun4i CRTC a flip is
/// signalled one frame early, so the owner keeps the last three shown
/// pictures referenced (see `Display::show_video`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The DRM fourcc, as the decoder's DRM frame descriptor gives it.
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub planes: [Plane; MAX_PLANES],
    /// How many of `planes` are in use.
    pub plane_count: usize,
    /// The format modifier, the same for every plane: linear is 0, cedrus
    /// pictures carry `ALLWINNER_TILED`.
    pub modifier: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    NoPlanes,
    TooManyPlanes(usize),
    /// Plane `n` has a negative descriptor.
    BadFd(usize),
    ZeroSize,
    TooLarge,
    /// A fourcc DRM does not know.
    Fourcc(u32),
    InvalidModifier,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::NoPlanes => f.write_str("no planes"),
            FrameError::TooManyPlanes(n) => write!(f, "{n} planes, at most {MAX_PLANES}"),
            FrameError::BadFd(n) => write!(f, "plane {n} has no descriptor"),
            FrameError::ZeroSize => f.write_str("zero width or height"),
            FrameError::TooLarge => write!(f, "larger than {MAX_SIDE} pixels a side"),
            FrameError::Fourcc(c) => write!(f, "unknown fourcc {c:#010x}"),
            FrameError::InvalidModifier => f.write_str("the modifier is DRM_FORMAT_MOD_INVALID"),
        }
    }
}

impl std::error::Error for FrameError {}

impl Frame {
    /// Everything the import would trip over, checked before any index.
    pub fn check(&self) -> Result<(), FrameError> {
        if self.plane_count == 0 {
            return Err(FrameError::NoPlanes);
        }
        if self.plane_count > MAX_PLANES {
            return Err(FrameError::TooManyPlanes(self.plane_count));
        }
        if self.width == 0 || self.height == 0 {
            return Err(FrameError::ZeroSize);
        }
        if self.width > MAX_SIDE || self.height > MAX_SIDE {
            return Err(FrameError::TooLarge);
        }
        if let Some(i) = self.planes().iter().position(|p| p.fd < 0) {
            return Err(FrameError::BadFd(i));
        }
        if DrmFourcc::try_from(self.fourcc).is_err() {
            return Err(FrameError::Fourcc(self.fourcc));
        }
        if self.modifier == MOD_INVALID {
            return Err(FrameError::InvalidModifier);
        }
        Ok(())
    }

    /// The planes in use; a count beyond the array is clamped, never indexed.
    pub fn planes(&self) -> &[Plane] {
        &self.planes[..self.plane_count.min(MAX_PLANES)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12() -> Frame {
        let mut planes = [Plane::default(); MAX_PLANES];
        planes[0] = Plane {
            fd: 7,
            pitch: 800,
            offset: 0,
        };
        planes[1] = Plane {
            fd: 7,
            pitch: 800,
            offset: 800 * 480,
        };
        Frame {
            fourcc: DrmFourcc::Nv12 as u32,
            width: 800,
            height: 480,
            planes,
            plane_count: 2,
            modifier: 0,
        }
    }

    #[test]
    fn a_good_frame_passes() {
        assert_eq!(nv12().check(), Ok(()));
        assert_eq!(nv12().planes().len(), 2);
    }

    #[test]
    fn plane_count_is_bounded() {
        let mut f = nv12();
        f.plane_count = 0;
        assert_eq!(f.check(), Err(FrameError::NoPlanes));
        f.plane_count = 5;
        assert_eq!(f.check(), Err(FrameError::TooManyPlanes(5)));
        // Never indexed beyond the array either.
        assert_eq!(f.planes().len(), MAX_PLANES);
    }

    #[test]
    fn descriptors_are_checked() {
        let mut f = nv12();
        f.planes[1].fd = -1;
        assert_eq!(f.check(), Err(FrameError::BadFd(1)));
        // An unused plane may hold anything.
        f.planes[1].fd = 7;
        f.planes[3].fd = -1;
        assert_eq!(f.check(), Ok(()));
    }

    #[test]
    fn size_is_checked() {
        let mut f = nv12();
        f.height = 0;
        assert_eq!(f.check(), Err(FrameError::ZeroSize));
        f.height = 480;
        f.width = MAX_SIDE + 1;
        assert_eq!(f.check(), Err(FrameError::TooLarge));
    }

    #[test]
    fn format_and_modifier_are_checked() {
        let mut f = nv12();
        f.fourcc = 0x0123_4567;
        assert_eq!(f.check(), Err(FrameError::Fourcc(0x0123_4567)));
        f.fourcc = DrmFourcc::Nv12 as u32;
        f.modifier = MOD_INVALID;
        assert_eq!(f.check(), Err(FrameError::InvalidModifier));
        // A vendor modifier the crate does not list is still a modifier.
        f.modifier = 0x0900_0000_0000_0001;
        assert_eq!(f.check(), Ok(()));
    }
}
