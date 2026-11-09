// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The phone's video: Annex-B access units in, pictures on the video plane
//! out, with the behaviour FastCarPlay's V4l2DrmDecoder earned on the board.
//!
//! - The decode runs on ffmpeg's v4l2-request hwaccel (the in-kernel cedrus
//!   stateless decoder) through a small C helper (`csrc/decoder.c`). Each
//!   picture is a DRM PRIME dma-buf that `lehu_drm::Display` imports once per
//!   pool buffer and flips without a copy.
//! - Every frame is decoded, only the newest is shown. A commit that finds
//!   the previous flip pending hands the frame back as `Busy`; it waits as
//!   the pending frame, a newer one supersedes it, and the main loop retries
//!   it from `Decoder::on_flip` when the flip completes. A decoder that
//!   waited on the vblank ran at 30 fps against the phone's 60, its queue
//!   overflowed and the dropped frames were references: the picture smeared
//!   until the next keyframe.
//! - The last three shown pictures stay referenced: the sun4i CRTC signals a
//!   flip one frame early when a commit starts before the previous one has
//!   latched, so the picture two commits back can still be on screen, and a
//!   buffer given back to the decoder then is written mid-scan.
//! - When the input queue did drop a frame the picture is held until a
//!   keyframe, which the protocol is asked for, or 300 ms; a phone that
//!   ignores the request costs a stutter, not a freeze.
//! - There is no flush. `avcodec_flush_buffers` on v4l2-request allocates a
//!   second cedrus pool next to the live one, and this board's CMA holds one;
//!   ffmpeg then decodes in software without saying so. A session's decoder
//!   is stopped on disconnect (the pool goes back to CMA) and started again
//!   on connect.
//! - A line of statistics every 5 s while frames flow, in FastCarPlay's
//!   format, so the two binaries compare on the same log.

pub mod annexb;
mod choice;
mod decoder;
mod ffi;
mod present;
mod probe;
mod resync;
mod stats;
mod unit;

pub use choice::{android_codecs, carplay_codec};
pub use decoder::{Decoder, Error, KeyframeRequest, Options, Stats};
pub use probe::{Caps, Codec, Ffmpeg, Format, Hardware, Support, caps, hw_available};
pub use unit::{AccessUnit, PADDING};
