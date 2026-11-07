//! The whole exchange against a phone played here: its own P-256 key and
//! a leaf for it, a chip that signs with another key, every message
//! sealed and opened the way the other side does.

use std::path::Path;

use openssl::bn::{BigNum, BigNumContext};
use openssl::ec::{EcKey, PointConversionForm};
use openssl::ecdsa::EcdsaSig;
use openssl::pkey::Private;

use super::cert::tests::leaf_with;
use super::keys::{self, Ephemeral, POINT};
use super::wire::{self, CLEAR, HEADER, Message, TUNNELED};
use super::*;
use crate::pairing::MfiSigner;

/// A 3.0 chip with a key of its own.
pub(crate) struct FakeChip {
    key: EcKey<Private>,
    major: u8,
}

impl FakeChip {
    pub(crate) fn new(major: u8) -> Self {
        let group = keys::group().unwrap();
        Self {
            key: EcKey::generate(&group).unwrap(),
            major,
        }
    }

    fn verifies(&self, digest: &[u8], sig: &[u8]) -> bool {
        let sig = EcdsaSig::from_private_components(
            BigNum::from_slice(&sig[..32]).unwrap(),
            BigNum::from_slice(&sig[32..]).unwrap(),
        )
        .unwrap();
        sig.verify(digest, &self.key).unwrap()
    }
}

/// r and s, each to 32 bytes.
fn raw(sig: &EcdsaSig) -> Vec<u8> {
    let mut out = sig.r().to_vec_padded(32).unwrap();
    out.extend(sig.s().to_vec_padded(32).unwrap());
    out
}

impl MfiSigner for FakeChip {
    fn certificate(&mut self) -> Option<Vec<u8>> {
        Some(vec![0x30; 609])
    }
    fn sign(&mut self, digest: &[u8]) -> Option<Vec<u8>> {
        assert_eq!(digest.len(), 32);
        Some(raw(&EcdsaSig::sign(digest, &self.key).unwrap()))
    }
    fn protocol_major(&self) -> u8 {
        self.major
    }
}

/// The phone's side.
pub(crate) struct Phone {
    key: EcKey<Private>,
    ephemeral: Ephemeral,
    nonce: u16,
    pub(crate) session_id: u16,
    head_unit_point: [u8; POINT],
    keys: Option<keys::Keys>,
    prefix: [u8; 16],
    leaf: Vec<u8>,
    hash: [u8; 32],
}

impl Phone {
    pub(crate) fn new() -> Self {
        let group = keys::group().unwrap();
        let key = EcKey::generate(&group).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let point: [u8; POINT] = key
            .public_key()
            .to_bytes(&group, PointConversionForm::COMPRESSED, &mut ctx)
            .unwrap()
            .try_into()
            .unwrap();
        let leaf = leaf_with(&point, 3, &[0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01]);
        let prefix = [0x5c; 16];
        let der = cert::unpack(&leaf).unwrap().der;
        Self {
            key,
            ephemeral: Ephemeral::generate().unwrap(),
            nonce: 0x1357,
            session_id: 0,
            head_unit_point: [0; POINT],
            keys: None,
            prefix,
            leaf,
            hash: sha256(&[&prefix[..], &der].concat()),
        }
    }

    pub(crate) fn setup() -> Vec<u8> {
        Message::new(CLEAR, 0x5100)
            .with(0, [2])
            .with(1, [3])
            .with(2, [1])
            .with(3, [1])
            .with(3, [2])
            .with(4, [7])
            .encode()
    }

    /// AuthStart from our answer to the setup; `tamper` spoils the
    /// signature.
    pub(crate) fn start(&mut self, setup_answer: &[u8], tamper: bool) -> Vec<u8> {
        let m = wire::decode(setup_answer).unwrap();
        assert_eq!(m.id, 0x5201);
        assert_eq!((m.u8(0), m.u8(1), m.u8(2)), (Some(3), Some(2), Some(0)));
        self.head_unit_point = m.exact::<POINT>(4).unwrap();
        self.session_id = self.nonce ^ m.u16(3).unwrap();
        let shared = self.ephemeral.shared(&self.head_unit_point).unwrap();
        let negotiation = [3u8, 1, 2, 7, 3, 2];
        let context = [
            &[keys::SIGMA][..],
            &self.head_unit_point,
            &self.ephemeral.public,
            &negotiation,
        ]
        .concat();
        let mut keys = keys::derive(&shared, &context).unwrap();
        let mac = keys::cmac(&keys.phone_mac, &self.hash).unwrap();
        let signed = keys::signed_data(&self.head_unit_point, &self.ephemeral.public, &mac);
        let mut sig = raw(&EcdsaSig::sign(&sha256(&signed), &self.key).unwrap());
        if tamper {
            sig[40] ^= 1;
        }
        let sealed = keys
            .start
            .seal(&[], &[&self.hash[..], &sig].concat())
            .unwrap();
        self.keys = Some(keys);
        Message::new(CLEAR, 0x5111)
            .with(0, self.nonce.to_be_bytes())
            .with(1, self.ephemeral.public)
            .with(2, sealed)
            .encode()
    }

