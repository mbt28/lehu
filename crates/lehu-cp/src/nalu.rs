// SPDX-License-Identifier: GPL-3.0-or-later
//! CarPlay's video framing to Annex-B. A screen frame is a chain of NAL
//! units each behind a 4-byte big-endian length; the codec configuration is
//! an `avcC` or `hvcC` record (sometimes inside an `avc1`/`hvc1` sample
//! entry, sometimes bare). The decoder wants `00 00 00 01` start codes.
//!
//! The configuration record detection (`looks_like_avcc`, the fourcc scan)
//! is LIVI's cp/stack/nalu.ts (Lasse Heitgres, GPL-3.0), through
//! FastCarPlay's cp_nalu.cpp; the in-place reframing is FastCarPlay's own.

const START: [u8; 4] = [0, 0, 0, 1];

/// Rewrite a frame's length prefixes into start codes in place: both are
/// four bytes, so nothing moves. The chain is checked whole before a byte
/// changes, because once a prefix is overwritten the lengths are gone.
/// False, and the buffer untouched, for a malformed chain.
pub fn frame_to_annexb_in_place(frame: &mut [u8]) -> bool {
    let len = frame.len();
    let mut off = 0;
    while off + 4 <= len {
        let n = u32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
            as usize;
        // Checked: on the board's 32-bit usize a length near 4 GiB wraps
        // the sum to a small number, the check passes and the walk never
        // advances.
        let Some(end) = off.checked_add(4).and_then(|o| o.checked_add(n)) else {
            return false;
        };
        if n == 0 || end > len {
            return false;
        }
        off = end;
    }
    if off != len {
        return false;
    }
    off = 0;
    while off + 4 <= len {
        let n = u32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
            as usize;
        frame[off..off + 4].copy_from_slice(&START);
        off += 4 + n;
    }
    true
}

fn be16(p: &[u8]) -> usize {
    usize::from(u16::from_be_bytes([p[0], p[1]]))
}

/// Append one NAL with a start code; false when it does not fit `a`.
fn push_nal(out: &mut Vec<u8>, a: &[u8], off: &mut usize) -> bool {
    let Some(len) = a.get(*off..*off + 2).map(be16) else {
        return false;
    };
    *off += 2;
    let Some(nal) = a.get(*off..*off + len) else {
        return false;
    };
    out.extend_from_slice(&START);
    out.extend_from_slice(nal);
    *off += len;
    true
}

/// The SPS and PPS of an `avcC` record, as Annex-B.
fn avcc_to_annexb(a: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // version, profile, compatibility, level, length size, then the counts.
    let Some(&num_sps) = a.get(5) else {
        return out;
    };
    let mut off = 6;
    for _ in 0..(num_sps & 0x1f) {
        if !push_nal(&mut out, a, &mut off) {
            return out;
        }
    }
    let Some(&num_pps) = a.get(off) else {
        return out;
    };
    off += 1;
    for _ in 0..num_pps {
        if !push_nal(&mut out, a, &mut off) {
            return out;
        }
    }
    out
}

/// The VPS, SPS and PPS of an `hvcC` record, as Annex-B.
fn hvcc_to_annexb(a: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // 22 bytes of profile and level fields precede the array count.
    let Some(&arrays) = a.get(22) else {
        return out;
    };
    let mut off = 23;
    for _ in 0..arrays {
        let Some(count) = a.get(off + 1..off + 3).map(be16) else {
            return out;
        };
        off += 3;
        for _ in 0..count {
            if !push_nal(&mut out, a, &mut off) {
                return out;
            }
        }
    }
    out
}

/// Whether `a` starts like a bare `avcC` record: one SPS whose NAL type
/// is 7.
fn looks_like_avcc(a: &[u8]) -> bool {
    if a.len() < 9 || a[5] & 0x1f < 1 {
        return false;
    }
    let sps_len = be16(&a[6..8]);
    8 + sps_len <= a.len() && a[8] & 0x1f == 7
}

/// The parameter sets of a VideoConfig payload as Annex-B, and whether the
/// stream is HEVC.
pub fn config_to_annexb(payload: &[u8]) -> (Vec<u8>, bool) {
    for i in 4..payload.len().saturating_sub(3) {
        match &payload[i..i + 4] {
            b"hvcC" => return (hvcc_to_annexb(&payload[i + 4..]), true),
            b"avcC" => return (avcc_to_annexb(&payload[i + 4..]), false),
            _ => {}
        }
    }
    if looks_like_avcc(payload) {
        (avcc_to_annexb(payload), false)
    } else {
        (hvcc_to_annexb(payload), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reframes_in_place_and_refuses_bad_chains() {
        let mut f = vec![0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        assert!(frame_to_annexb_in_place(&mut f));
        assert_eq!(f, [0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41]);
        let mut short = vec![0, 0, 0, 9, 1, 2];
        let before = short.clone();
        assert!(!frame_to_annexb_in_place(&mut short));
        assert_eq!(short, before);
        let mut zero = vec![0, 0, 0, 0];
        assert!(!frame_to_annexb_in_place(&mut zero));
        let mut tail = vec![0, 0, 0, 1, 0x41, 0xff];
        assert!(!frame_to_annexb_in_place(&mut tail));
        let mut empty: Vec<u8> = Vec::new();
        assert!(frame_to_annexb_in_place(&mut empty));
    }

    #[test]
    fn avcc_record_bare_and_wrapped() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x38, 0x80];
        let mut rec = vec![1, 0x42, 0, 0x1e, 0xff, 0xe1, 0, 4];
        rec.extend_from_slice(&sps);
        rec.extend_from_slice(&[1, 0, 4]);
        rec.extend_from_slice(&pps);
        let (out, hevc) = config_to_annexb(&rec);
        assert!(!hevc);
        let mut want = START.to_vec();
        want.extend_from_slice(&sps);
        want.extend_from_slice(&START);
        want.extend_from_slice(&pps);
        assert_eq!(out, want);
        let mut wrapped = vec![0u8; 12];
        wrapped.extend_from_slice(b"avcC");
        wrapped.extend_from_slice(&rec);
        assert_eq!(config_to_annexb(&wrapped), (want.clone(), false));
        // Truncated inside the PPS: what was whole is kept, nothing indexed past the end.
        let (out, _) = config_to_annexb(&rec[..rec.len() - 2]);
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn hvcc_record() {
        let mut rec = vec![0u8; 22];
        rec.push(2); // arrays
        rec.extend_from_slice(&[0xa0, 0, 1, 0, 2, 0x40, 0x01]); // VPS
        rec.extend_from_slice(&[0xa1, 0, 1, 0, 2, 0x42, 0x01]); // SPS
        let mut wrapped = vec![0u8; 8];
        wrapped.extend_from_slice(b"hvcC");
        wrapped.extend_from_slice(&rec);
        let (out, hevc) = config_to_annexb(&wrapped);
        assert!(hevc);
        assert_eq!(out, [0, 0, 0, 1, 0x40, 0x01, 0, 0, 0, 1, 0x42, 0x01]);
        let (out, hevc) = config_to_annexb(&[0u8; 10]);
        assert!(hevc && out.is_empty());
    }
}
