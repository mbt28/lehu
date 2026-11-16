// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A WAV file's samples, for clips LEHU plays itself (the map's spoken
//! prompts): RIFF/WAVE with 16-bit PCM, mono or stereo, at any rate the
//! mixer resamples. Nothing is converted: the clips are made in this shape
//! on a PC, so the board only copies them.

use crate::Error;
use crate::segment::Format;

/// The shape and the samples (interleaved) of a WAV file's bytes.
pub fn read(bytes: &[u8]) -> Result<(Format, Vec<i16>), Error> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::Format("not a RIFF/WAVE file"));
    }
    let mut format = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = at + 8;
        // A data chunk cut short (a writer that never went back to fill
        // in the size) is read to the end of the file.
        let end = body.saturating_add(size).min(bytes.len());
        let chunk = &bytes[body..end];
        match id {
            b"fmt " => {
                if chunk.len() < 16 {
                    return Err(Error::Format("a WAV fmt chunk cut short"));
                }
                let tag = u16::from_le_bytes([chunk[0], chunk[1]]);
                let channels = u16::from_le_bytes([chunk[2], chunk[3]]);
                let rate = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                let bits = u16::from_le_bytes([chunk[14], chunk[15]]);
                // 1 is PCM; 0xFFFE (extensible) is PCM too when 16-bit.
                if tag != 1 && tag != 0xFFFE {
                    return Err(Error::Format("a WAV that is not PCM"));
                }
                if bits != 16 {
                    return Err(Error::Format("a WAV that is not 16-bit"));
                }
                if !(1..=2).contains(&channels) {
                    return Err(Error::Format("a WAV with more than two channels"));
                }
                if !(8000..=48000).contains(&rate) {
                    return Err(Error::Format("a WAV rate outside 8-48 kHz"));
                }
                format = Some(Format::new(rate, channels as u8));
            }
            b"data" => {
                let format = format.ok_or(Error::Format("WAV data before its fmt chunk"))?;
                let samples = chunk
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b))
                    .collect();
                return Ok((format, samples));
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(Error::Format("a WAV without data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, bits: u16, samples: &[i16], extra: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(extra);
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn reads_mono_pcm_past_other_chunks() {
        // A LIST chunk of odd length, padded, before the format.
        let extra = [b"LIST".as_slice(), &3u32.to_le_bytes(), &[1, 2, 3, 0]].concat();
        let bytes = wav(24000, 1, 16, &[1, -2, 300, i16::MIN], &extra);
        let (format, samples) = read(&bytes).unwrap();
        assert_eq!(format, Format::new(24000, 1));
        assert_eq!(samples, vec![1, -2, 300, i16::MIN]);
    }

    #[test]
    fn a_data_size_left_unwritten_reads_to_the_end() {
        let mut bytes = wav(22050, 2, 16, &[5, 6, 7, 8], &[]);
        let at = bytes.len() - 8 - 4;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let (format, samples) = read(&bytes).unwrap();
        assert_eq!(format, Format::new(22050, 2));
        assert_eq!(samples, vec![5, 6, 7, 8]);
    }

    #[test]
    fn refuses_what_it_cannot_play() {
        assert!(read(b"RIFF\0\0\0\0AVI ").is_err());
        assert!(read(&wav(24000, 1, 8, &[0], &[])).is_err());
        assert!(read(&wav(96000, 1, 16, &[0], &[])).is_err());
        assert!(read(&wav(24000, 6, 16, &[0], &[])).is_err());
        let bytes = wav(24000, 1, 16, &[0], &[]);
        assert!(read(&bytes[..bytes.len() - 30]).is_err(), "no data chunk");
    }
}