    fn channel(&mut self, id: u16, ours: bool) -> &mut keys::Channel {
        let k = self.keys.as_mut().unwrap();
        match (id, ours) {
            (0x5212 | 0x5113, _) => &mut k.phone_cert,
            (0x5213 | 0x5112, _) => &mut k.our_cert,
            (0x5214, _) => &mut k.finish,
            (_, true) => &mut k.from_phone,
            (_, false) => &mut k.ours,
        }
    }

    pub(crate) fn seal(&mut self, msg: Message) -> Vec<u8> {
        let msg = Message {
            start: self.session_id,
            ..msg
        };
        let params = msg.encode_params();
        let header = msg.header(params.len() + keys::TAG);
        let sealed = self.channel(msg.id, true).seal(&header, &params).unwrap();
        [&header[..], &sealed].concat()
    }

    pub(crate) fn open(&mut self, body: &[u8]) -> Message {
        let (start, id) = wire::peek(body).unwrap();
        assert_eq!(
            start, self.session_id,
            "sealed under the session: {id:#06x}"
        );
        let plain = self
            .channel(id, false)
            .open(&body[..HEADER], &body[HEADER..])
            .unwrap();
        Message {
            start,
            id,
            params: wire::decode_params(&plain).unwrap(),
        }
    }

    pub(crate) fn certificate(&mut self) -> Vec<u8> {
        let msg = Message::new(0, 0x5113)
            .with(0, [6])
            .with(1, self.leaf.clone())
            .with(2, self.prefix);
        self.seal(msg)
    }

    pub(crate) fn tunnel_write(&mut self, data: &[u8]) -> Vec<u8> {
        let inner = Message::new(TUNNELED, 0x5500)
            .with(0, [1])
            .with(1, data.to_vec())
            .encode();
        self.seal(Message::new(0, 0x5164).with(0, inner))
    }

    pub(crate) fn tunnel_read(&mut self, answer: &[u8]) -> Vec<u8> {
        let m = self.open(answer);
        assert_eq!(m.id, 0x5264);
        let inner = wire::decode(m.param(0).unwrap()).unwrap();
        assert_eq!((inner.start, inner.id), (TUNNELED, 0x5501));
        inner.param(1).unwrap().to_vec()
    }

    /// The whole exchange up to both sides proven.
    pub(crate) fn prove(&mut self, session: &mut Session, chip: &mut FakeChip) {
        let mut none = |_: &[u8]| -> Result<Vec<u8>, String> { Err("no tunnel".into()) };
        let answer = session.handle(&Self::setup(), chip, &mut none).unwrap();
        let start = self.start(&answer, false);
        let want = session.handle(&start, chip, &mut none).unwrap();
        let want = self.open(&want);
        assert_eq!(want.id, 0x5212);
        assert_eq!(want.param(0), Some(&self.hash[..]));
        let finish = session
            .handle(&self.certificate(), chip, &mut none)
            .unwrap();
        let finish = self.open(&finish);
        assert_eq!(finish.id, 0x5214);
        let our_hash = finish.exact::<32>(0).unwrap();
        let sig = finish.param(1).unwrap();
        let k = self.keys.as_ref().unwrap();
        let mac = keys::cmac(&k.our_mac, &our_hash).unwrap();
        let signed = keys::signed_data(&self.head_unit_point, &self.ephemeral.public, &mac);
        assert!(chip.verifies(&sha256(&signed), sig), "the chip's signature");
        let ask = self.seal(Message::new(0, 0x5112).with(0, our_hash));
        let cert = self.open(&session.handle(&ask, chip, &mut none).unwrap());
        assert_eq!(cert.id, 0x5213);
        let (der, prefix) = (cert.param(1).unwrap(), cert.param(2).unwrap());
        assert_eq!(der, &[0x30; 609][..]);
        assert_eq!(sha256(&[prefix, der].concat()), our_hash);
        let done = self.seal(Message::new(0, 0x51FF));
        let done = self.open(&session.handle(&done, chip, &mut none).unwrap());
        assert_eq!(done.id, 0x52FF);
        assert!(session.proven());
    }
}

fn session() -> Session {
    Session::new(Path::new("/nonexistent"))
}

#[test]
fn a_phone_and_the_chip_prove_each_other_and_the_tunnel_carries_data() {
    let mut chip = FakeChip::new(3);
    let mut s = session();
    let mut phone = Phone::new();
    phone.prove(&mut s, &mut chip);

    let mut echo = |b: &[u8]| -> Result<Vec<u8>, String> { Ok([b"echo:", b].concat()) };
    // Without the phone's word the tunnel carries nothing.
    let refused = s
        .handle(&phone.tunnel_write(b"m1"), &mut chip, &mut echo)
        .unwrap();
    assert_eq!(wire::peek(&refused).unwrap().1, 0x5265);
    s.binds_pairing = true;
    let taken = s
        .handle(&phone.tunnel_write(b"m1"), &mut chip, &mut echo)
        .unwrap();
    assert_eq!(phone.open(&taken).id, 0x5266);
    let read = phone.seal(Message::new(0, 0x5163));
    let answer = s.handle(&read, &mut chip, &mut echo).unwrap();
    assert_eq!(phone.tunnel_read(&answer), b"echo:m1");
}

