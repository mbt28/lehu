// SPDX-License-Identifier: GPL-3.0-or-later
//! Pictures decoded here against the same pictures decoded by two
//! established decoders (zune-jpeg, png; test-only): the fixtures were
//! written by PIL, the PNGs of every colour type by the png crate.

use super::*;

const BLACK: [u8; 3] = [0, 0, 0];

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// The reference decoder's RGB at full size.
fn reference_jpeg(data: &[u8]) -> (u32, u32, Vec<u8>) {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), opts);
    let rgb = d.decode().unwrap();
    let (w, h) = d.dimensions().unwrap();
    (w as u32, h as u32, rgb)
}

/// `rgb` box-filtered to the size `side` shows it at, as the decoder does.
fn shrink(w: u32, h: u32, rgb: &[u8], side: u32) -> Picture {
    let mut s = scale::Scaler::new(w, h, side);
    for row in rgb.chunks_exact(w as usize * 3) {
        s.push(row);
    }
    s.finish().unwrap()
}

/// Mean and largest difference of two pictures of one size.
fn diff(a: &Picture, b: &Picture) -> (f64, u8) {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let mut sum = 0u64;
    let mut max = 0u8;
    for (x, y) in a.rgb.iter().zip(&b.rgb) {
        let d = x.abs_diff(*y);
        sum += u64::from(d);
        max = max.max(d);
    }
    (sum as f64 / a.rgb.len() as f64, max)
}

#[test]
fn jpegs_at_full_size_match_the_reference() {
    for (name, mean_max) in [
        ("base444.jpg", 1.0),
        ("grey.jpg", 1.0),
        ("proggrey.jpg", 1.0),
        // Colour at half resolution: the reference interpolates it, this
        // decoder repeats it, which differs at the sharp edges.
        ("base420.jpg", 5.0),
        ("base422.jpg", 5.0),
        ("prog420.jpg", 5.0),
        ("restart.jpg", 5.0),
        ("large420.jpg", 3.0),
        ("largeprog.jpg", 3.0),
    ] {
        let data = fixture(name);
        let (w, h, rgb) = reference_jpeg(&data);
        let want = Picture {
            width: w,
            height: h,
            rgb,
        };
        let got = decode(&data, w.max(h), BLACK).unwrap();
        let (mean, max) = diff(&got, &want);
        assert!(
            mean < mean_max,
            "{name}: mean difference {mean:.2}, max {max}"
        );
    }
}

#[test]
fn jpegs_shrunk_by_the_dct_match_the_reference_shrunk() {
    for name in [
        "base420.jpg",
        "large420.jpg",
        "largeprog.jpg",
        "restart.jpg",
    ] {
        let data = fixture(name);
        let (w, h, rgb) = reference_jpeg(&data);
        // At 9 the DCT gives 8 x 8 means and the box takes a few: a
        // little blur on top.
        for (side, mean_max) in [(9, 8.0), (20, 2.5), (37, 2.5), (64, 3.0), (160, 3.0)] {
            let want = shrink(w, h, &rgb, side);
            let got = decode(&data, side, BLACK).unwrap();
            assert_eq!((got.width, got.height), fit(w, h, side), "{name} at {side}");
            let (mean, _) = diff(&got, &want);
            assert!(
                mean < mean_max,
                "{name} at {side}: mean difference {mean:.2}"
            );
        }
    }
}

#[test]
fn a_progressive_jpeg_decodes_as_its_baseline_twin() {
    // The same coefficients, sent in one scan or in ten.
    for (base, prog) in [
        ("base420.jpg", "prog420.jpg"),
        ("large420.jpg", "largeprog.jpg"),
        ("grey.jpg", "proggrey.jpg"),
    ] {
        for side in [16, 77, 160] {
            let a = decode(&fixture(base), side, BLACK).unwrap();
            let b = decode(&fixture(prog), side, BLACK).unwrap();
            if base == "grey.jpg" {
                // Written at another quality: only alike.
                assert!(diff(&a, &b).0 < 3.0);
            } else {
                assert_eq!(a, b, "{prog} at {side}");
            }
        }
    }
}

