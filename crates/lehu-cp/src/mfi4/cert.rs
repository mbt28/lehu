//! The phone's certificates as MFi 4 carries them: compressed, every field
//! at a fixed width (the kind, the issuer's and subject's short names, the
//! validity times as digits, the compressed key, the leaf's property, the
//! authority key id, a plan id, the signature as a short DER sequence).
//! Unpacked here back into the X.509 DER the issuer signed, so the chain
//! can be checked against Apple's accessory root when the head unit has
//! it, and the leaf's key read to check the phone's signature.
//!
//! The reader takes every field through a bounds-checked `take`; nothing
//! is indexed.

use std::path::Path;

use openssl::bn::{BigNum, BigNumContext};
use openssl::ec::{EcKey, EcPoint, PointConversionForm};
use openssl::ecdsa::EcdsaSig;
use openssl::pkey::{PKey, Public};
use openssl::x509::X509;

use super::keys::{POINT, group};
use crate::crypto::{sha1, sha256};

/// SHA-256 of the DER of Apple's accessory root CA: a file of that name
/// is trusted only if it is that certificate.
const ROOT_SHA256: [u8; 32] = [
    0x77, 0x57, 0xb6, 0x1f, 0x5b, 0xe7, 0xfc, 0x25, 0x9c, 0xad, 0x89, 0xa5, 0x9c, 0x11, 0x74, 0xb9,
    0xa0, 0x62, 0x34, 0xcb, 0xcb, 0x68, 0xdc, 0xd2, 0x43, 0xde, 0x30, 0x4d, 0x86, 0x75, 0x3c, 0x00,
];
pub const ROOT_FILE: &str = "accessory-root-ca.pem";

const LEAVES: std::ops::RangeInclusive<u8> = 0xA0..=0xA3;
const INTERMEDIATES: std::ops::RangeInclusive<u8> = 0xA4..=0xA5;
const WITH_PLAN_ID: u8 = 0x01;
const WITH_ACCESSORY_DATA: u8 = 0x02;
/// The last byte of a leaf's property: the key is held by an Apple device.
const HELD_BY_APPLE_DEVICE: u8 = 0x03;

const NAME_LEN: usize = 8;
const TIME_LEN: usize = 14;
const PROPERTY_LEN: usize = 32;
const KEY_ID_LEN: usize = 20;
const PLAN_ID_LEN: usize = 36;
const FULL_POINT: usize = 65;

const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const OID_UNIT: &[u8] = &[0x55, 0x04, 0x0b];
const OID_ORGANIZATION: &[u8] = &[0x55, 0x04, 0x0a];
const OID_STATE: &[u8] = &[0x55, 0x04, 0x08];
const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
const OID_SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1d, 0x0e];
const OID_AUTHORITY_KEY_ID: &[u8] = &[0x55, 0x1d, 0x23];
const OID_APPLE_ACCESSORY: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 0x06, 0x47];
const ACCESSORY_PROPERTY: u8 = 1;
const PLAN_ID: u8 = 2;
const ACCESSORY_DATA: u8 = 3;

const ORGANIZATION: &str = "Apple Inc.";
const STATE: &str = "California";
const ROOT_PREFIX: &str = "Apple Accessory Root CA - ";
const AUTHORITY_PREFIX: &str = "Apple Accessory Host Attestation Authority - ";
const LEAF_UNIT: &str = "Apple Accessory Host Attestations";

/// One certificate back in DER, with what the checks need of it.
#[derive(Debug, Clone)]
pub struct Unpacked {
    pub der: Vec<u8>,
    pub leaf: bool,
    pub point: [u8; FULL_POINT],
    pub authority: [u8; KEY_ID_LEN],
    pub key_id: [u8; KEY_ID_LEN],
    pub property: Option<[u8; PROPERTY_LEN]>,
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let (head, rest) = self
            .0
            .split_at_checked(n)
            .ok_or("the certificate is cut short")?;
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "the certificate is cut short".into())
    }

    fn text(&mut self, n: usize) -> Result<&'a str, String> {
        std::str::from_utf8(self.take(n)?).map_err(|_| "a name that is no text".into())
    }

    /// A DER sequence with a short length, whole.
    fn sequence(&mut self) -> Result<&'a [u8], String> {
        match self.0 {
            [0x30, len, ..] if *len < 0x80 => self.take(2 + usize::from(*len)),
            _ => Err("expected a short DER sequence".into()),
        }
    }
}

