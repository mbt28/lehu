// SPDX-License-Identifier: GPL-3.0-or-later
//! libopus, declared here: the image's build is fixed point, so decoding
//! lands in S16 with no float step (ffmpeg's libopus wrapper only decodes
//! to float, and float to S16 per sample on this core is soft-float and
//! dominates the cost). The decoder's rate is independent of the packets'
//! (Opus resamples inside), so one 48 kHz decoder takes the phone's 16 and
//! 24 kHz voice without a resampler of our own.

/// The longest Opus packet, 120 ms, at 48 kHz.
pub const MAX_FRAME: usize = 5760;

#[cfg(not(lehu_audio_no_opus))]
mod real {
    use std::ffi::c_int;
    use std::ptr::NonNull;

    use super::MAX_FRAME;
    use crate::Error;
    use crate::segment::{Format, Segment};

    #[repr(C)]
    pub struct OpusDecoderRaw {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct OpusEncoderRaw {
        _p: [u8; 0],
    }

    const OPUS_OK: c_int = 0;
    const OPUS_APPLICATION_VOIP: c_int = 2048;
    const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
    const OPUS_SET_VBR_REQUEST: c_int = 4006;
    const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
    const OPUS_SET_DTX_REQUEST: c_int = 4016;
    const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
    const OPUS_SIGNAL_VOICE: c_int = 3001;

    unsafe extern "C" {
        fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoderRaw;
        fn opus_decode(
            st: *mut OpusDecoderRaw,
            data: *const u8,
            len: i32,
            pcm: *mut i16,
            frame_size: c_int,
            decode_fec: c_int,
        ) -> c_int;
        fn opus_decoder_destroy(st: *mut OpusDecoderRaw);
        fn opus_encoder_create(
            fs: i32,
            channels: c_int,
            application: c_int,
            error: *mut c_int,
        ) -> *mut OpusEncoderRaw;
        fn opus_encode(
            st: *mut OpusEncoderRaw,
            pcm: *const i16,
            frame_size: c_int,
            data: *mut u8,
            max_data_bytes: i32,
        ) -> i32;
        fn opus_encoder_ctl(st: *mut OpusEncoderRaw, request: c_int, ...) -> c_int;
        fn opus_encoder_destroy(st: *mut OpusEncoderRaw);
    }

    fn rate_ok(rate: u32) -> bool {
        matches!(rate, 8000 | 12000 | 16000 | 24000 | 48000)
    }

    /// Opus packets to S16 PCM.
    pub struct OpusDecoder {
        raw: NonNull<OpusDecoderRaw>,
        rate: u32,
        channels: u8,
        upmix: bool,
        /// The mono decode before duplication; allocated once.
        scratch: Vec<i16>,
    }

    // SAFETY: the state is used by one thread at a time; not `Sync`.
    unsafe impl Send for OpusDecoder {}

    impl OpusDecoder {
        /// A decoder putting out `rate` Hz, `channels` channels; a mono
        /// stream is duplicated to stereo when `upmix`.
        pub fn new(rate: u32, channels: u8, upmix: bool) -> Result<Self, Error> {
            if !rate_ok(rate) {
                return Err(Error::Format("Opus rate: 8, 12, 16, 24 or 48 kHz"));
            }
            if channels != 1 && channels != 2 {
                return Err(Error::Format("Opus channels: 1 or 2"));
            }
            let mut err: c_int = 0;
            // SAFETY: err receives the code.
            let raw =
                unsafe { opus_decoder_create(rate as i32, c_int::from(channels), &raw mut err) };
            let Some(raw) = NonNull::new(raw).filter(|_| err == OPUS_OK) else {
                return Err(Error::Codec {
                    what: "opus_decoder_create",
                    code: err,
                });
            };
            let upmix = upmix && channels == 1;
            Ok(OpusDecoder {
                raw,
                rate,
                channels,
                upmix,
                scratch: vec![0; if upmix { MAX_FRAME } else { 0 }],
            })
        }

        /// The shape of what `decode` writes.
        pub fn output(&self) -> Format {
            Format::new(self.rate, if self.upmix { 2 } else { self.channels })
        }

