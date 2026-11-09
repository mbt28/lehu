// SPDX-License-Identifier: GPL-3.0-or-later
//! TLV8, the pairing messages' encoding: `[type][length][value]`, a value
//! over 255 bytes split into consecutive items of the same type and joined
//! again on decode.
//!
//! After LIVI's cp/stack/tlv8.ts and livi-cp/src/tlv8.rs (Lasse Heitgres,
//! GPL-3.0), through FastCarPlay's cp_tlv8.cpp; changed for LEHU.

/// Decoded items, type to value. A handful per message: a small vector
/// searched linearly beats a map here.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Items {
    items: Vec<(u8, Vec<u8>)>,
}

impl Items {
    pub fn get(&self, kind: u8) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.as_slice())
    }

    /// The first byte of a one-byte item (the pairing state), 0 when absent.
    pub fn byte(&self, kind: u8) -> u8 {
        self.get(kind).and_then(|v| v.first().copied()).unwrap_or(0)
    }
}

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: Option<u8> = None;
    for &(kind, value) in items {
        // Two adjacent items of one type would read as one fragmented value:
        // a zero-length separator of another type keeps them apart.
        if prev == Some(kind) {
            out.push(0xff);
            out.push(0);
        }
        let mut off = 0;
        loop {
            let len = (value.len() - off).min(255);
            out.push(kind);
            out.push(len as u8);
            out.extend_from_slice(&value[off..off + len]);
            off += len;
            if off >= value.len() {
                break;
            }
        }
        prev = Some(kind);
    }
    out
}

/// Decode; a truncated trailing item ends the scan (what came before it
/// stands, which is how the C++ and LIVI behave).
pub fn decode(buf: &[u8]) -> Items {
    let mut out = Items::default();
    let mut p = 0;
    let mut last: Option<(u8, usize)> = None;
    while p + 2 <= buf.len() {
        let kind = buf[p];
        let len = usize::from(buf[p + 1]);
        let Some(value) = buf.get(p + 2..p + 2 + len) else {
            break;
        };
        p += 2 + len;
        match last {
            // A continuation only follows a full 255-byte fragment.
            Some((k, 255)) if k == kind => {
                if let Some((_, v)) = out.items.iter_mut().rev().find(|(ik, _)| *ik == kind) {
                    v.extend_from_slice(value);
                }
            }
            _ => {
                if let Some(slot) = out.items.iter_mut().find(|(ik, _)| *ik == kind) {
                    slot.1 = value.to_vec();
                } else {
                    out.items.push((kind, value.to_vec()));
                }
            }
        }
        last = Some((kind, len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_values_and_joins_them() {
        let long = vec![7u8; 600];
        let enc = encode(&[(6, &[1]), (3, &long), (3, &[9])]);
        assert_eq!(enc.len(), 2 + 1 + (2 + 255) * 2 + (2 + 90) + 2 + 2 + 1);
        let d = decode(&enc);
        assert_eq!(d.byte(6), 1);
        assert_eq!(d.get(3), Some(&[9u8][..]));
        let enc = encode(&[(3, &long)]);
        assert_eq!(decode(&enc).get(3), Some(long.as_slice()));
    }

    #[test]
    fn truncated_item_ends_the_scan() {
        let d = decode(&[6, 1, 3, 4, 9, 1, 2]);
        assert_eq!(d.byte(6), 3);
        assert_eq!(d.get(4), None);
        assert_eq!(decode(&[]).byte(6), 0);
    }
}
