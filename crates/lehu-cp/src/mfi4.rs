// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! MFi 4 mutual authentication, as a phone that runs CarPlay Ultra asks
//! for it: a SIGMA-I exchange on `/auth-setup` (marked `x-apple-at: 0x20`)
//! in which the phone proves itself with its Apple-issued certificate and
//! the head unit with its MFi 3.0 coprocessor, both over ephemeral P-256
//! keys. The phone asks for it only when the CarPlayStartSession said the
//! head unit supports it.
//!
//! Once both are proven the session carries a tunnel: the pair-setup runs
//! through it (each TLV exchange a sealed write and a sealed read), and a
//! later `/auth-setup` binds the pair-verify to the authentication by
//! trading proofs keyed from the pair-verify secret over the transcript of
//! its messages.
//!
//! The steps, the phone's message on the left:
//!
//! ```text
//! 0x5100 setup (versions, chain kind, auth kinds)   -> 0x5201 our nonce, our point
//! 0x5111 start (its nonce, point, sealed hash+sig)  -> 0x5212 want its certificate
//! 0x5113 its certificate (leaf, maybe intermediate) -> 0x5214 our hash, our signature
//! 0x5112 wants our certificate                      -> 0x5213 the chip's certificate
//! 0x51ff succeeded                                  -> 0x52ff succeeded
//! 0x5164 tunnel write / 0x5163 tunnel read          -> 0x5266 taken / 0x5264 data
//! ```
//!
//! From the start on, the messages are sealed under the session id (the
//! two nonces xored) with AES-128-CCM, the header as associated data. A
//! message that does not open is answered with a reset in the clear.

pub mod cert;
pub mod keys;
pub mod wire;

use std::path::Path;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::crypto::{self, sha256};
use crate::pairing::MfiSigner;
use cert::{Trust, Unpacked};
use keys::{Channel, Ephemeral, Keys, POINT, TAG};
use wire::{CLEAR, HEADER, Message, TUNNELED};

/// The `x-apple-at` value that asks for this on `/auth-setup`.
pub const AUTH_TYPE: u32 = 0x20;

/// The two proofs that bind a pair-verify to the authentication: HMAC-
/// SHA256 over the pair-verify's messages, keyed from its secret; the
/// phone shows the first, we answer with the second.
pub fn transcript_proofs(shared: &[u8; 32], transcript: &[u8]) -> Option<([u8; 32], [u8; 32])> {
    let proof = |salt: &str, info: &str| -> Option<[u8; 32]> {
        let key = crypto::hkdf_sha512(shared, salt, info);
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).ok()?;
        mac.update(transcript);
        Some(mac.finalize().into_bytes().into())
    };
    Some((
        proof("MFi4-KT1-Salt", "MFi4-KT1-Info")?,
        proof("MFi4-KT2-Salt", "MFi4-KT2-Info")?,
    ))
}

/// A header value as the phone writes numbers: `0x` hex, a leading 0
/// octal, else decimal.
pub fn header_number(v: &str) -> Option<u32> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else if let Some(octal) = v.strip_prefix('0').filter(|o| !o.is_empty()) {
        u32::from_str_radix(octal, 8).ok()
    } else {
        v.parse().ok()
    }
}

mod id {
    pub const PHONE_SETUP: u16 = 0x5100;
    pub const PHONE_SETUP_FAILED: u16 = 0x5102;
    pub const PHONE_START: u16 = 0x5111;
    pub const PHONE_WANTS_CERT: u16 = 0x5112;
    pub const PHONE_CERT: u16 = 0x5113;
    pub const PHONE_TUNNEL_READ: u16 = 0x5163;
    pub const PHONE_TUNNEL_DATA: u16 = 0x5164;
    pub const PHONE_GAVE_UP: u16 = 0x51FA;
    pub const PHONE_FAILED: u16 = 0x51FE;
    pub const PHONE_SUCCEEDED: u16 = 0x51FF;

