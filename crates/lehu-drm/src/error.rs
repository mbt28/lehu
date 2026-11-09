// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What can go wrong opening the display. Showing a picture never returns
//! an error: `Show` says what became of it.

use std::fmt;
use std::io;

use crate::frame::FrameError;

#[derive(Debug)]
pub enum Error {
    /// The card node could not be opened.
    Open(String, io::Error),
    /// Something else owns the display (a compositor), or we are not on the
    /// active VT: this display is not ours, the caller may run headless.
    NotMaster(String),
    /// The driver has no atomic modesetting. The tiled plane only routes
    /// through the display front end via the atomic API.
    NoAtomic(String),
    /// No connected connector with a mode.
    NoConnector,
    NoCrtc,
    /// No plane on the CRTC takes NV12.
    NoVideoPlane,
    /// An atomic property every KMS driver exposes is missing.
    Property(&'static str),
    /// An ioctl failed; the name says which.
    Io(&'static str, io::Error),
    /// No `/dev/dri/card*` node gave a display.
    NoCard,
    /// A frame that cannot be imported.
    Frame(FrameError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Open(path, e) => write!(f, "{path}: cannot open: {e}"),
            Error::NotMaster(path) => write!(f, "{path}: not DRM master"),
            Error::NoAtomic(path) => write!(f, "{path}: no atomic modesetting"),
            Error::NoConnector => f.write_str("no connected DRM connector"),
            Error::NoCrtc => f.write_str("no CRTC for the connector"),
            Error::NoVideoPlane => f.write_str("no NV12 plane found"),
            Error::Property(name) => write!(f, "missing atomic property {name}"),
            Error::Io(what, e) => write!(f, "{what}: {e}"),
            Error::NoCard => f.write_str("no /dev/dri/card* node gives a display"),
            Error::Frame(e) => write!(f, "frame refused: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Open(_, e) | Error::Io(_, e) => Some(e),
            Error::Frame(e) => Some(e),
            _ => None,
        }
    }
}

impl From<FrameError> for Error {
    fn from(e: FrameError) -> Self {
        Error::Frame(e)
    }
}
