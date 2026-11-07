//! The phone's audio: PCM segments in, the sound card out, with the
//! behaviour FastCarPlay's mixer earned on the board, and the pieces around
//! it.
//!
//! - [`Mixer`]: one thread owns the ALSA PCM and mixes two queues of
//!   [`Segment`]s, main (music) and aux (voice, prompts, the call), in Q15
//!   integer arithmetic at 48 kHz stereo, the only shape the F1C codec
//!   plays. Music waits for its jitter buffer (`buffer-ms`, live from the
//!   settings) and refills after running dry; voice waits for a few
//!   segments and is capped at `voice-max-latency-ms`; a backlog a second
//!   past the music buffer is skipped to the live edge; audible voice ducks
//!   the music to the `fade` floor and releases after four quiet periods;
//!   the device is released after `idle-hold-ms` of silence so the DAC and
//!   amplifier power down. A `[Audio]` line every 5 s says how it went.
//! - [`Volume`]: the level and mute, through the card's own control via
//!   ALSA's simple mixer (cubic dB mapping) or a software gain the mixer
//!   ramps; the save of the level debounced by 2 s, handed to the caller.
//! - [`Decoder`], [`AacDecoder`], [`OpusDecoder`]: CarPlay's packets to
//!   PCM through ffmpeg's fixed-point AAC and the fixed-point libopus.
//! - [`Mic`]: capture, DC-blocked and framed as the phone asks, coded to
//!   PCM or Opus, into the session's microphone queue.
//! - [`outputs`]: the playback devices the kernel lists, for the screens.
//!
//! Everything per sample is integer; no `f32` touches the audio.

mod aac;
mod alsa;
mod cards;
mod decoder;
mod engine;
mod fixed;
mod mic;
mod mixer;
mod opus;
mod priority;
mod segment;
mod source;
mod stats;
mod volume;

use std::fmt;

pub use aac::{AacDecoder, MAX_UNIT, available as aac_available};
pub use cards::{Output, opens, outputs};
pub use decoder::{Decoder, decode_pcm_be, pcm_samples};
pub use engine::{MAX_BUFFER_MS, MIX_PERIOD, MIX_PERIOD_MS, SILENCE_LEVEL};
pub use mic::{DcBlocker, Mic, frame_samples};
pub use mixer::{AUX_DEPTH, Input, MAIN_DEPTH, Mixer};
pub use opus::{MAX_FRAME, OpusDecoder, OpusEncoder, available as opus_available};
pub use segment::{Format, Segment};
pub use source::MIX_RATE;
pub use volume::{Volume, mixer_device_for};

/// What can go wrong with a decoder, an encoder or the microphone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The library is not in this build (a host without it).
    Unavailable(&'static str),
    /// The codec refused: the call and its code.
    Codec { what: &'static str, code: i32 },
    /// A shape or a length the code cannot take.
    Format(&'static str),
    /// The segment has no room for the frame.
    NoRoom,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unavailable(lib) => write!(f, "built without {lib}"),
            Error::Codec { what, code } => write!(f, "{what}: error {code}"),
            Error::Format(what) => f.write_str(what),
            Error::NoRoom => f.write_str("no room in the segment"),
        }
    }
}

impl std::error::Error for Error {}
