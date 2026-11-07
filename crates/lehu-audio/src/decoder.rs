//! The decoders behind one face, keyed by the session's `AudioFormat`:
//! what the loop opens when a CarPlay stream starts and feeds from
//! `Sinks::audio_packet`, one packet in, PCM appended to a `Segment` out.
//!
//! The shapes follow what the phone negotiates: AAC-LC media at 44.1 or
//! 48 kHz stereo, Opus voice at 48 kHz mono (duplicated to stereo, as
//! FastCarPlay did), and LPCM at the rate and channels of its format bit
//! (telephony and the assistant's downlink are 16 kHz mono; decoding them
//! as stereo split adjacent samples into a phasey comb and ran 2.75x too
//! fast).

use lehu_session::{AudioCodec, AudioFormat};

use crate::Error;
use crate::aac::AacDecoder;
use crate::opus::OpusDecoder;
use crate::segment::{Format, Segment};

pub enum Decoder {
    /// Native-order S16, interleaved, copied through; mono duplicated when
    /// `upmix`.
    Pcm {
        format: Format,
        upmix: bool,
    },
    Aac(AacDecoder),
    Opus(OpusDecoder),
}

impl Decoder {
    /// A decoder for a stream of `format`.
    pub fn open(format: AudioFormat) -> Result<Decoder, Error> {
        if format.channels != 1 && format.channels != 2 {
            return Err(Error::Format("audio channels: 1 or 2"));
        }
        if format.rate == 0 {
            return Err(Error::Format("audio rate"));
        }
        Ok(match format.codec {
            AudioCodec::Pcm => Decoder::Pcm {
                format: Format::new(format.rate, format.channels),
                upmix: false,
            },
            AudioCodec::AacLc => Decoder::Aac(AacDecoder::new(format.rate, format.channels)?),
            AudioCodec::Opus => Decoder::Opus(OpusDecoder::new(
                format.rate,
                format.channels,
                format.channels == 1,
            )?),
        })
    }

    /// The shape of what `decode` writes.
    pub fn output(&self) -> Format {
        match self {
            Decoder::Pcm { format, upmix } => {
                Format::new(format.rate, if *upmix { 2 } else { format.channels })
            }
            Decoder::Aac(d) => d.output(),
            Decoder::Opus(d) => d.output(),
        }
    }

    /// One packet appended to `out`, whose format is set. Frames written.
    pub fn decode(&mut self, payload: &[u8], out: &mut Segment) -> Result<usize, Error> {
        match self {
            Decoder::Pcm { format, upmix } => {
                out.format = Format::new(format.rate, if *upmix { 2 } else { format.channels });
                pcm_samples(payload, false, *upmix && format.channels == 1, out)
                    .map(|samples| samples / usize::from(out.format.channels))
            }
            Decoder::Aac(d) => d.decode(payload, out),
            Decoder::Opus(d) => d.decode(payload, out),
        }
    }
}

/// S16 samples from the wire appended to `out`: big-endian (CarPlay's
/// LPCM) or native order, mono duplicated when `upmix`. Samples written.
/// An odd byte at the end is ignored.
pub fn pcm_samples(
    payload: &[u8],
    big_endian: bool,
    upmix: bool,
    out: &mut Segment,
) -> Result<usize, Error> {
    let samples = payload.len() / 2;
    let n = if upmix { 2 * samples } else { samples };
    let Some(dst) = out.space(n) else {
        return Err(Error::NoRoom);
    };
    let src = payload.as_chunks::<2>().0.iter().map(|b| {
        if big_endian {
            i16::from_be_bytes([b[0], b[1]])
        } else {
            i16::from_le_bytes([b[0], b[1]])
        }
    });
    if upmix {
        for (pair, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src) {
            pair[0] = s;
            pair[1] = s;
        }
    } else {
        for (d, s) in dst.iter_mut().zip(src) {
            *d = s;
        }
    }
    Ok(n)
}

/// CarPlay's LPCM: big-endian on the wire, at the stream's format, mono
/// duplicated to stereo when `upmix`. Frames written.
pub fn decode_pcm_be(
    payload: &[u8],
    format: Format,
    upmix: bool,
    out: &mut Segment,
) -> Result<usize, Error> {
    if format.channels == 0 || format.rate == 0 {
        return Err(Error::Format("audio channels and rate"));
    }
    let upmix = upmix && format.channels == 1;
    out.format = Format::new(format.rate, if upmix { 2 } else { format.channels });
    pcm_samples(payload, true, upmix, out).map(|samples| samples / usize::from(out.format.channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_passes_through_in_either_order() {
        let mut seg = Segment::with_capacity(8);
        let mut d = Decoder::open(AudioFormat {
            codec: AudioCodec::Pcm,
            rate: 16000,
            channels: 1,
        })
        .expect("pcm");
        assert_eq!(d.output(), Format::MONO_16K);
        assert_eq!(
            d.decode(&[0x01, 0x02, 0xff, 0xff, 0x00], &mut seg)
                .expect("copied"),
            2
        );
        assert_eq!(seg.data(), &[0x0201, -1]);
        assert_eq!(seg.format, Format::MONO_16K);
        seg.clear();
        assert_eq!(
            decode_pcm_be(&[0x01, 0x02, 0xff, 0xfe], Format::MONO_16K, true, &mut seg).expect("be"),
            2
        );
        assert_eq!(seg.data(), &[0x0102, 0x0102, -2, -2]);
        assert_eq!(seg.format, Format::new(16000, 2));
        assert!(matches!(
            decode_pcm_be(&[0; 20], Format::MONO_16K, false, &mut seg),
            Err(Error::NoRoom)
        ));
        assert_eq!(seg.len(), 4, "nothing written when it does not fit");
    }

    #[test]
    fn shapes_are_checked_before_any_library() {
        for bad in [
            AudioFormat {
                codec: AudioCodec::Pcm,
                rate: 48000,
                channels: 3,
            },
            AudioFormat {
                codec: AudioCodec::Opus,
                rate: 0,
                channels: 1,
            },
        ] {
            assert!(matches!(Decoder::open(bad), Err(Error::Format(_))));
        }
        match Decoder::open(AudioFormat {
            codec: AudioCodec::Opus,
            rate: 48000,
            channels: 1,
        }) {
            Ok(d) => assert_eq!(d.output(), Format::new(48000, 2), "mono Opus is upmixed"),
            Err(Error::Unavailable(_)) => eprintln!("no libopus on this host"),
            Err(e) => panic!("{e}"),
        }
        match Decoder::open(AudioFormat {
            codec: AudioCodec::AacLc,
            rate: 44100,
            channels: 2,
        }) {
            Ok(d) => assert_eq!(d.output(), Format::new(44100, 2)),
            Err(Error::Unavailable(_)) => eprintln!("no ffmpeg on this host"),
            Err(e) => panic!("{e}"),
        }
    }
}
