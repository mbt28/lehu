// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! AAC-LC through the C helper (`csrc/aac.c`) on libavcodec's fixed-point
//! decoder. Without ffmpeg's headers on the host the stub below takes its
//! place and every open fails.

/// The longest access unit taken: AAC-LC's own bound is 6144 bits per
/// channel, far below this.
pub const MAX_UNIT: usize = 8192;
/// Readable bytes the helper needs after the unit (ffmpeg's padding).
const PADDING: usize = 64;

#[cfg(not(lehu_audio_no_aac))]
mod real {
    use std::ffi::{CStr, c_char, c_int, c_long};
    use std::ptr::NonNull;

    use super::{MAX_UNIT, PADDING};
    use crate::Error;
    use crate::segment::{Format, Segment};

    #[repr(C)]
    struct Dec {
        _p: [u8; 0],
    }

    unsafe extern "C" {
        fn lehu_aac_open(rate: c_int, channels: c_int, err: *mut c_char, errlen: usize)
        -> *mut Dec;
        fn lehu_aac_name(d: *const Dec) -> *const c_char;
        fn lehu_aac_decode(
            d: *mut Dec,
            data: *const u8,
            len: usize,
            out: *mut i16,
            cap: usize,
            upmix: c_int,
        ) -> c_long;
        fn lehu_aac_close(d: *mut Dec);
    }

    fn message(buf: &[c_char]) -> String {
        // SAFETY: the helper always terminates what it writes into `buf`.
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// AAC-LC access units to S16 PCM.
    pub struct AacDecoder {
        raw: NonNull<Dec>,
        rate: u32,
        channels: u8,
        /// The unit with the padding ffmpeg reads past its end; allocated
        /// once.
        scratch: Vec<u8>,
    }

    // SAFETY: the helper keeps no thread-local state; the context is used
    // by one thread at a time and `AacDecoder` is not `Sync`.
    unsafe impl Send for AacDecoder {}

    impl AacDecoder {
        /// A decoder for a stream of `rate` Hz, `channels` channels (the
        /// AudioSpecificConfig is built from them; Apple sends none).
        pub fn new(rate: u32, channels: u8) -> Result<Self, Error> {
            if channels != 1 && channels != 2 {
                return Err(Error::Format("AAC channels: 1 or 2"));
            }
            let Ok(rate_c) = c_int::try_from(rate) else {
                return Err(Error::Format("AAC rate"));
            };
            let mut err = [0 as c_char; 96];
            // SAFETY: err is a writable buffer of the length given.
            let raw = unsafe {
                lehu_aac_open(rate_c, c_int::from(channels), err.as_mut_ptr(), err.len())
            };
            let Some(raw) = NonNull::new(raw) else {
                log::error!("aac: {}", message(&err));
                return Err(Error::Codec {
                    what: "aac open",
                    code: -libc::EINVAL,
                });
            };
            Ok(AacDecoder {
                raw,
                rate,
                channels,
                scratch: vec![0; MAX_UNIT + PADDING],
            })
        }

        /// ffmpeg's name for the decoder in use ("aac_fixed" on the board).
        pub fn name(&self) -> String {
            // SAFETY: open; the name is a static string.
            unsafe { CStr::from_ptr(lehu_aac_name(self.raw.as_ptr())) }
                .to_string_lossy()
                .into_owned()
        }

        pub fn output(&self) -> Format {
            Format::new(self.rate, self.channels)
        }

        /// One access unit to the end of `out`, whose format is set.
        /// Frames written (0 when the decoder kept the unit).
        pub fn decode(&mut self, unit: &[u8], out: &mut Segment) -> Result<usize, Error> {
            if unit.is_empty() || unit.len() > MAX_UNIT {
                return Err(Error::Format("AAC unit length"));
            }
            self.scratch[..unit.len()].copy_from_slice(unit);
            self.scratch[unit.len()..unit.len() + PADDING].fill(0);
            out.format = self.output();
            let ch = usize::from(self.channels);
            let raw = self.raw;
            let data = self.scratch.as_ptr();
            let mut code: c_long = 0;
            let written = out.fill(|tail| {
                // SAFETY: data is readable for unit.len() + PADDING bytes;
                // tail is writable for its length.
                let n = unsafe {
                    lehu_aac_decode(
                        raw.as_ptr(),
                        data,
                        unit.len(),
                        tail.as_mut_ptr(),
                        tail.len(),
                        0,
                    )
                };
                match usize::try_from(n) {
                    Ok(samples) => samples,
                    Err(_) => {
                        code = n;
                        0
                    }
                }
            });
            match written {
                None => Err(Error::NoRoom),
                Some(_) if code == c_long::from(-libc::ENOSPC) => Err(Error::NoRoom),
                Some(_) if code < 0 => Err(Error::Codec {
                    what: "aac decode",
                    code: code as i32,
                }),
                Some(samples) => Ok(samples / ch),
            }
        }
    }

    impl Drop for AacDecoder {
        fn drop(&mut self) {
            // SAFETY: open, closed once, here.
            unsafe { lehu_aac_close(self.raw.as_ptr()) }
        }
    }
}

#[cfg(lehu_audio_no_aac)]
mod stub {
    use crate::Error;
    use crate::segment::{Format, Segment};

    /// Never opens: there is no ffmpeg on this host.
    pub struct AacDecoder(());

    impl AacDecoder {
        pub fn new(_rate: u32, _channels: u8) -> Result<Self, Error> {
            Err(Error::Unavailable("libavcodec"))
        }

        pub fn name(&self) -> String {
            String::new()
        }

        pub fn output(&self) -> Format {
            Format::STEREO_48K
        }

        pub fn decode(&mut self, _unit: &[u8], _out: &mut Segment) -> Result<usize, Error> {
            Err(Error::Unavailable("libavcodec"))
        }
    }
}

#[cfg(not(lehu_audio_no_aac))]
pub use real::AacDecoder;
#[cfg(lehu_audio_no_aac)]
pub use stub::AacDecoder;

/// Whether this build has libavcodec.
pub fn available() -> bool {
    cfg!(not(lehu_audio_no_aac))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::segment::{Format, Segment};

    /// One raw AAC-LC access unit of digital silence, 48 kHz stereo, as
    /// ffmpeg's encoder produces it (`anullsrc`, ADTS header stripped).
    const SILENT_UNIT: [u8; 6] = [0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c];

    #[test]
    fn opens_and_decodes_a_silent_unit() {
        if !available() {
            eprintln!("no ffmpeg on this host: AAC tests skipped");
            return;
        }
        let mut d = AacDecoder::new(48000, 2).expect("decoder");
        assert!(!d.name().is_empty());
        assert_eq!(d.output(), Format::STEREO_48K);
        let mut seg = Segment::with_capacity(2 * 1024);
        // The first unit primes the decoder; by the second a frame is out.
        let mut frames = 0;
        for _ in 0..3 {
            frames += d.decode(&SILENT_UNIT, &mut seg).expect("decoded");
            if frames > 0 {
                break;
            }
        }
        assert_eq!(frames, 1024);
        assert_eq!(seg.len(), 2 * 1024);
        assert!(seg.is_silent(4));
        assert!(matches!(d.decode(&[], &mut seg), Err(Error::Format(_))));
        assert!(matches!(
            d.decode(&[0; MAX_UNIT + 1], &mut seg),
            Err(Error::Format(_))
        ));
        let mut small = Segment::with_capacity(16);
        assert!(matches!(
            d.decode(&SILENT_UNIT, &mut small),
            Err(Error::NoRoom)
        ));
        assert!(AacDecoder::new(48000, 3).is_err());
    }
}