    pub const SETUP: u16 = 0x5201;
    pub const SETUP_FAILED: u16 = 0x5202;
    pub const WANT_CERT: u16 = 0x5212;
    pub const CERT: u16 = 0x5213;
    pub const FINISH: u16 = 0x5214;
    pub const TUNNEL_DATA: u16 = 0x5264;
    pub const TUNNEL_FAILED: u16 = 0x5265;
    pub const TUNNEL_TAKEN: u16 = 0x5266;
    pub const GAVE_UP: u16 = 0x52FA;
    pub const RESET: u16 = 0x52FD;
    pub const FAILED: u16 = 0x52FE;
    pub const SUCCEEDED: u16 = 0x52FF;

    pub const TUNNELED_FROM_PHONE: u16 = 0x5500;
    pub const TUNNELED_FROM_US: u16 = 0x5501;
}

const LOWEST_VERSION: u8 = 2;
const OUR_VERSION: u8 = 3;
/// The phone proves itself with a device certificate chain.
const PHONE_PROVES_AS_DEVICE: u8 = 1;
/// The authentication kind where the head unit responds.
const WE_RESPOND: u8 = 2;
const OUR_CHAIN_IS_ACCESSORY: u8 = 0;
const OUR_CERT_IS_3_0_LEAF: u8 = 0;
const PHONE_INTERMEDIATE: [u8; 2] = [4, 5];
const PHONE_LEAF: u8 = 6;
const MAX_AUTH_TYPES: usize = 4;
const MAX_CAPABILITIES: usize = 3;
const HASH: usize = 32;
const SIGNATURE: usize = 64;
const PREFIX: usize = 16;
const PAIRING_DATA: u8 = 1;
/// What one tunnel write may carry: a pair-setup message, a proof.
const MAX_TUNNEL: usize = 4096;

/// What the tunnel does with the bytes the phone writes into it: the
/// pair-setup, or the transcript binding. Its answer is what the phone
/// reads next.
pub type Tunnel<'a> = &'a mut dyn FnMut(&[u8]) -> Result<Vec<u8>, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Fresh,
    SetUp,
    AwaitingPhoneCert,
    Finished,
    Proven,
    Ended,
}

/// One connection's authentication.
pub struct Session {
    step: Step,
    our_nonce: u16,
    session_id: u16,
    ephemeral: Option<Ephemeral>,
    phone_point: [u8; POINT],
    version: u8,
    phone_auth_types: Vec<u8>,
    phone_capabilities: Vec<u8>,
    keys: Option<Keys>,
    phone_hash: [u8; HASH],
    phone_signature: [u8; SIGNATURE],
    our_prefix: [u8; PREFIX],
    our_cert: Vec<u8>,
    our_hash: [u8; HASH],
    trust: Trust,
    leaf_waiting: Option<Unpacked>,
    tunnel_out: Vec<u8>,
    /// The phone said (`x-apple-pairingtranscript`) the pairing is bound
    /// to this authentication: only then does the tunnel carry anything.
    pub binds_pairing: bool,
}

/// How a step failed: a malformed message is answered with "failed", an
/// operation of ours (the chip) with "gave up".
enum Fault {
    Malformed(String),
    Operation(String),
}

impl Session {
    /// `dir` may hold Apple's accessory root, to check the phone's chain.
    pub fn new(dir: &Path) -> Self {
        let trust = Trust::new(dir);
        if !trust.rooted() {
            log::info!(
                "carplay: mfi4: no Apple accessory root in {}, the phone's chain goes unchecked",
                dir.display()
            );
        }
        Self {
            step: Step::Fresh,
            our_nonce: 0,
            session_id: 0,
            ephemeral: None,
            phone_point: [0; POINT],
            version: 0,
            phone_auth_types: Vec::new(),
            phone_capabilities: Vec::new(),
            keys: None,
            phone_hash: [0; HASH],
            phone_signature: [0; SIGNATURE],
            our_prefix: [0; PREFIX],
            our_cert: Vec::new(),
            our_hash: [0; HASH],
            trust,
            leaf_waiting: None,
            tunnel_out: Vec::new(),
            binds_pairing: false,
        }
    }

    pub fn proven(&self) -> bool {
        self.step == Step::Proven
    }