pub fn unpack(compressed: &[u8]) -> Result<Unpacked, String> {
    let mut r = Reader(compressed);
    let [kind] = r.array::<1>()?;
    let leaf = LEAVES.contains(&kind);
    if !leaf && !INTERMEDIATES.contains(&kind) {
        return Err(format!("unknown certificate kind {kind:#04x}"));
    }
    let issuer = r.text(NAME_LEN)?;
    let not_before = r.text(TIME_LEN)?;
    let not_after = r.text(TIME_LEN)?;
    let subject = if leaf { None } else { Some(r.text(NAME_LEN)?) };
    let point = full_point(&r.array::<POINT>()?)?;
    let property = if leaf {
        Some(r.array::<PROPERTY_LEN>()?)
    } else {
        None
    };
    let authority = r.array::<KEY_ID_LEN>()?;
    let plan_id = if kind & WITH_PLAN_ID != 0 {
        Some(r.take(PLAN_ID_LEN)?)
    } else {
        None
    };
    let signature = r.sequence()?;
    let accessory_data = if leaf && kind & WITH_ACCESSORY_DATA != 0 {
        Some(r.sequence()?)
    } else {
        None
    };
    if !r.0.is_empty() {
        return Err(format!("{} bytes past the certificate", r.0.len()));
    }

    let key_id = sha1(&point);
    let (issuer_name, subject_name) = match subject {
        None => (
            name(&[(OID_COMMON_NAME, &format!("{AUTHORITY_PREFIX}{issuer}"))]),
            name(&[
                (OID_COMMON_NAME, &upper_hex(&sha256(&point))),
                (OID_UNIT, LEAF_UNIT),
            ]),
        ),
        Some(subject) => (
            name(&[(OID_COMMON_NAME, &format!("{ROOT_PREFIX}{issuer}"))]),
            name(&[(OID_COMMON_NAME, &format!("{AUTHORITY_PREFIX}{subject}"))]),
        ),
    };

    let mut extensions = Vec::new();
    if !leaf {
        extensions.push(extension(
            OID_BASIC_CONSTRAINTS,
            true,
            &der(0x30, &der(0x01, &[0xff])),
        ));
    }
    extensions.push(extension(
        OID_AUTHORITY_KEY_ID,
        false,
        &der(0x30, &der(0x80, &authority)),
    ));
    if !leaf {
        extensions.push(extension(OID_SUBJECT_KEY_ID, false, &der(0x04, &key_id)));
        extensions.push(extension(OID_KEY_USAGE, true, &der(0x03, &[0x01, 0x06])));
    }
    if let Some(property) = &property {
        extensions.push(extension(&apple(ACCESSORY_PROPERTY), true, property));
    }
    if let Some(plan_id) = plan_id {
        extensions.push(extension(&apple(PLAN_ID), false, plan_id));
    }
    if let Some(data) = accessory_data {
        extensions.push(extension(&apple(ACCESSORY_DATA), false, data));
    }

    let algorithm = der(0x30, &der(0x06, OID_ECDSA_SHA256));
    let key_info = der(
        0x30,
        &[
            der(
                0x30,
                &[der(0x06, OID_EC_PUBLIC_KEY), der(0x06, OID_P256)].concat(),
            ),
            bit_string(&point),
        ]
        .concat(),
    );
    let tbs = der(
        0x30,
        &[
            der(0xa0, &der(0x02, &[2])),
            der(0x02, &point[..KEY_ID_LEN]),
            algorithm.clone(),
            with_place(issuer_name),
            der(0x30, &[time(not_before)?, time(not_after)?].concat()),
            with_place(subject_name),
            key_info,
            der(0xa3, &der(0x30, &extensions.concat())),
        ]
        .concat(),
    );
    let der = der(0x30, &[tbs, algorithm, bit_string(signature)].concat());
    Ok(Unpacked {
        der,
        leaf,
        point,
        authority,
        key_id,
        property,
    })
}

