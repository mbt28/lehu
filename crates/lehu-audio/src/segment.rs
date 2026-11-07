//! One segment of PCM as a protocol hands it over: S16 interleaved samples
//! at a stated rate and channel count, filled in place, carried through
//! the queue by value and reused.

/// A stream's shape. On the wire between the protocols and the mixer it is
/// one word, `(rate << 4) | channels`, always above 0xFFFF; the two small
/// legacy values 4 (48 kHz stereo media) and 5 (16 kHz mono speech) are
/// what the Android Auto path advertised to the phone and still uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u8,
}

impl Format {
    pub const STEREO_48K: Format = Format {
        rate: 48000,
        channels: 2,
    };
    pub const MONO_16K: Format = Format {
        rate: 16000,
        channels: 1,
    };

    pub const fn new(rate: u32, channels: u8) -> Format {
        Format { rate, channels }
    }

    /// The format word for this shape.
    pub const fn word(self) -> u32 {
        (self.rate << 4) | self.channels as u32
    }

    /// The shape a format word describes. A channel count other than 1 or
    /// 2 is taken as stereo; a legacy value other than 5 as 48 kHz stereo.
    pub const fn from_word(v: u32) -> Format {
        if v > 0xFFFF {
            let ch = (v & 0xF) as u8;
            return Format {
                rate: v >> 4,
                channels: if ch == 1 || ch == 2 { ch } else { 2 },
            };
        }
        if v == 5 {
            Format::MONO_16K
        } else {
            Format::STEREO_48K
        }
    }

    /// Milliseconds `frames` last at this rate.
    pub fn ms_of(self, frames: usize) -> u32 {
        if self.rate == 0 {
            return 0;
        }
        (frames as u64 * 1000 / u64::from(self.rate)) as u32
    }
}

/// A PCM segment. The buffer is allocated once, at the size the protocol
/// chooses, and never grows: samples that do not fit are refused rather
/// than reallocated on the wire thread.
pub struct Segment {
    buf: Vec<i16>,
    /// Samples (not frames) of data.
    len: usize,
    pub format: Format,
}

impl Segment {
    /// A segment that takes up to `samples` samples.
    pub fn with_capacity(samples: usize) -> Self {
        Self {
            buf: vec![0; samples],
            len: 0,
            format: Format::STEREO_48K,
        }
    }

    /// A segment holding a copy of `data`, as big as it needs to be. For
    /// tests; a protocol reuses `with_capacity` segments.
    pub fn from_slice(format: Format, data: &[i16]) -> Self {
        let mut s = Self::with_capacity(data.len());
        s.format = format;
        s.write(data);
        s
    }

    /// Samples this segment can hold.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Samples of data.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Frames of data (samples over channels).
    pub fn frames(&self) -> usize {
        self.len / usize::from(self.format.channels.max(1))
    }

    /// How long the data lasts.
    pub fn duration_ms(&self) -> u32 {
        self.format.ms_of(self.frames())
    }

    /// Empty, ready to be filled again. The format stays.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Append `data`. False, and nothing written, when it does not fit.
    pub fn write(&mut self, data: &[i16]) -> bool {
        let Some(end) = self.len.checked_add(data.len()) else {
            return false;
        };
        if end > self.buf.len() {
            return false;
        }
        self.buf[self.len..end].copy_from_slice(data);
        self.len = end;
        true
    }

    /// Room for `n` more samples, to be filled by the caller. None when they
    /// do not fit. The samples count as data as soon as this returns.
    pub fn space(&mut self, n: usize) -> Option<&mut [i16]> {
        let end = self.len.checked_add(n)?;
        if end > self.buf.len() {
            return None;
        }
        let start = self.len;
        self.len = end;
        Some(&mut self.buf[start..end])
    }

    /// Let `f` write into the free tail and say how many samples it wrote;
    /// those become data. For a decoder that only knows its output length
    /// afterwards. None, and nothing changed, when the tail is empty.
    pub fn fill(&mut self, f: impl FnOnce(&mut [i16]) -> usize) -> Option<usize> {
        if self.len >= self.buf.len() {
            return None;
        }
        let free = self.buf.len() - self.len;
        let n = f(&mut self.buf[self.len..]).min(free);
        self.len += n;
        Some(n)
    }