/// A picture with something in it, `channels` of `depth`-bit samples.
fn pattern(w: u32, h: u32, channels: usize, depth: u8) -> Vec<u8> {
    let max = (1u32 << depth) - 1;
    let mut samples = Vec::new();
    for y in 0..h {
        for x in 0..w {
            for c in 0..channels {
                let v = (x * 7 + y * 13 + c as u32 * 50 + (x * y) % 17) % (max + 1);
                samples.push(v);
            }
        }
    }
    // Packed rows, as the format stores them.
    let mut out = Vec::new();
    let per_row = w as usize * channels;
    for row in samples.chunks(per_row) {
        let mut acc = 0u32;
        let mut nbits = 0;
        for &s in row {
            if depth == 16 {
                out.extend_from_slice(&(s as u16).to_be_bytes());
                continue;
            }
            acc = (acc << depth) | s;
            nbits += u32::from(depth);
            while nbits >= 8 {
                nbits -= 8;
                out.push((acc >> nbits) as u8);
            }
        }
        if nbits > 0 {
            out.push((acc << (8 - nbits)) as u8);
        }
    }
    out
}

fn encode_png(
    w: u32,
    h: u32,
    colour: ::png::ColorType,
    depth: ::png::BitDepth,
    pixels: &[u8],
    palette: Option<(&[u8], &[u8])>,
) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = ::png::Encoder::new(&mut out, w, h);
        e.set_color(colour);
        e.set_depth(depth);
        if let Some((plte, trns)) = palette {
            e.set_palette(plte.to_vec());
            e.set_trns(trns.to_vec());
        }
        let mut wr = e.write_header().unwrap();
        wr.write_image_data(pixels).unwrap();
    }
    out
}

fn reference_png(data: &[u8], bg: [u8; 3]) -> Picture {
    let mut d = ::png::Decoder::new(std::io::Cursor::new(data));
    d.set_transformations(::png::Transformations::EXPAND | ::png::Transformations::STRIP_16);
    let mut r = d.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let (w, h) = (info.width, info.height);
    let channels = info.color_type.samples();
    let mut rgb = Vec::new();
    for px in buf[..info.buffer_size()].chunks_exact(channels) {
        let (c, a) = match channels {
            1 => ([px[0]; 3], 255),
            2 => ([px[0]; 3], px[1]),
            3 => ([px[0], px[1], px[2]], 255),
            _ => ([px[0], px[1], px[2]], px[3]),
        };
        for i in 0..3 {
            let v = (u16::from(c[i]) * u16::from(a) + u16::from(bg[i]) * u16::from(255 - a) + 127)
                / 255;
            rgb.push(v as u8);
        }
    }
    Picture {
        width: w,
        height: h,
        rgb,
    }
}

#[test]
fn pngs_of_every_kind_match_the_reference() {
    use ::png::{BitDepth as D, ColorType as C};
    let bg = [16, 20, 24];
    let plte: Vec<u8> = (0..=255u8).flat_map(|i| [i, 255 - i, i / 2]).collect();
    let trns: Vec<u8> = (0..=255u8).map(|i| i.wrapping_mul(3)).collect();
    for (w, h) in [(1, 1), (13, 7), (64, 40)] {
        for (colour, channels, depths) in [
            (C::Grayscale, 1, &[1u8, 2, 4, 8, 16][..]),
            (C::Rgb, 3, &[8, 16]),
            (C::Indexed, 1, &[1, 2, 4, 8]),
            (C::GrayscaleAlpha, 2, &[8, 16]),
            (C::Rgba, 4, &[8, 16]),
        ] {
            for &depth in depths {
                let pixels = pattern(w, h, channels, depth);
                let d = match depth {
                    1 => D::One,
                    2 => D::Two,
                    4 => D::Four,
                    8 => D::Eight,
                    _ => D::Sixteen,
                };
                let entries = 1usize << depth.min(8);
                let palette =
                    (colour == C::Indexed).then(|| (&plte[..entries * 3], &trns[..entries]));
                let data = encode_png(w, h, colour, d, &pixels, palette);
                let want = reference_png(&data, bg);
                let got = decode(&data, w.max(h), bg).unwrap();
                assert_eq!(got, want, "{colour:?} {depth}-bit {w}x{h}");
            }
        }
    }
}

