// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! The MFi 4 SIGMA-I keys: ephemeral P-256 points (33-byte compressed),
//! their ECDH secret, a CMAC-based extract-and-expand into 200 bytes of key
//! material, cut into AES-128-CCM channels (a key and a 12-byte nonce that
//! counts up per message) and the two MAC keys the signatures bind. All on
//! OpenSSL's libcrypto, which the image carries for Android Auto.

use openssl::bn::BigNumContext;
use openssl::cipher::Cipher as Aead;
use openssl::cipher_ctx::CipherCtx;
use openssl::derive::Deriver;
use openssl::ec::{EcGroup, EcKey, EcPoint, PointConversionForm};
use openssl::error::ErrorStack;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::sign::Signer;
use openssl::symm::Cipher;

use crate::crypto::sha256;

pub const POINT: usize = 33;
pub const TAG: usize = 16;
const EXPANDED: usize = 200;
const EXTRACTION: &[u8] = b"MFi 4.0 SIGMA-I Authentication Randomness Extraction";
const EXPANSION: &[u8] = b"MFi 4.0 SIGMA-I Authentication Key Expansion";
pub const SIGMA: u8 = 0x01;

pub fn group() -> Result<EcGroup, String> {
    EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).map_err(|e| e.to_string())
}

/// Our ephemeral key for one authentication.
pub struct Ephemeral {
    key: EcKey<Private>,
    pub public: [u8; POINT],
}

impl Ephemeral {
    pub fn generate() -> Result<Self, String> {
        let group = group()?;
        let key = EcKey::generate(&group).map_err(|e| e.to_string())?;
        let mut ctx = BigNumContext::new().map_err(|e| e.to_string())?;
        let public = key
            .public_key()
            .to_bytes(&group, PointConversionForm::COMPRESSED, &mut ctx)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            key,
            public: public
                .try_into()
                .map_err(|_| "a point that is no 33 bytes")?,
        })
    }

    /// The ECDH secret with the phone's point, checked to be on the curve.
    pub fn shared(&self, peer: &[u8; POINT]) -> Result<Vec<u8>, String> {
        let group = group()?;
        let mut ctx = BigNumContext::new().map_err(|e| e.to_string())?;
        let point = EcPoint::from_bytes(&group, peer, &mut ctx).map_err(|e| e.to_string())?;
        let peer = EcKey::from_public_key(&group, &point).map_err(|e| e.to_string())?;
        peer.check_key().map_err(|e| e.to_string())?;
        let ours = PKey::from_ec_key(self.key.clone()).map_err(|e| e.to_string())?;
        let theirs = PKey::from_ec_key(peer).map_err(|e| e.to_string())?;
        let mut deriver = Deriver::new(&ours).map_err(|e| e.to_string())?;
        deriver.set_peer(&theirs).map_err(|e| e.to_string())?;
        deriver.derive_to_vec().map_err(|e| e.to_string())
    }
}

/// AES-CMAC (RFC 4493).
pub fn cmac(key: &[u8; 16], data: &[u8]) -> Result<[u8; 16], String> {
    let run = || -> Result<Vec<u8>, ErrorStack> {
        let pkey = PKey::cmac(&Cipher::aes_128_cbc(), key)?;
        let mut signer = Signer::new_without_digest(&pkey)?;
        signer.update(data)?;
        signer.sign_to_vec()
    };
    run()
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "a CMAC that is no AES block".into())
}

/// One direction's AES-128-CCM: the key and the nonce, which counts up
/// (big-endian) after each message.
pub struct Channel {
    key: [u8; 16],
    nonce: [u8; 12],
}

impl Channel {
    fn new(material: &[u8; EXPANDED], key_at: usize, nonce_at: usize) -> Self {
        let mut key = [0; 16];
        let mut nonce = [0; 12];
        key.copy_from_slice(&material[key_at..key_at + 16]);
        nonce.copy_from_slice(&material[nonce_at..nonce_at + 12]);
        Self { key, nonce }
    }

    fn step(&mut self) {
        for byte in self.nonce.iter_mut().rev() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
    }