#[test]
fn a_bad_signature_a_stranger_and_a_spoiled_seal_are_refused() {
    let mut chip = FakeChip::new(3);
    let mut none = |_: &[u8]| -> Result<Vec<u8>, String> { Ok(Vec::new()) };

    // The phone's signature does not verify: failed, in the clear.
    let mut s = session();
    let mut phone = Phone::new();
    let answer = s.handle(&Phone::setup(), &mut chip, &mut none).unwrap();
    let start = phone.start(&answer, true);
    phone.open(&s.handle(&start, &mut chip, &mut none).unwrap());
    let out = s
        .handle(&phone.certificate(), &mut chip, &mut none)
        .unwrap();
    assert_eq!(wire::peek(&out), Some((CLEAR, 0x52FE)));
    assert!(!s.proven());

    // A sealed message that does not open: a reset.
    let mut s = session();
    let mut phone = Phone::new();
    let answer = s.handle(&Phone::setup(), &mut chip, &mut none).unwrap();
    let start = phone.start(&answer, false);
    phone.open(&s.handle(&start, &mut chip, &mut none).unwrap());
    let mut cert = phone.certificate();
    let last = cert.len() - 1;
    cert[last] ^= 1;
    let out = s.handle(&cert, &mut chip, &mut none).unwrap();
    assert_eq!(wire::peek(&out), Some((CLEAR, 0x52FD)));

    // Under a start word that is not the session's: failed.
    let mut other = Message::new(0x7777, 0x5113).encode();
    other[3] = other.len() as u8;
    assert_eq!(
        wire::peek(&s.handle(&other, &mut chip, &mut none).unwrap()),
        Some((CLEAR, 0x52FE))
    );

    // Out of order: failed.
    let mut s = session();
    let early = Message::new(CLEAR, 0x51FF).encode();
    assert_eq!(
        wire::peek(&s.handle(&early, &mut chip, &mut none).unwrap()),
        Some((CLEAR, 0x52FE))
    );

    // A setup without the respond kind, or from too old a version.
    let refuse = Message::new(CLEAR, 0x5100)
        .with(0, [2])
        .with(1, [3])
        .with(2, [1])
        .with(3, [1])
        .encode();
    assert_eq!(
        wire::peek(&session().handle(&refuse, &mut chip, &mut none).unwrap()),
        Some((CLEAR, 0x5202))
    );
    let old = Message::new(CLEAR, 0x5100)
        .with(0, [1])
        .with(1, [3])
        .with(2, [1])
        .with(3, [2])
        .encode();
    assert_eq!(
        wire::peek(&session().handle(&old, &mut chip, &mut none).unwrap()),
        Some((CLEAR, 0x5202))
    );

    // A 2.0C chip cannot do this at all.
    assert!(
        session()
            .handle(&Phone::setup(), &mut FakeChip::new(2), &mut none)
            .is_err()
    );

    // The phone gives up: the session is over.
    let mut s = session();
    assert!(
        s.handle(&Message::new(CLEAR, 0x51FE).encode(), &mut chip, &mut none)
            .is_err()
    );
    assert!(s.handle(&Phone::setup(), &mut chip, &mut none).is_err());
    assert!(session().handle(b"short", &mut chip, &mut none).is_err());
}

/// Computed with the reference implementation from these inputs.
#[test]
fn the_transcript_proofs_match_the_reference() {
    let (kt1, kt2) = transcript_proofs(&[0x22; 32], b"the pair-verify transcript").unwrap();
    assert_eq!(
        kt1.to_vec(),
        cert::tests::unhex("fb4a76c6827b550f7ce7ffbc7fe77ba5fbef7e68db902daec50eaf7a53d79791")
    );
    assert_eq!(
        kt2.to_vec(),
        cert::tests::unhex("ebdb12866522dc17a73005f301930d52343f99ae75e33ac06ee30108b23e50d8")
    );
}

#[test]
fn header_numbers_read_like_the_phone_writes_them() {
    assert_eq!(header_number("0x20"), Some(32));
    assert_eq!(header_number(" 0X20 "), Some(32));
    assert_eq!(header_number("040"), Some(32));
    assert_eq!(header_number("32"), Some(32));
    assert_eq!(header_number("0"), Some(0));
    assert_eq!(header_number("1"), Some(1));
    assert_eq!(header_number("0x"), None);
    assert_eq!(header_number("09"), None);
    assert_eq!(header_number(""), None);
}