    /// One message from the phone in, our answer out. `Err` ends the
    /// request with an error status (the phone gave up, or our chip is
    /// not a 3.0).
    pub fn handle(
        &mut self,
        body: &[u8],
        chip: &mut dyn MfiSigner,
        tunnel: Tunnel<'_>,
    ) -> Result<Vec<u8>, String> {
        let asked = wire::peek(body).map_or(0, |(_, id)| id);
        let answer = self.answer(body, chip, tunnel);
        match &answer {
            Ok(out) => {
                let to = wire::peek(out).map_or(0, |(_, id)| id);
                log::info!("carplay: mfi4 {asked:#06x} -> {to:#06x} ({:?})", self.step);
            }
            Err(e) => log::warn!("carplay: mfi4 {asked:#06x}: {e}"),
        }
        answer
    }

    fn answer(
        &mut self,
        body: &[u8],
        chip: &mut dyn MfiSigner,
        tunnel: Tunnel<'_>,
    ) -> Result<Vec<u8>, String> {
        if self.step == Step::Ended {
            return Err("the authentication on this connection is over".into());
        }
        let (start, msg_id) = wire::peek(body).ok_or("not one whole message")?;
        if [id::PHONE_SETUP_FAILED, id::PHONE_FAILED, id::PHONE_GAVE_UP].contains(&msg_id) {
            self.step = Step::Ended;
            return Err(format!("the phone gave up ({msg_id:#06x})"));
        }
        let msg = if start == CLEAR {
            wire::decode(body)?
        } else if self.session_id != 0 && start == self.session_id {
            match self.open(msg_id, body) {
                Some(msg) => msg,
                None => {
                    log::warn!("carplay: mfi4: {msg_id:#06x} does not open");
                    return Ok(clear(id::RESET));
                }
            }
        } else {
            log::warn!("carplay: mfi4: {msg_id:#06x} under start {start:#06x}");
            return Ok(clear(id::FAILED));
        };

        let answer = match (msg_id, self.step) {
            (id::PHONE_SETUP, Step::Fresh) => {
                if chip.protocol_major() != OUR_VERSION {
                    return Err("mutual authentication needs a 3.0 coprocessor".into());
                }
                return Ok(self.setup(&msg).unwrap_or_else(|e| {
                    log::warn!("carplay: mfi4: setup refused: {e}");
                    clear(id::SETUP_FAILED)
                }));
            }
            (id::PHONE_START, Step::SetUp) => self.start(&msg, chip),
            (id::PHONE_CERT, Step::AwaitingPhoneCert) => self.phone_cert(&msg, chip),
            (id::PHONE_WANTS_CERT, Step::Finished) => self.our_cert(&msg),
            (id::PHONE_SUCCEEDED, Step::Finished) => {
                log::info!("carplay: mfi4: the phone and the head unit proved each other");
                self.step = Step::Proven;
                Ok(Message::new(self.session_id, id::SUCCEEDED))
            }
            (id::PHONE_TUNNEL_DATA, Step::Proven) => return Ok(self.tunnel_in(&msg, tunnel)),
            (id::PHONE_TUNNEL_READ, Step::Proven) => {
                let out = std::mem::take(&mut self.tunnel_out);
                let inner = Message::new(TUNNELED, id::TUNNELED_FROM_US)
                    .with(0, [PAIRING_DATA])
                    .with(1, out)
                    .encode();
                Ok(Message::new(self.session_id, id::TUNNEL_DATA).with(0, inner))
            }
            (other, step) => Err(Fault::Malformed(format!("{other:#06x} while {step:?}"))),
        };
        Ok(
            match answer.and_then(|m| self.seal(&m).map_err(Fault::Operation)) {
                Ok(sealed) => sealed,
                Err(Fault::Malformed(e)) => {
                    log::warn!("carplay: mfi4: {e}");
                    clear(id::FAILED)
                }
                Err(Fault::Operation(e)) => {
                    log::warn!("carplay: mfi4: {e}");
                    clear(id::GAVE_UP)
                }
            },
        )
    }

