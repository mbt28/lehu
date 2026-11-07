//! The sealed framing of the control and event channels after pair-verify,
//! and of the iAP tunnel: `[2-byte length LE][ciphertext][16-byte tag]`,
//! the length being the associated data and the nonce a per-direction
//! counter. Separate keys each way.

use crate::crypto::{ChaChaStream, TAG_LEN, nonce64};

/// Frames carry at most this much; a message is split.
const MAX_PAYLOAD: usize = 0x4000;

pub struct FrameCipher {
    read: ChaChaStream,
    write: ChaChaStream,
    read_ctr: u64,
    write_ctr: u64,
}

impl FrameCipher {
    pub fn new(read_key: &[u8; 32], write_key: &[u8; 32]) -> Self {
        Self {
            read: ChaChaStream::new(read_key),
            write: ChaChaStream::new(write_key),
            read_ctr: 0,
            write_ctr: 0,
        }
    }

    /// Open every whole frame at the front of `sealed`, appending the
    /// plaintext to `plain`; the frames used are removed from `sealed`.
    /// False when a tag fails, which ends the channel: the counters can no
    /// longer agree.
    pub fn decrypt(&mut self, sealed: &mut Vec<u8>, plain: &mut Vec<u8>) -> bool {
        let mut off = 0;
        while sealed.len() - off >= 2 {
            let len = usize::from(u16::from_le_bytes([sealed[off], sealed[off + 1]]));
            let end = off + 2 + len + TAG_LEN;
            if sealed.len() < end {
                break;
            }
            let (head, rest) = sealed[off..end].split_at_mut(2);
            if !self.read.open(&nonce64(self.read_ctr), head, rest) {
                return false;
            }
            self.read_ctr += 1;
            plain.extend_from_slice(&rest[..len]);
            off = end;
        }
        sealed.drain(..off);
        true
    }

    /// Seal `plain` into frames, appended to `out`.
    pub fn encrypt_into(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        let mut chunks = plain.chunks(MAX_PAYLOAD);
        let mut next = chunks.next();
        if next.is_none() {
            next = Some(&[]);
        }
        while let Some(chunk) = next {
            let head = (chunk.len() as u16).to_le_bytes();
            out.extend_from_slice(&head);
            let start = out.len();
            out.extend_from_slice(chunk);
            match self
                .write
                .seal(&nonce64(self.write_ctr), &head, &mut out[start..])
            {
                Some(tag) => out.extend_from_slice(&tag),
                None => {
                    out.truncate(start - 2);
                    return;
                }
            }
            self.write_ctr += 1;
            next = chunks.next();
        }
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + 2 + TAG_LEN);
        self.encrypt_into(plain, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_both_ways_and_split() {
        let k1 = [1u8; 32];
        let k2 = [2u8; 32];
        let mut a = FrameCipher::new(&k1, &k2);
        let mut b = FrameCipher::new(&k2, &k1);
        let big = vec![0xabu8; MAX_PAYLOAD + 10];
        let mut sealed = a.encrypt(b"first");
        sealed.extend_from_slice(&a.encrypt(&big));
        assert_eq!(sealed.len(), 5 + 18 + big.len() + 2 * 18);
        let mut plain = Vec::new();
        let mut part = sealed[..30].to_vec();
        assert!(b.decrypt(&mut part, &mut plain));
        assert_eq!(plain, b"first");
        assert_eq!(part.len(), 30 - 23);
        part.extend_from_slice(&sealed[30..]);
        assert!(b.decrypt(&mut part, &mut plain));
        assert_eq!(&plain[5..], &big[..]);
        assert!(part.is_empty());
        // Replay: the counter moved on, so the same frame no longer opens.
        let mut again = a.encrypt(b"y");
        let copy = again.clone();
        assert!(b.decrypt(&mut again, &mut plain));
        assert!(!b.decrypt(&mut copy.clone(), &mut plain));
        let empty = a.encrypt(b"");
        assert_eq!(empty.len(), 18);
        // Tampering is fatal.
        let mut bad = a.encrypt(b"x");
        bad[3] ^= 1;
        assert!(!b.decrypt(&mut bad, &mut plain));
    }
}
