// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The panel: one connector and its CRTC driven with atomic commits, and
//! two planes always the same way round, as FastCarPlay's display had them.
//!
//! - The phone's video (NV12 from the decoder, de-tiled, converted and
//!   scaled by the display front end) is on the primary plane that takes
//!   NV12. The UI is on the first overlay plane, RGB565 when the plane has
//!   it (half the memory and copy bandwidth of 32-bit), else ARGB8888, else
//!   XRGB8888.
//! - The UI is one dumb buffer the screens write into by rows. A dumb buffer
//!   is write-combined memory: nothing here ever reads one back. The plane is
//!   committed over the band of rows that carries content, or over a window
//!   above live video, and disabled when there is nothing to show, so it
//!   costs no scanout bandwidth while video plays.
//! - Decoded pictures arrive as dma-bufs and are imported once per pool
//!   buffer (the framebuffer cache). Video commits never block the decoder:
//!   a flip still pending hands the frame back as `Show::Busy`.
//! - Some drivers refuse to enable the CRTC with no primary plane, so a
//!   UI-only modeset (the home screen before any video) may need a black
//!   placeholder on the video plane; the first real frames replace and free
//!   it.
//!
//! One `Display` is shared between the main thread (UI, camera) and the
//! decoder thread through an `Arc`: every method takes `&self` and the
//! state sits behind one mutex, as it did in C++.

mod band;
mod cache;
mod display;
mod error;
mod frame;
mod request;

pub use display::{Display, Show};
pub use drm::buffer::DrmFourcc as Fourcc;
pub use error::Error;
pub use frame::{Frame, FrameError, MAX_PLANES, Plane};