    /// The phone's offer: the versions, the chain it proves with, the
    /// kinds of authentication and its capabilities. Answered with our
    /// version, our nonce and our ephemeral point.
    fn setup(&mut self, msg: &Message) -> Result<Vec<u8>, String> {
        let lowest = msg.u8(0).ok_or("no lowest version")?;
        let highest = msg.u8(1).ok_or("no highest version")?;
        if lowest < LOWEST_VERSION {
            return Err(format!("the phone starts at version {lowest}"));
        }
        if msg.u8(2) != Some(PHONE_PROVES_AS_DEVICE) {
            return Err(format!("certificate chain {:?}", msg.u8(2)));
        }
        let auth_types = single_bytes(msg, 3)?;
        let capabilities = single_bytes(msg, 4)?;
        if auth_types.is_empty()
            || auth_types.len() > MAX_AUTH_TYPES
            || capabilities.len() > MAX_CAPABILITIES
        {
            return Err(format!(
                "{} kinds, {} capabilities",
                auth_types.len(),
                capabilities.len()
            ));
        }
        if !auth_types.contains(&WE_RESPOND) {
            return Err(format!("the phone offers {auth_types:?}, we only respond"));
        }
        self.version = highest.min(OUR_VERSION);
        self.phone_auth_types = auth_types;
        self.phone_capabilities = capabilities;
        self.our_nonce = loop {
            let n = u16::from_be_bytes(crypto::random_array::<2>().ok_or("no randomness")?);
            if n != 0 && n != u16::MAX {
                break n;
            }
        };
        let ephemeral = Ephemeral::generate()?;
        let answer = Message::new(CLEAR, id::SETUP)
            .with(0, [self.version])
            .with(1, [WE_RESPOND])
            .with(2, [OUR_CHAIN_IS_ACCESSORY])
            .with(3, self.our_nonce.to_be_bytes())
            .with(4, ephemeral.public);
        self.ephemeral = Some(ephemeral);
        self.step = Step::SetUp;
        Ok(answer.encode())
    }

    /// The phone's nonce, point and its sealed certificate hash and
    /// signature: the keys derive, the seal must open. We ask for the
    /// certificate that hash names.
    fn start(&mut self, msg: &Message, chip: &mut dyn MfiSigner) -> Result<Message, Fault> {
        let malformed = |what: &str| Fault::Malformed(format!("AuthStart: {what}"));
        let nonce = msg.u16(0).ok_or_else(|| malformed("no nonce"))?;
        let point = msg
            .exact::<POINT>(1)
            .ok_or_else(|| malformed("no 33 byte key"))?;
        let sealed = msg
            .exact::<{ HASH + SIGNATURE + TAG }>(2)
            .ok_or_else(|| malformed("seal"))?;
        if nonce == 0 || nonce == u16::MAX {
            return Err(malformed("a reserved nonce"));
        }
        let session_id = nonce ^ self.our_nonce;
        if session_id == 0 {
            return Err(malformed("both nonces are the same"));
        }
        let ephemeral = self
            .ephemeral
            .as_ref()
            .ok_or_else(|| malformed("no point of ours"))?;
        let shared = ephemeral.shared(&point).map_err(Fault::Malformed)?;
        let negotiation = [
            &[OUR_VERSION][..],
            &self.phone_auth_types,
            &self.phone_capabilities,
            &[self.version, WE_RESPOND],
        ]
        .concat();
        let context = [&[keys::SIGMA][..], &ephemeral.public, &point, &negotiation].concat();
        let mut keys = keys::derive(&shared, &context).map_err(Fault::Operation)?;
        let opened = keys
            .start
            .open(&[], &sealed)
            .ok_or_else(|| malformed("the seal does not open"))?;
        let (Some(hash), Some(signature)) = (
            opened
                .get(..HASH)
                .and_then(|h| <[u8; HASH]>::try_from(h).ok()),
            opened
                .get(HASH..)
                .and_then(|s| <[u8; SIGNATURE]>::try_from(s).ok()),
        ) else {
            return Err(malformed("the seal holds no hash and signature"));
        };
        self.session_id = session_id;
        self.phone_hash = hash;
        self.phone_signature = signature;
        self.phone_point = point;
        self.keys = Some(keys);
        self.our_prefix = crypto::random_array().ok_or_else(|| malformed("no randomness"))?;
        self.our_cert = chip
            .certificate()
            .ok_or_else(|| Fault::Operation("the chip gave no certificate".into()))?;
        self.our_hash = sha256(&[&self.our_prefix[..], &self.our_cert].concat());
        self.step = Step::AwaitingPhoneCert;
        Ok(Message::new(self.session_id, id::WANT_CERT).with(0, self.phone_hash))
    }