    pub fn data(&self) -> &[i16] {
        &self.buf[..self.len]
    }

    /// Scale every sample by `gain_q15` (32768 = unchanged, never more):
    /// a stream's level, applied once as it enters the mixer.
    pub fn scale(&mut self, gain_q15: i32) {
        if gain_q15 >= 32768 {
            return;
        }
        let g = gain_q15.max(0);
        for s in &mut self.buf[..self.len] {
            *s = ((i32::from(*s) * g) >> 15) as i16;
        }
    }

    /// Every sample within `level` of zero: digital silence, what a phone
    /// sends while a stream stays open and nothing plays.
    pub fn is_silent(&self, level: i16) -> bool {
        self.data().iter().all(|&s| s <= level && s >= -level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_word_round_trips_the_carplay_shapes() {
        for f in [
            Format::new(48000, 2),
            Format::new(16000, 1),
            Format::new(24000, 1),
            Format::new(8000, 2),
            Format::new(44100, 2),
        ] {
            assert!(f.word() > 0xFFFF);
            assert_eq!(Format::from_word(f.word()), f);
        }
    }

    #[test]
    fn format_word_legacy_values_and_bad_channels() {
        assert_eq!(Format::from_word(4), Format::STEREO_48K);
        assert_eq!(Format::from_word(5), Format::MONO_16K);
        assert_eq!(Format::from_word(0), Format::STEREO_48K);
        assert_eq!(Format::from_word(0xFFFF), Format::STEREO_48K);
        // A channel count outside 1..2 is stereo.
        assert_eq!(Format::from_word((48000 << 4) | 6), Format::STEREO_48K);
        assert_eq!(Format::from_word(48000 << 4), Format::STEREO_48K);
    }

    #[test]
    fn scale_by_a_level() {
        let mut s = Segment::from_slice(Format::MONO_16K, &[1000, -1000, i16::MAX, i16::MIN]);
        s.scale(32768);
        assert_eq!(s.data(), &[1000, -1000, i16::MAX, i16::MIN]);
        s.scale(16384);
        assert_eq!(s.data(), &[500, -500, 16383, -16384]);
        s.scale(0);
        assert_eq!(s.data(), &[0; 4]);
    }

    #[test]
    fn durations() {
        assert_eq!(Format::new(48000, 2).ms_of(1024), 21);
        assert_eq!(Format::new(16000, 1).ms_of(120), 7);
        assert_eq!(Format::new(0, 1).ms_of(120), 0);
        let s = Segment::from_slice(Format::new(48000, 2), &[0; 2048]);
        assert_eq!(s.frames(), 1024);
        assert_eq!(s.duration_ms(), 21);
    }

    #[test]
    fn writes_are_bounded_by_the_capacity() {
        let mut s = Segment::with_capacity(4);
        assert!(s.is_empty());
        assert!(s.write(&[1, 2, 3]));
        assert!(!s.write(&[4, 5]));
        assert_eq!(s.data(), &[1, 2, 3]);
        assert!(s.write(&[4]));
        assert_eq!(s.len(), 4);
        assert!(s.space(1).is_none());
        assert!(s.fill(|_| 1).is_none());
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.capacity(), 4);
    }

    #[test]
    fn space_and_fill_hand_out_the_tail() {
        let mut s = Segment::with_capacity(8);
        s.write(&[9]);
        s.space(3).expect("fits").copy_from_slice(&[1, 2, 3]);
        assert_eq!(s.data(), &[9, 1, 2, 3]);
        let n = s
            .fill(|tail| {
                assert_eq!(tail.len(), 4);
                tail[0] = 7;
                tail[1] = 8;
                2
            })
            .expect("room");
        assert_eq!(n, 2);
        assert_eq!(s.data(), &[9, 1, 2, 3, 7, 8]);
        // A fill that claims more than the tail is cut to the tail.
        assert_eq!(s.fill(|_| 99), Some(2));
        assert_eq!(s.len(), 8);
    }

    #[test]
    fn silence() {
        let s = Segment::from_slice(Format::STEREO_48K, &[0, 4, -4, 3]);
        assert!(s.is_silent(4));
        assert!(!s.is_silent(3));
        let s = Segment::from_slice(Format::STEREO_48K, &[0, 0, 5, 0]);
        assert!(!s.is_silent(4));
    }
}