fn full_point(compressed: &[u8; POINT]) -> Result<[u8; FULL_POINT], String> {
    let group = group()?;
    let mut ctx = BigNumContext::new().map_err(|e| e.to_string())?;
    let point = EcPoint::from_bytes(&group, compressed, &mut ctx).map_err(|e| e.to_string())?;
    point
        .to_bytes(&group, PointConversionForm::UNCOMPRESSED, &mut ctx)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "a point that is no 65 bytes".into())
}

/// A DER element; the lengths here stay far below 64 KiB (the whole
/// compressed certificate came in one parameter).
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let n = content.len();
    let mut out = Vec::with_capacity(n + 4);
    out.push(tag);
    match n {
        0..0x80 => out.push(n as u8),
        0x80..0x100 => out.extend([0x81, n as u8]),
        _ => out.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend_from_slice(content);
    out
}

fn bit_string(bytes: &[u8]) -> Vec<u8> {
    der(0x03, &[&[0][..], bytes].concat())
}

fn with_place(mut entries: Vec<Vec<u8>>) -> Vec<u8> {
    entries.push(rdn(OID_ORGANIZATION, ORGANIZATION));
    entries.push(rdn(OID_STATE, STATE));
    der(0x30, &entries.concat())
}

fn name(entries: &[(&[u8], &str)]) -> Vec<Vec<u8>> {
    entries.iter().map(|(oid, value)| rdn(oid, value)).collect()
}

fn rdn(oid: &[u8], value: &str) -> Vec<u8> {
    der(
        0x31,
        &der(
            0x30,
            &[der(0x06, oid), der(0x0c, value.as_bytes())].concat(),
        ),
    )
}

fn extension(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let critical = if critical {
        der(0x01, &[0xff])
    } else {
        Vec::new()
    };
    der(0x30, &[der(0x06, oid), critical, der(0x04, value)].concat())
}

fn apple(arc: u8) -> Vec<u8> {
    [OID_APPLE_ACCESSORY, &[arc]].concat()
}

/// UTCTime from 1950 to 2049, GeneralizedTime outside.
fn time(digits: &str) -> Result<Vec<u8>, String> {
    let bad = || format!("a validity time {digits:?}");
    if digits.len() != TIME_LEN || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let year: u32 = digits
        .get(..4)
        .and_then(|y| y.parse().ok())
        .ok_or_else(bad)?;
    let short = digits.get(2..).ok_or_else(bad)?;
    Ok(if (1950..2050).contains(&year) {
        der(0x17, format!("{short}Z").as_bytes())
    } else {
        der(0x18, format!("{digits}Z").as_bytes())
    })
}

fn upper_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

struct Issuer {
    key_id: [u8; KEY_ID_LEN],
    key: PKey<Public>,
}

/// The issuers the phone's chain is checked against: Apple's accessory
/// root when the head unit has it on disk, and each intermediate the
/// phone hands over and the root signed. Without the root the chain goes
/// unchecked (the leaf's own signature over the session still is).
pub struct Trust {
    issuers: Vec<Issuer>,
    rooted: bool,
}

impl Trust {
    pub fn new(dir: &Path) -> Self {
        match find_root(dir) {
            Some(root) => Self {
                issuers: vec![root],
                rooted: true,
            },
            None => Self {
                issuers: Vec::new(),
                rooted: false,
            },
        }
    }

    pub fn rooted(&self) -> bool {
        self.rooted
    }

    pub fn knows_issuer_of(&self, cert: &Unpacked) -> bool {
        self.issuers.iter().any(|i| i.key_id == cert.authority)
    }

    pub fn check(&self, cert: &Unpacked) -> Result<(), String> {
        if cert.leaf && cert.property.and_then(|p| p.last().copied()) != Some(HELD_BY_APPLE_DEVICE)
        {
            return Err("a leaf not held by an Apple device".into());
        }
        if !self.rooted {
            return Ok(());
        }
        let issuer = self
            .issuers
            .iter()
            .find(|i| i.key_id == cert.authority)
            .ok_or_else(|| format!("no known issuer {}", upper_hex(&cert.authority)))?;
        let x509 = X509::from_der(&cert.der).map_err(|e| e.to_string())?;
        if !x509.verify(&issuer.key).map_err(|e| e.to_string())? {
            return Err("the issuer did not sign it".into());
        }
        Ok(())
    }

