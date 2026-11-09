//! The CarPlay primitives, all pure Rust: X25519 and Ed25519 for the
//! pairing, HKDF-SHA512 for every derived key, ChaCha20-Poly1305 for every
//! sealed byte, AES-128-CTR and SHA-1 for the MFiSAP. The stream AEAD keeps
//! its key schedule and works in place: the receive paths decrypt where the
//! bytes landed and allocate nothing per packet.
//!
//! The set of primitives and the tests follow LIVI's cp/stack/crypto.ts
//! and livi-cp/src/crypto.rs (Lasse Heitgres, GPL-3.0); the bodies are
//! thin wrappers over the RustCrypto crates.

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

pub const TAG_LEN: usize = 16;

/// Fill `buf` from the kernel's random source. Only a broken system fails;
/// the keys made from it are then unusable, which the caller reports.
pub fn random(buf: &mut [u8]) -> bool {
    getrandom::fill(buf).is_ok()
}

pub fn random_array<const N: usize>() -> Option<[u8; N]> {
    let mut a = [0u8; N];
    random(&mut a).then_some(a)
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// HKDF-SHA512, 32 bytes out: every CarPlay key is derived this way.
pub fn hkdf_sha512(ikm: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    // A 32-byte request can never exceed HKDF's limit.
    let _ = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm).expand(info.as_bytes(), &mut out);
    out
}

// ── X25519 ────────────────────────────────────────────────────────────────

pub struct X25519 {
    secret: x25519_dalek::StaticSecret,
    pub public: [u8; 32],
}

impl X25519 {
    pub fn generate() -> Option<Self> {
        let bytes: [u8; 32] = random_array()?;
        let secret = x25519_dalek::StaticSecret::from(bytes);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        Some(Self { secret, public })
    }

    /// The shared secret with `peer`; `None` for a low-order or zero peer
    /// key, whose secret would be known to anyone.
    pub fn shared(&self, peer: &[u8; 32]) -> Option<[u8; 32]> {
        let shared = self
            .secret
            .diffie_hellman(&x25519_dalek::PublicKey::from(*peer));
        shared.was_contributory().then(|| shared.to_bytes())
    }
}

// ── Ed25519 ───────────────────────────────────────────────────────────────

pub fn ed25519_generate() -> Option<([u8; 32], [u8; 32])> {
    let seed: [u8; 32] = random_array()?;
    let key = SigningKey::from_bytes(&seed);
    Some((seed, key.verifying_key().to_bytes()))
}

pub fn ed25519_public(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

pub fn ed25519_sign(seed: &[u8; 32], data: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(seed).sign(data).to_bytes()
}

pub fn ed25519_verify(public: &[u8], data: &[u8], sig: &[u8]) -> bool {
    let (Ok(public), Ok(sig)) = (<[u8; 32]>::try_from(public), <[u8; 64]>::try_from(sig)) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&public) else {
        return false;
    };
    key.verify_strict(data, &Signature::from_bytes(&sig))
        .is_ok()
}

// ── AES-128-CTR (the MFiSAP signature) ────────────────────────────────────

pub fn aes128_ctr(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    let mut c = ctr::Ctr128BE::<aes::Aes128>::new(key.into(), iv.into());
    c.apply_keystream(data);
}

// ── ChaCha20-Poly1305 ─────────────────────────────────────────────────────

/// A 12-byte nonce from a counter: four zero bytes then the counter
/// little-endian, the shape every CarPlay stream uses.
pub fn nonce64(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

/// A nonce from a pairing-message label, right-aligned ("PS-Msg05").
pub fn nonce_label(label: &str) -> [u8; 12] {
    let mut n = [0u8; 12];
    let b = label.as_bytes();
    let len = b.len().min(8);
    n[12 - len..].copy_from_slice(&b[..len]);
    n
}

/// One stream's AEAD with its key scheduled once: a packet costs the cipher
/// and nothing else. The one-shot form paid a key schedule per packet, and
/// at a hundred packets a second each way that was most of the audio
/// thread's time on this core.
pub struct ChaChaStream {
    aead: ChaCha20Poly1305,
}

impl ChaChaStream {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(key.into()),
        }
    }

    /// Open `buf` = ciphertext || tag in place. On success the plaintext is
    /// `buf[..buf.len() - 16]`. On failure the buffer holds garbage: the
    /// callers drop the stream then anyway (an AEAD desync does not heal).
    pub fn open(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> bool {
        let Some(split) = buf.len().checked_sub(TAG_LEN) else {
            return false;
        };
        let (body, tag) = buf.split_at_mut(split);
        let tag = Tag::clone_from_slice(tag);
        self.aead
            .decrypt_in_place_detached(Nonce::from_slice(nonce), aad, body, &tag)
            .is_ok()
    }

    /// Open with the tag held apart from the ciphertext.
    pub fn open_detached(&self, nonce: &[u8; 12], aad: &[u8], body: &mut [u8], tag: &[u8]) -> bool {
        if tag.len() != TAG_LEN {
            return false;
        }
        let tag = Tag::clone_from_slice(tag);
        self.aead
            .decrypt_in_place_detached(Nonce::from_slice(nonce), aad, body, &tag)
            .is_ok()
    }

    /// Seal `body` in place; the tag goes to the caller.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], body: &mut [u8]) -> Option<[u8; TAG_LEN]> {
        self.aead
            .encrypt_in_place_detached(Nonce::from_slice(nonce), aad, body)
            .ok()
            .map(Into::into)
    }
}