    /// The phone's leaf (it must hash to what it signed for), or an
    /// intermediate we asked for. With the root at hand an unknown issuer
    /// is asked for first.
    fn phone_cert(&mut self, msg: &Message, chip: &mut dyn MfiSigner) -> Result<Message, Fault> {
        let malformed = |what: String| Fault::Malformed(format!("phone certificate: {what}"));
        let kind = msg.u8(0).ok_or_else(|| malformed("no kind".into()))?;
        let packed = msg.param(1).ok_or_else(|| malformed("none".into()))?;
        let ids = msg.ids();
        let unpacked = cert::unpack(packed).map_err(malformed)?;
        if kind == PHONE_LEAF {
            if ids != [0, 1, 2] {
                return Err(malformed(format!("leaf parameters {ids:?}")));
            }
            let prefix = msg.param(2).unwrap_or_default();
            let hash = sha256(&[prefix, &unpacked.der].concat());
            if !bool::from(hash.ct_eq(&self.phone_hash)) {
                return Err(malformed("it is not the one the phone signed for".into()));
            }
            if self.trust.rooted() && !self.trust.knows_issuer_of(&unpacked) {
                let issuer = unpacked.authority;
                self.leaf_waiting = Some(unpacked);
                return Ok(Message::new(self.session_id, id::WANT_CERT).with(1, issuer));
            }
            return self.finish(unpacked, chip);
        }
        if !PHONE_INTERMEDIATE.contains(&kind) || ids != [0, 1] {
            return Err(malformed(format!("kind {kind} with {ids:?}")));
        }
        self.trust.admit(&unpacked).map_err(malformed)?;
        let leaf = self
            .leaf_waiting
            .take()
            .ok_or_else(|| malformed("no leaf before".into()))?;
        self.finish(leaf, chip)
    }

    /// The phone's signature checked with its leaf's key; ours made by
    /// the chip over the same points and our MAC.
    fn finish(&mut self, leaf: Unpacked, chip: &mut dyn MfiSigner) -> Result<Message, Fault> {
        self.trust
            .check(&leaf)
            .map_err(|e| Fault::Malformed(format!("phone leaf: {e}")))?;
        let (Some(keys), Some(ephemeral)) = (self.keys.as_ref(), self.ephemeral.as_ref()) else {
            return Err(Fault::Malformed("finish before the start".into()));
        };
        let ours = ephemeral.public;
        let phone_mac = keys::cmac(&keys.phone_mac, &self.phone_hash).map_err(Fault::Operation)?;
        let theirs = keys::signed_data(&ours, &self.phone_point, &phone_mac);
        if !cert::signed_by(&leaf, &theirs, &self.phone_signature).map_err(Fault::Malformed)? {
            return Err(Fault::Malformed(
                "the phone's signature does not verify".into(),
            ));
        }
        let our_mac = keys::cmac(&keys.our_mac, &self.our_hash).map_err(Fault::Operation)?;
        let digest = sha256(&keys::signed_data(&ours, &self.phone_point, &our_mac));
        let signature = chip
            .sign(&digest)
            .ok_or_else(|| Fault::Operation("the chip did not sign".into()))?;
        if signature.len() != SIGNATURE {
            return Err(Fault::Operation(format!(
                "a {} byte signature",
                signature.len()
            )));
        }
        self.step = Step::Finished;
        Ok(Message::new(self.session_id, id::FINISH)
            .with(0, self.our_hash)
            .with(1, signature))
    }