    /// An intermediate the root signed becomes an issuer.
    pub fn admit(&mut self, cert: &Unpacked) -> Result<(), String> {
        self.check(cert)?;
        self.issuers.push(Issuer {
            key_id: cert.key_id,
            key: public_key(&cert.point)?,
        });
        Ok(())
    }
}

fn find_root(dir: &Path) -> Option<Issuer> {
    let file = dir.join(ROOT_FILE);
    let pem = std::fs::read(&file).ok()?;
    let root = X509::stack_from_pem(&pem)
        .ok()?
        .into_iter()
        .find(|c| c.to_der().is_ok_and(|d| sha256(&d) == ROOT_SHA256));
    let Some(root) = root else {
        log::warn!("carplay: {} is not Apple's accessory root", file.display());
        return None;
    };
    let key_id = root.subject_key_id()?.as_slice().try_into().ok()?;
    Some(Issuer {
        key_id,
        key: root.public_key().ok()?,
    })
}

fn public_key(point: &[u8; FULL_POINT]) -> Result<PKey<Public>, String> {
    PKey::from_ec_key(ec_key(point)?).map_err(|e| e.to_string())
}

fn ec_key(point: &[u8; FULL_POINT]) -> Result<EcKey<Public>, String> {
    let group = group()?;
    let mut ctx = BigNumContext::new().map_err(|e| e.to_string())?;
    let point = EcPoint::from_bytes(&group, point, &mut ctx).map_err(|e| e.to_string())?;
    EcKey::from_public_key(&group, &point).map_err(|e| e.to_string())
}