/// Seal into a new buffer: ciphertext || tag. For the pairing messages,
/// which are few.
pub fn chacha_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut out = plain.to_vec();
    match ChaChaStream::new(key).seal(nonce, aad, &mut out) {
        Some(tag) => {
            out.extend_from_slice(&tag);
            out
        }
        None => Vec::new(),
    }
}

/// Open into a new buffer; `None` when the tag does not match.
pub fn chacha_open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    let mut buf = sealed.to_vec();
    if !ChaChaStream::new(key).open(nonce, aad, &mut buf) {
        return None;
    }
    buf.truncate(sealed.len() - TAG_LEN);
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_known_answer() {
        // RFC 5869 style check against a value computed with Python's hmac.
        let out = hkdf_sha512(b"secret", "salt", "info");
        assert_eq!(
            out,
            [
                0x90, 0xe2, 0x69, 0xf0, 0x53, 0xd3, 0x83, 0xc4, 0xb2, 0x07, 0x0b, 0xe9, 0x32, 0x38,
                0xad, 0xf3, 0x58, 0xf3, 0xd6, 0x7b, 0xd7, 0xb1, 0x7c, 0xa3, 0xde, 0x95, 0xf1, 0x0a,
                0x50, 0xa8, 0x38, 0x5e
            ]
        );
    }

    #[test]
    fn chacha_round_trip_in_place_and_detects_tampering() {
        let key = [7u8; 32];
        let s = ChaChaStream::new(&key);
        let mut buf = b"hello carplay".to_vec();
        let tag = s.seal(&nonce64(3), b"aad", &mut buf).unwrap();
        assert_ne!(&buf[..], b"hello carplay");
        buf.extend_from_slice(&tag);
        assert!(!s.open(&nonce64(4), b"aad", &mut buf.clone()));
        assert!(!s.open(&nonce64(3), b"bad", &mut buf.clone()));
        assert!(s.open(&nonce64(3), b"aad", &mut buf));
        assert_eq!(&buf[..13], b"hello carplay");
        assert!(!s.open(&nonce64(3), b"", &mut [1u8; 5]));
        let sealed = chacha_seal(&key, &nonce_label("PS-Msg05"), &[], b"x");
        assert_eq!(
            chacha_open(&key, &nonce_label("PS-Msg05"), &[], &sealed).unwrap(),
            b"x"
        );
        assert!(chacha_open(&key, &nonce_label("PS-Msg06"), &[], &sealed).is_none());
    }

    #[test]
    fn nonces() {
        assert_eq!(nonce64(0x0102), [0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&nonce_label("PV-Msg02")[4..], b"PV-Msg02");
        assert_eq!(
            nonce_label("ab"),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'a', b'b']
        );
    }

    #[test]
    fn keys_agree_and_signatures_verify() {
        let a = X25519::generate().unwrap();
        let b = X25519::generate().unwrap();
        assert_eq!(a.shared(&b.public), b.shared(&a.public));
        assert!(a.shared(&[0u8; 32]).is_none());
        let (seed, public) = ed25519_generate().unwrap();
        assert_eq!(ed25519_public(&seed), public);
        let sig = ed25519_sign(&seed, b"msg");
        assert!(ed25519_verify(&public, b"msg", &sig));
        assert!(!ed25519_verify(&public, b"msh", &sig));
        assert!(!ed25519_verify(&public[..31], b"msg", &sig));
    }

    #[test]
    fn aes_ctr_is_its_own_inverse() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let mut data = b"signature bytes".to_vec();
        aes128_ctr(&key, &iv, &mut data);
        assert_ne!(&data[..], b"signature bytes");
        aes128_ctr(&key, &iv, &mut data);
        assert_eq!(&data[..], b"signature bytes");
        assert_eq!(sha1(b"abc")[..4], [0xa9, 0x99, 0x3e, 0x36]);
        assert_eq!(sha256(b"abc")[..4], [0xba, 0x78, 0x16, 0xbf]);
    }
}
