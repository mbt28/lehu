// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The media paths' own parsing: the NAL kinds of an access unit, the
//! Annex-B splitter that cuts a stream into units, the PCM the phones send,
//! the format word between the protocols and the mixer, and the album
//! art's pictures.

use lehu_audio::{Decoder, Format, Segment, decode_pcm_be, pcm_samples};
use lehu_session::{AudioCodec, AudioFormat};
use lehu_video::annexb::{self, Splitter};

use crate::{Rng, Target, bounded, ensure, fingerprint};

fn h264_stream() -> Vec<u8> {
    let mut s = Vec::new();
    for nal in [
        &[0x67, 0x42, 0x00, 0x1e, 0xab][..],
        &[0x68, 0xce, 0x38, 0x80],
        &[0x65, 0x88, 0x84, 0x21],
        &[0x41, 0x9a, 0x02],
        &[0x41, 0x9a, 0x04],
        &[0x09, 0xf0],
        &[0x06, 0x05, 0x01],
        &[0x41, 0x9a, 0x06],
    ] {
        s.extend_from_slice(&[0, 0, 0, 1]);
        s.extend_from_slice(nal);
    }
    s
}

fn hevc_stream() -> Vec<u8> {
    let mut s = Vec::new();
    for nal in [
        &[0x40, 0x01, 0x0c][..],
        &[0x42, 0x01, 0x01],
        &[0x44, 0x01, 0xc1],
        &[0x26, 0x01, 0xaf],
        &[0x02, 0x01, 0xd0],
        &[0x02, 0x01, 0x80],
    ] {
        s.extend_from_slice(&[0, 0, 1]);
        s.extend_from_slice(nal);
    }
    s
}

/// Every unit the splitter makes of `b`, fed in pieces `rng` picks.
fn split(b: &[u8], hevc: bool, rng: Option<&mut Rng>) -> Vec<Vec<u8>> {
    let mut sp = Splitter::new(hevc);
    let mut units = Vec::new();
    match rng {
        Some(rng) => {
            let mut at = 0;
            while at < b.len() {
                let n = (1 + rng.below(16)).min(b.len() - at);
                sp.push(&b[at..at + n]);
                at += n;
                while let Some(u) = sp.next_unit() {
                    units.push(u.data().to_vec());
                }
            }
        }
        None => sp.push(b),
    }
    sp.finish();
    while let Some(u) = sp.next_unit() {
        units.push(u.data().to_vec());
    }
    units
}

#[test]
fn annexb_units() {
    Target::new("annexb_units")
        .seed(h264_stream())
        .seed(hevc_stream())
        .dict(&["\0\0\x01", "\0\0\0\x01"])
        .max_len(4096)
        .run(|b| {
            for hevc in [false, true] {
                let _ = (annexb::kinds(b, hevc), annexb::has_picture(b, hevc));
                let whole = split(b, hevc, None);
                let total: usize = whole.iter().map(Vec::len).sum();
                bounded("access units", total, b.len(), 1, 0)?;
                // Torn pushes cut the stream at the same places.
                let mut rng = Rng::new(fingerprint(b));
                let torn = split(b, hevc, Some(&mut rng));
                ensure(torn == whole, || {
                    "torn pushes split the stream differently".into()
                })?;
            }
            Ok(())
        });
}