        /// One packet to the end of `out`, whose format is set. Frames
        /// written.
        pub fn decode(&mut self, packet: &[u8], out: &mut Segment) -> Result<usize, Error> {
            let Ok(len) = i32::try_from(packet.len()) else {
                return Err(Error::Format("Opus packet too long"));
            };
            if len == 0 {
                return Err(Error::Format("empty Opus packet"));
            }
            out.format = self.output();
            let ch = usize::from(self.channels);
            if self.upmix {
                // SAFETY: scratch holds MAX_FRAME mono samples; the packet
                // is readable for len bytes.
                let n = unsafe {
                    opus_decode(
                        self.raw.as_ptr(),
                        packet.as_ptr(),
                        len,
                        self.scratch.as_mut_ptr(),
                        MAX_FRAME as c_int,
                        0,
                    )
                };
                let frames = usize::try_from(n).map_err(|_| Error::Codec {
                    what: "opus_decode",
                    code: n,
                })?;
                let Some(dst) = out.space(2 * frames) else {
                    return Err(Error::NoRoom);
                };
                for (pair, &s) in dst
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(&self.scratch[..frames])
                {
                    pair[0] = s;
                    pair[1] = s;
                }
                return Ok(frames);
            }
            let raw = self.raw;
            let mut code = 0;
            let written = out.fill(|tail| {
                let frame_size = (tail.len() / ch).min(MAX_FRAME);
                // SAFETY: tail holds frame_size * ch samples; the packet is
                // readable for len bytes.
                let n = unsafe {
                    opus_decode(
                        raw.as_ptr(),
                        packet.as_ptr(),
                        len,
                        tail.as_mut_ptr(),
                        frame_size as c_int,
                        0,
                    )
                };
                match usize::try_from(n) {
                    Ok(frames) => frames * ch,
                    Err(_) => {
                        code = n;
                        0
                    }
                }
            });
            match written {
                None => Err(Error::NoRoom),
                Some(_) if code < 0 => Err(Error::Codec {
                    what: "opus_decode",
                    code,
                }),
                Some(samples) => Ok(samples / ch),
            }
        }
    }

    impl Drop for OpusDecoder {
        fn drop(&mut self) {
            // SAFETY: created once, destroyed once.
            unsafe { opus_decoder_destroy(self.raw.as_ptr()) }
        }
    }

    /// Mono voice to Opus packets, for the microphone: VOIP, voice
    /// signal, low complexity (the ARM926 budget) and the bits for the
    /// rate; DTX off so the RTP timestamps stay continuous.
    pub struct OpusEncoder {
        raw: NonNull<OpusEncoderRaw>,
    }

    // SAFETY: as for the decoder.
    unsafe impl Send for OpusEncoder {}

    impl OpusEncoder {
        pub fn new(rate: u32) -> Result<Self, Error> {
            if !rate_ok(rate) {
                return Err(Error::Format("Opus rate: 8, 12, 16, 24 or 48 kHz"));
            }
            let mut err: c_int = 0;
            // SAFETY: err receives the code.
            let raw =
                unsafe { opus_encoder_create(rate as i32, 1, OPUS_APPLICATION_VOIP, &raw mut err) };
            let Some(raw) = NonNull::new(raw).filter(|_| err == OPUS_OK) else {
                return Err(Error::Codec {
                    what: "opus_encoder_create",
                    code: err,
                });
            };
            let bitrate: c_int = if rate <= 24000 {
                48000
            } else if rate <= 32000 {
                64000
            } else {
                96000
            };
            // SAFETY: each request takes one int, as libopus documents.
            unsafe {
                opus_encoder_ctl(raw.as_ptr(), OPUS_SET_BITRATE_REQUEST, bitrate);
                opus_encoder_ctl(raw.as_ptr(), OPUS_SET_VBR_REQUEST, 1 as c_int);
                opus_encoder_ctl(raw.as_ptr(), OPUS_SET_COMPLEXITY_REQUEST, 1 as c_int);
                opus_encoder_ctl(raw.as_ptr(), OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE);
                opus_encoder_ctl(raw.as_ptr(), OPUS_SET_DTX_REQUEST, 0 as c_int);
            }
            Ok(OpusEncoder { raw })
        }

        /// One frame of mono samples (a valid Opus duration: 2.5, 5, 10,
        /// 20, 40 or 60 ms) into `out`. Bytes written.
        pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, Error> {
            let (Ok(frames), Ok(max)) = (c_int::try_from(pcm.len()), i32::try_from(out.len()))
            else {
                return Err(Error::Format("Opus frame too long"));
            };
            // SAFETY: pcm holds frames mono samples; out is writable for
            // max bytes.
            let n = unsafe {
                opus_encode(
                    self.raw.as_ptr(),
                    pcm.as_ptr(),
                    frames,
                    out.as_mut_ptr(),
                    max,
                )
            };
            usize::try_from(n).map_err(|_| Error::Codec {
                what: "opus_encode",
                code: n,
            })
        }
    }