    /// Our certificate, when the phone names it by the hash we sent.
    fn our_cert(&mut self, msg: &Message) -> Result<Message, Fault> {
        match (msg.param(0), msg.param(1)) {
            (Some(hash), None) if bool::from(hash.ct_eq(&self.our_hash)) => {
                Ok(Message::new(self.session_id, id::CERT)
                    .with(0, [OUR_CERT_IS_3_0_LEAF])
                    .with(1, self.our_cert.clone())
                    .with(2, self.our_prefix))
            }
            (Some(_), None) => Err(Fault::Operation("the phone asks for another hash".into())),
            (None, Some(_)) => Err(Fault::Operation(
                "the phone asks for an intermediate".into(),
            )),
            _ => Err(Fault::Malformed(
                "certificate request without hash or key id".into(),
            )),
        }
    }

    /// A tunneled write: the inner message's data to the tunnel, its
    /// answer kept for the read that follows.
    fn tunnel_in(&mut self, msg: &Message, tunnel: Tunnel<'_>) -> Vec<u8> {
        let result = (|| {
            if !self.binds_pairing {
                return Err("the phone did not say it binds the pairing".to_string());
            }
            let (_, outer) = msg.params.first().ok_or("an empty tunnel message")?;
            let inner = wire::decode(outer)?;
            if inner.start != TUNNELED
                || ![id::TUNNELED_FROM_PHONE, id::TUNNELED_FROM_US].contains(&inner.id)
            {
                return Err(format!(
                    "tunneled {:#06x} under {:#06x}",
                    inner.id, inner.start
                ));
            }
            let data = inner.param(1).ok_or("tunneled message without data")?;
            if data.len() > MAX_TUNNEL {
                return Err(format!("{} bytes through the tunnel", data.len()));
            }
            tunnel(data)
        })();
        let sealed = match result {
            Ok(out) => {
                self.tunnel_out = out;
                self.seal(&Message::new(self.session_id, id::TUNNEL_TAKEN))
            }
            Err(e) => {
                log::warn!("carplay: mfi4: tunnel: {e}");
                return Message::new(self.session_id, id::TUNNEL_FAILED).encode();
            }
        };
        sealed.unwrap_or_else(|e| {
            log::warn!("carplay: mfi4: {e}");
            clear(id::GAVE_UP)
        })
    }

    /// Which channel a message travels on: the certificate exchanges and
    /// the finish have their own, everything else ours or the phone's.
    fn channel(&mut self, msg_id: u16, ours: bool) -> Option<&mut Channel> {
        let keys = self.keys.as_mut()?;
        Some(match (msg_id, ours) {
            (id::WANT_CERT | id::PHONE_CERT, _) => &mut keys.phone_cert,
            (id::CERT | id::PHONE_WANTS_CERT, _) => &mut keys.our_cert,
            (id::FINISH, _) => &mut keys.finish,
            (_, true) => &mut keys.ours,
            (_, false) => &mut keys.from_phone,
        })
    }

    fn open(&mut self, msg_id: u16, body: &[u8]) -> Option<Message> {
        let (start, _) = wire::peek(body)?;
        let (header, sealed) = body.split_at_checked(HEADER)?;
        let plain = self.channel(msg_id, false)?.open(header, sealed)?;
        let params = wire::decode_params(&plain).ok()?;
        Some(Message {
            start,
            id: msg_id,
            params,
        })
    }

    fn seal(&mut self, msg: &Message) -> Result<Vec<u8>, String> {
        let params = msg.encode_params();
        let header = msg.header(params.len() + TAG);
        let channel = self.channel(msg.id, true).ok_or("no keys to seal with")?;
        Ok([&header[..], &channel.seal(&header, &params)?].concat())
    }
}

fn clear(msg_id: u16) -> Vec<u8> {
    Message::new(CLEAR, msg_id).encode()
}

/// A list of one-byte values (the parameter repeated).
fn single_bytes(msg: &Message, param: u16) -> Result<Vec<u8>, String> {
    msg.all(param)
        .map(|v| match v {
            [b] => Ok(*b),
            _ => Err(format!("parameter {param} is {} bytes", v.len())),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