#[test]
fn pcm_payloads() {
    Target::new("pcm_payloads")
        .seed(vec![0x01, 0x02, 0xff, 0xfe, 0x80, 0x00, 0x7f, 0xff])
        .seed(vec![0; 640])
        .max_len(8192)
        // Zero channels: bugs::decode_pcm_be_zero_channels.
        .run(|b| {
            let shape = b.first().copied().unwrap_or(0);
            let channels = shape % 3;
            let upmix = shape & 2 != 0;
            let capacity = usize::from(b.get(1).copied().unwrap_or(0)) * 16;
            let mut seg = Segment::with_capacity(capacity);
            if let Ok(frames) = decode_pcm_be(b, Format::new(48000, channels), upmix, &mut seg) {
                // An odd sample of a stereo payload stays in the segment
                // as half a frame; the count is of whole frames.
                ensure(
                    frames == seg.len() / usize::from(seg.format.channels.max(1)),
                    || "frames and samples disagree".into(),
                )?;
                bounded("samples", seg.len(), b.len(), 1, 0)?;
            }
            seg.clear();
            if let Ok(n) = pcm_samples(b, false, upmix, &mut seg) {
                ensure(n <= capacity && n == seg.len(), || {
                    "samples past the segment".into()
                })?;
            }
            if let Ok(mut d) = Decoder::open(AudioFormat {
                codec: AudioCodec::Pcm,
                rate: 16000,
                channels,
            }) {
                seg.clear();
                if let Ok(frames) = d.decode(b, &mut seg) {
                    bounded("decoded frames", frames, b.len(), 1, 0)?;
                }
            }
            let _ = (seg.frames(), seg.duration_ms(), seg.is_silent(4));
            Ok(())
        });
}

#[test]
fn format_words() {
    let seeds = [
        Format::STEREO_48K.word(),
        Format::MONO_16K.word(),
        4,
        5,
        0x10000,
        u32::MAX,
    ]
    .map(|w| w.to_le_bytes().to_vec());
    Target::new("format_words")
        .seeds(seeds)
        .max_len(8)
        .run(|b| {
            let mut w = [0u8; 4];
            for (d, s) in w.iter_mut().zip(b) {
                *d = *s;
            }
            let word = u32::from_le_bytes(w);
            let f = Format::from_word(word);
            ensure(matches!(f.channels, 1 | 2), || {
                format!("{word:#x} gave {} channels", f.channels)
            })?;
            ensure(f.rate > 0 || word >> 4 == 0, || {
                format!("{word:#x} gave rate 0")
            })?;
            // A shape the word can carry comes back from it.
            let shape = Format::new(word >> 4, 1 + (b.len() % 2) as u8);
            if (0x1000..=(u32::MAX >> 4)).contains(&shape.rate) {
                ensure(Format::from_word(shape.word()) == shape, || {
                    format!("{shape:?} does not round trip its word")
                })?;
            }
            let _ = f.ms_of(usize::from(b.len() as u16));
            Ok(())
        });
}

/// The album art decoder's test pictures: every JPEG kind it reads
/// (baseline at three samplings, grey, progressive, restart markers) and
/// two PNGs.
fn pictures() -> Vec<Vec<u8>> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../lehu-image/fixtures");
    let mut out = Vec::new();
    for name in [
        "base420.jpg",
        "base422.jpg",
        "base444.jpg",
        "grey.jpg",
        "prog420.jpg",
        "proggrey.jpg",
        "restart.jpg",
        "rgba.png",
        "palette.png",
    ] {
        out.push(std::fs::read(format!("{dir}/{name}")).expect("a fixture"));
    }
    out
}

#[test]
fn album_art() {
    Target::new("album_art")
        .seeds(pictures())
        .dict_bytes(&[
            &[0xff, 0xd8],
            &[0xff, 0xc0],
            &[0xff, 0xc2],
            &[0xff, 0xc4],
            &[0xff, 0xda],
            &[0xff, 0xdb],
            &[0xff, 0xdd],
            &[0xff, 0xd0],
            &[0xff, 0xd9],
            &[0xff, 0x00],
            b"IHDR",
            b"PLTE",
            b"tRNS",
            b"IDAT",
            b"IEND",
        ])
        .max_len(8192)
        .cost(4)
        .run(|b| {
            for side in [1, 37, 160] {
                if let Ok(p) = lehu_image::decode(b, side, [16, 20, 24]) {
                    ensure(p.width.max(p.height) == side, || {
                        format!("{}x{} for a {side} box", p.width, p.height)
                    })?;
                    ensure(p.rgb.len() == (p.width * p.height * 3) as usize, || {
                        "pixels and size disagree".into()
                    })?;
                }
            }
            Ok(())
        });
}