    /// The plain bytes sealed, the 16-byte tag after them.
    pub fn seal(&mut self, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, String> {
        let out = ccm(&self.key, &self.nonce, aad, plain, None).map_err(|e| e.to_string())?;
        self.step();
        Ok(out)
    }

    /// `None` when the tag does not check out; the nonce then stays.
    pub fn open(&mut self, aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let (data, tag) = sealed.split_at_checked(sealed.len().checked_sub(TAG)?)?;
        let plain = ccm(&self.key, &self.nonce, aad, data, Some(tag)).ok()?;
        self.step();
        Some(plain)
    }
}

fn ccm(
    key: &[u8; 16],
    nonce: &[u8; 12],
    aad: &[u8],
    data: &[u8],
    tag: Option<&[u8]>,
) -> Result<Vec<u8>, ErrorStack> {
    let cipher = Aead::aes_128_ccm();
    let mut ctx = CipherCtx::new()?;
    match tag {
        None => ctx.encrypt_init(Some(cipher), None, None)?,
        Some(_) => ctx.decrypt_init(Some(cipher), None, None)?,
    }
    ctx.set_iv_length(nonce.len())?;
    match tag {
        None => ctx.set_tag_length(TAG)?,
        Some(tag) => ctx.set_tag(tag)?,
    }
    match tag {
        None => ctx.encrypt_init(None, Some(key), Some(nonce))?,
        Some(_) => ctx.decrypt_init(None, Some(key), Some(nonce))?,
    }
    // CCM takes the length first, then the associated data, then the
    // data in one piece; the tag is checked in that last update.
    ctx.set_data_len(data.len())?;
    if !aad.is_empty() {
        ctx.cipher_update(aad, None)?;
    }
    let mut out = vec![0; data.len() + cipher.block_size()];
    let mut n = ctx.cipher_update(data, Some(&mut out))?;
    if tag.is_none() {
        n += ctx.cipher_final(&mut out[n..])?;
        out.truncate(n);
        let mut tag = [0; TAG];
        ctx.tag(&mut tag)?;
        out.extend_from_slice(&tag);
    } else {
        out.truncate(n);
    }
    Ok(out)
}

/// Every channel and MAC key of one authentication. The phone's messages
/// and ours each have their own channel per step.
pub struct Keys {
    pub start: Channel,
    pub phone_cert: Channel,
    pub from_phone: Channel,
    pub finish: Channel,
    pub our_cert: Channel,
    pub ours: Channel,
    pub phone_mac: [u8; 16],
    pub our_mac: [u8; 16],
}

/// Extract with CMAC under a salt from the label, expand in CMAC counter
/// mode (block counter, label, 0, context, output bits) to 200 bytes.
pub fn derive(shared: &[u8], context: &[u8]) -> Result<Keys, String> {
    let mut salt = [0; 16];
    salt.copy_from_slice(&sha256(EXTRACTION)[..16]);
    let kdk = cmac(&salt, shared)?;
    let bits = ((EXPANDED * 8) as u32).to_be_bytes();
    let mut material = [0u8; EXPANDED];
    let mut input = Vec::with_capacity(4 + EXPANSION.len() + 1 + context.len() + 4);
    for (block, chunk) in (1u32..).zip(material.chunks_mut(16)) {
        input.clear();
        input.extend_from_slice(&block.to_be_bytes());
        input.extend_from_slice(EXPANSION);
        input.push(0);
        input.extend_from_slice(context);
        input.extend_from_slice(&bits);
        let out = cmac(&kdk, &input)?;
        chunk.copy_from_slice(&out[..chunk.len()]);
    }
    let mac = |at: usize| {
        let mut m = [0; 16];
        m.copy_from_slice(&material[at..at + 16]);
        m
    };
    Ok(Keys {
        start: Channel::new(&material, 0, 16),
        phone_mac: mac(28),
        phone_cert: Channel::new(&material, 44, 60),
        from_phone: Channel::new(&material, 72, 88),
        finish: Channel::new(&material, 100, 116),
        our_mac: mac(128),
        our_cert: Channel::new(&material, 144, 160),
        ours: Channel::new(&material, 172, 188),
    })
}

/// What a signature covers: the SIGMA tag, our point, the phone's point
/// (that order for both sides' signatures) and the signer's MAC.
pub fn signed_data(ours: &[u8; POINT], phone: &[u8; POINT], mac: &[u8; 16]) -> Vec<u8> {
    [&[SIGMA][..], ours, phone, mac].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn cmac_matches_the_rfc_4493_example() {
        let key: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c")
            .try_into()
            .unwrap();
        let msg = unhex("6bc1bee22e409f96e93d7e117393172a");
        assert_eq!(
            cmac(&key, &msg).unwrap().to_vec(),
            unhex("070a16b46b4d4144f79bdd9dd04a287c")
        );
        assert_eq!(
            cmac(&key, &[]).unwrap().to_vec(),
            unhex("bb1d6929e95937287fa37d129b756746")
        );
    }

    /// Computed with the reference implementation from these inputs.
    #[test]
    fn the_key_material_matches_the_reference() {
        let shared: Vec<u8> = (1u8..=32).collect();
        let mut k = derive(&shared, b"\x01context of the negotiation").unwrap();
        let seal = |c: &mut Channel, aad: &[u8], p: &[u8]| c.seal(aad, p).unwrap();
        assert_eq!(
            seal(&mut k.start, b"", b"start plain"),
            unhex("8c85c41e757ea7976df9c2e8a2fde19a219bf202eacd2c087f7dbf")
        );
        assert_eq!(
            seal(&mut k.phone_cert, b"hdr", b"pc"),
            unhex("0698757acaba5e08753471364fba84eb1a19")
        );
        assert_eq!(
            seal(&mut k.from_phone, b"hdr", b"fp"),
            unhex("0f31063060a747a6c63ed68a18c73047f3be")
        );
        assert_eq!(
            seal(&mut k.finish, b"hdr", b"fin"),
            unhex("cc81e883050807f1df0375793e0700c50ad76a")
        );
        assert_eq!(
            seal(&mut k.our_cert, b"hdr", b"oc"),
            unhex("a706d6ef6d80033f011b9fe00d147cefe862")
        );
        assert_eq!(
            seal(&mut k.ours, b"hdr", b"ours"),
            unhex("54feeecc91d434fec6294d32c906c23b38f9395b")
        );
        assert_eq!(
            seal(&mut k.ours, b"hdr", b"ours"),
            unhex("0362a967bec364d6ef48046b8aafb0bea84ffc17"),
            "the nonce counted on"
        );
        assert_eq!(
            k.phone_mac.to_vec(),
            unhex("cfbd5375b4427628e60f9cafe2abe960")
        );
        assert_eq!(
            k.our_mac.to_vec(),
            unhex("6aca78876b90e9416fd754e979d297f6")
        );
    }

    #[test]
    fn both_ends_agree_and_a_channel_counts_its_nonce() {
        let a = Ephemeral::generate().unwrap();
        let b = Ephemeral::generate().unwrap();
        let shared = a.shared(&b.public).unwrap();
        assert_eq!(shared, b.shared(&a.public).unwrap());
        assert_eq!(shared.len(), 32);

        let mut ours = derive(&shared, b"ctx").unwrap();
        let mut theirs = derive(&shared, b"ctx").unwrap();
        let first = ours.ours.seal(b"aad", b"one").unwrap();
        let second = ours.ours.seal(b"aad", b"one").unwrap();
        assert_ne!(first, second);
        assert_eq!(
            theirs.ours.open(b"aad", &first).as_deref(),
            Some(&b"one"[..])
        );
        assert!(theirs.ours.open(b"other", &second).is_none());
        assert_eq!(
            theirs.ours.open(b"aad", &second).as_deref(),
            Some(&b"one"[..])
        );
        let empty = ours.ours.seal(b"aad", b"").unwrap();
        assert_eq!(theirs.ours.open(b"aad", &empty).as_deref(), Some(&[][..]));
        assert!(
            theirs.ours.open(b"aad", &[0; 15]).is_none(),
            "shorter than a tag"
        );
        assert!(a.shared(&[0; POINT]).is_err());
        assert!(a.shared(&[0xff; POINT]).is_err());
    }

    #[test]
    fn the_nonce_carries_into_the_next_byte() {
        let mut ch = Channel {
            key: [0; 16],
            nonce: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff],
        };
        ch.step();
        assert_eq!(ch.nonce, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    }
}