#[test]
fn png_fixtures_and_shrinking() {
    for name in ["rgba.png", "palette.png"] {
        let data = fixture(name);
        let want = reference_png(&data, BLACK);
        assert_eq!(decode(&data, 40, BLACK).unwrap(), want, "{name}");
        let small = decode(&data, 16, BLACK).unwrap();
        assert_eq!(small, shrink(want.width, want.height, &want.rgb, 16));
    }
}

#[test]
fn a_small_picture_is_stretched_to_its_box_once() {
    let data = fixture("base444.jpg");
    let p = decode(&data, 160, BLACK).unwrap();
    assert_eq!((p.width, p.height), (160, 94));
    assert_eq!(p.rgb.len(), 160 * 94 * 3);
    // A flat picture stays flat when stretched.
    let flat = encode_png(
        3,
        2,
        ::png::ColorType::Rgb,
        ::png::BitDepth::Eight,
        &[9, 99, 199].repeat(6),
        None,
    );
    let p = decode(&flat, 10, BLACK).unwrap();
    assert_eq!((p.width, p.height), (10, 7));
    assert!(
        p.rgb
            .as_chunks::<3>()
            .0
            .iter()
            .all(|px| *px == [9, 99, 199])
    );
}

#[test]
fn the_box_fits_the_longest_side() {
    assert_eq!(fit(600, 600, 160), (160, 160));
    assert_eq!(fit(640, 480, 160), (160, 120));
    assert_eq!(fit(480, 640, 160), (120, 160));
    assert_eq!(fit(4096, 1, 160), (160, 1));
    assert_eq!(fit(1, 4096, 160), (1, 160));
    assert_eq!(fit(0, 0, 160), (0, 0));
}

#[test]
fn what_is_not_a_picture_or_too_much_of_one_is_refused() {
    assert_eq!(decode(b"", 160, BLACK), Err(Error::Format));
    assert_eq!(decode(b"GIF89a", 160, BLACK), Err(Error::Format));
    assert_eq!(
        decode(&vec![0xff; MAX_INPUT + 1], 160, BLACK),
        Err(Error::TooBig)
    );
    assert_eq!(
        decode(&fixture("base420.jpg"), 0, BLACK),
        Err(Error::TooBig)
    );
    // Every truncation of a picture fails cleanly or still decodes.
    for name in ["base420.jpg", "prog420.jpg", "restart.jpg", "rgba.png"] {
        let data = fixture(name);
        for n in 0..data.len() {
            let _ = decode(&data[..n], 64, BLACK);
        }
    }
    // A header asking for more than the bounds.
    let mut big = fixture("base420.jpg");
    let sof = big.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    big[sof + 5..sof + 9].copy_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    assert_eq!(decode(&big, 160, BLACK), Err(Error::TooBig));
    let wide = encode_png(
        1,
        1,
        ::png::ColorType::Rgb,
        ::png::BitDepth::Eight,
        &[1, 2, 3],
        None,
    );
    let mut wide = wide;
    wide[16..20].copy_from_slice(&5000u32.to_be_bytes());
    assert_eq!(decode(&wide, 160, BLACK), Err(Error::TooBig));
    // An interlaced PNG is not read.
    let mut laced = encode_png(
        1,
        1,
        ::png::ColorType::Rgb,
        ::png::BitDepth::Eight,
        &[1, 2, 3],
        None,
    );
    laced[28] = 1;
    assert_eq!(decode(&laced, 160, BLACK), Err(Error::Unsupported));
}