/// The phone's raw (r || s) ECDSA signature over SHA-256 of `data`, by
/// the leaf's key.
pub fn signed_by(cert: &Unpacked, data: &[u8], signature: &[u8; 64]) -> Result<bool, String> {
    let (r, s) = signature.split_at(32);
    let r = BigNum::from_slice(r).map_err(|e| e.to_string())?;
    let s = BigNum::from_slice(s).map_err(|e| e.to_string())?;
    let sig = EcdsaSig::from_private_components(r, s).map_err(|e| e.to_string())?;
    let key = ec_key(&cert.point)?;
    sig.verify(&sha256(data), &key).map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// P-256's generator, compressed: a valid key for the layout tests.
    const G: &str = "036b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296";

    pub(crate) fn leaf_with(point: &[u8; POINT], held: u8, signature: &[u8]) -> Vec<u8> {
        let mut leaf = vec![0xA0u8];
        leaf.extend(b"ABCDEFGH");
        leaf.extend(b"20250101000000");
        leaf.extend(b"20510101000000");
        leaf.extend(point);
        let mut property = [0x5au8; 32];
        property[31] = held;
        leaf.extend(property);
        leaf.extend([0x77u8; 20]);
        leaf.extend(signature);
        leaf
    }

    const SIG: [u8; 8] = [0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01];

    /// The DER the reference rebuilds from the same compressed leaf.
    #[test]
    fn a_leaf_unpacks_to_the_reference_der() {
        let g: [u8; POINT] = unhex(G).try_into().unwrap();
        let u = unpack(&leaf_with(&g, 3, &SIG)).unwrap();
        assert!(u.leaf);
        assert_eq!(
            u.der,
            unhex(
                "308202273082020ca0030201020214046b17d1f2e12c4247f8bce6e563a440f277037d300a06082a8648ce3d040302306a313e303c06035504030c354170706c65204163636573736f727920486f7374204174746573746174696f6e20417574686f72697479202d20414243444546474831133011060355040a0c0a4170706c6520496e632e3113301106035504080c0a43616c69666f726e69613020170d3235303130313030303030305a180f32303531303130313030303030305a3081a13149304706035504030c4036393842454136334443343441333434363633464631343239414541313038343244463237423642393931454632353836364232433643303243444343354245312a3028060355040b0c214170706c65204163636573736f727920486f7374204174746573746174696f6e7331133011060355040a0c0a4170706c6520496e632e3113301106035504080c0a43616c69666f726e69613059301306072a8648ce3d020106082a8648ce3d030107034200046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5a3563054301f0603551d2304183016801477777777777777777777777777777777777777773031060a2a864886f763640647010101ff04205a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a03300a06082a8648ce3d0403020309003006020101020101"
            )
        );
        assert!(X509::from_der(&u.der).is_ok(), "OpenSSL reads it");
        assert_eq!(u.authority, [0x77; 20]);
    }

    #[test]
    fn an_intermediate_with_a_plan_id_unpacks_to_the_reference_der() {
        let mut mid = vec![0xA5u8];
        mid.extend(b"ROOTNAME");
        mid.extend(b"20200101000000");
        mid.extend(b"20400101000000");
        mid.extend(b"SUBJECTN");
        mid.extend(unhex(G));
        mid.extend([0x44u8; 20]);
        mid.extend([0x88u8; 36]);
        mid.extend([0x30, 0x06, 0x02, 0x01, 0x02, 0x02, 0x01, 0x03]);
        let u = unpack(&mid).unwrap();
        assert!(!u.leaf);
        assert_eq!(
            u.der,
            unhex(
                "3082021d30820202a0030201020214046b17d1f2e12c4247f8bce6e563a440f277037d300a06082a8648ce3d0403023057312b302906035504030c224170706c65204163636573736f727920526f6f74204341202d20524f4f544e414d4531133011060355040a0c0a4170706c6520496e632e3113301106035504080c0a43616c69666f726e6961301e170d3230303130313030303030305a170d3430303130313030303030305a306a313e303c06035504030c354170706c65204163636573736f727920486f7374204174746573746174696f6e20417574686f72697479202d205355424a4543544e31133011060355040a0c0a4170706c6520496e632e3113301106035504080c0a43616c69666f726e69613059301306072a8648ce3d020106082a8648ce3d030107034200046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5a38198308195300f0603551d130101ff040530030101ff301f0603551d230418301680144444444444444444444444444444444444444444301d0603551d0e04160414b467a3999eb5efa671de0032f08a9c9e5efda0e1300e0603551d0f0101ff0404030201063032060a2a864886f763640647020424888888888888888888888888888888888888888888888888888888888888888888888888300a06082a8648ce3d0403020309003006020102020103"
            )
        );
        assert_eq!(
            u.key_id.to_vec(),
            unhex("b467a3999eb5efa671de0032f08a9c9e5efda0e1")
        );
    }

    #[test]
    fn a_damaged_certificate_is_refused() {
        let g: [u8; POINT] = unhex(G).try_into().unwrap();
        let leaf = leaf_with(&g, 3, &SIG);
        for cut in 0..leaf.len() {
            assert!(unpack(&leaf[..cut]).is_err(), "cut at {cut}");
        }
        assert!(unpack(&[leaf.as_slice(), &[0]].concat()).is_err());
        let mut kind = leaf.clone();
        kind[0] = 0xA7;
        assert!(unpack(&kind).is_err());
        let mut time = leaf.clone();
        time[10] = b'x';
        assert!(unpack(&time).is_err());
        let mut point = leaf.clone();
        point[37] = 0x07;
        assert!(unpack(&point).is_err(), "no such point prefix");
        let mut sig = leaf.clone();
        let at = sig.len() - 7;
        sig[at] = 0x80;
        assert!(unpack(&sig).is_err(), "a long DER length");
    }

    #[test]
    fn a_leaf_must_be_held_by_an_apple_device() {
        let g: [u8; POINT] = unhex(G).try_into().unwrap();
        let trust = Trust::new(Path::new("/nonexistent"));
        assert!(!trust.rooted());
        assert!(
            trust
                .check(&unpack(&leaf_with(&g, 3, &SIG)).unwrap())
                .is_ok()
        );
        assert!(
            trust
                .check(&unpack(&leaf_with(&g, 2, &SIG)).unwrap())
                .is_err()
        );
    }
}