    impl Drop for OpusEncoder {
        fn drop(&mut self) {
            // SAFETY: created once, destroyed once.
            unsafe { opus_encoder_destroy(self.raw.as_ptr()) }
        }
    }
}

#[cfg(lehu_audio_no_opus)]
mod stub {
    use crate::Error;
    use crate::segment::{Format, Segment};

    /// Never opens: there is no libopus on this host.
    pub struct OpusDecoder(());

    impl OpusDecoder {
        pub fn new(_rate: u32, _channels: u8, _upmix: bool) -> Result<Self, Error> {
            Err(Error::Unavailable("libopus"))
        }

        pub fn output(&self) -> Format {
            Format::STEREO_48K
        }

        pub fn decode(&mut self, _packet: &[u8], _out: &mut Segment) -> Result<usize, Error> {
            Err(Error::Unavailable("libopus"))
        }
    }

    pub struct OpusEncoder(());

    impl OpusEncoder {
        pub fn new(_rate: u32) -> Result<Self, Error> {
            Err(Error::Unavailable("libopus"))
        }

        pub fn encode(&mut self, _pcm: &[i16], _out: &mut [u8]) -> Result<usize, Error> {
            Err(Error::Unavailable("libopus"))
        }
    }
}

#[cfg(not(lehu_audio_no_opus))]
pub use real::{OpusDecoder, OpusEncoder};
#[cfg(lehu_audio_no_opus)]
pub use stub::{OpusDecoder, OpusEncoder};

/// Whether this build has libopus.
pub fn available() -> bool {
    cfg!(not(lehu_audio_no_opus))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::segment::{Format, Segment};

    fn skip() -> bool {
        if !available() {
            eprintln!("no libopus on this host: Opus tests skipped");
            return true;
        }
        false
    }

    #[test]
    fn a_toc_only_packet_decodes_to_one_frame_of_silence() {
        if skip() {
            return;
        }
        // One byte: the TOC alone (config 31: CELT fullband, 20 ms, code
        // 0), a frame of no data, which decodes as 960 frames of nothing.
        let mut d = OpusDecoder::new(48000, 1, true).expect("decoder");
        assert_eq!(d.output(), Format::new(48000, 2));
        let mut seg = Segment::with_capacity(2 * MAX_FRAME);
        let frames = d.decode(&[0xfc], &mut seg).expect("decoded");
        assert_eq!(frames, 960);
        assert_eq!(seg.len(), 2 * 960);
        assert!(seg.is_silent(4));
        assert!(matches!(d.decode(&[], &mut seg), Err(Error::Format(_))));
        // No room: refused, nothing written.
        let mut small = Segment::with_capacity(100);
        assert!(matches!(d.decode(&[0xfc], &mut small), Err(Error::NoRoom)));
        assert!(small.is_empty());
    }

    #[test]
    fn a_voice_frame_round_trips() {
        if skip() {
            return;
        }
        let mut e = OpusEncoder::new(16000).expect("encoder");
        // 20 ms of a 440 Hz tone at 16 kHz, from a small table of its own
        // (the test may use floats; the crate does not).
        let tone: Vec<i16> = (0..320)
            .map(|i| ((i as f64 * 440.0 * std::f64::consts::TAU / 16000.0).sin() * 12000.0) as i16)
            .collect();
        let mut packet = [0u8; 1024];
        let n = e.encode(&tone, &mut packet).expect("encoded");
        assert!(n > 2 && n < 400, "{n} bytes");
        // Decoded at 48 kHz mono, no upmix: 960 frames straight into the
        // segment.
        let mut d = OpusDecoder::new(48000, 1, false).expect("decoder");
        let mut seg = Segment::with_capacity(MAX_FRAME);
        let frames = d.decode(&packet[..n], &mut seg).expect("decoded");
        assert_eq!(frames, 960);
        assert_eq!(seg.format, Format::new(48000, 1));
        // The first packet is the codec's warm-up; the second carries the
        // tone.
        seg.clear();
        let n = e.encode(&tone, &mut packet).expect("encoded");
        d.decode(&packet[..n], &mut seg).expect("decoded");
        let peak = seg
            .data()
            .iter()
            .map(|s| s.saturating_abs())
            .max()
            .unwrap_or(0);
        assert!(peak > 3000, "peak {peak}");
        // Stereo from a stereo decoder.
        let mut d = OpusDecoder::new(48000, 2, false).expect("decoder");
        let mut seg = Segment::with_capacity(2 * MAX_FRAME);
        assert_eq!(d.decode(&packet[..n], &mut seg).expect("decoded"), 960);
        assert_eq!(seg.len(), 2 * 960);
        assert!(OpusDecoder::new(44100, 1, false).is_err());
    }
}
