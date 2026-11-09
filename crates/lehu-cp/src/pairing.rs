// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! The accessory side of CarPlay's pairing: pair-setup (SRP-6a, then the
//! long-term Ed25519 keys exchanged under a key from the SRP session),
//! pair-verify (ephemeral X25519 and signatures with those long-term keys,
//! yielding the control channel's keys) and the MFiSAP `/auth-setup` (the
//! coprocessor signs the two public keys). The identity and the paired
//! phones persist under the state directory, so a phone pairs once.
//!
//! Every proof is compared in constant time; every TLV is read, never
//! indexed.
//!
//! After LIVI's cp/stack/pairSetup.ts, pairVerify.ts, authSetup.ts,
//! mfiSigner.ts, identity.ts and pairings.ts (Lasse Heitgres, GPL-3.0),
//! through FastCarPlay's cp_pair_setup.cpp, cp_pair_verify.cpp,
//! cp_auth_setup.cpp and cp_identity.cpp; merged and changed for LEHU.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use subtle::ConstantTimeEq;

use crate::crypto::{self, X25519};
use crate::srp;
use crate::tlv8;

const TLV_IDENTIFIER: u8 = 0x01;
const TLV_SALT: u8 = 0x02;
const TLV_PUBLIC_KEY: u8 = 0x03;
const TLV_PROOF: u8 = 0x04;
const TLV_ENCRYPTED: u8 = 0x05;
const TLV_STATE: u8 = 0x06;
const TLV_ERROR: u8 = 0x07;
const TLV_SIGNATURE: u8 = 0x0a;

const ERR_AUTHENTICATION: u8 = 2;
const SETUP_CODE: &str = "3939";

// ── The identity and the paired phones ────────────────────────────────────

/// The accessory's long-term Ed25519 key and its pairing id (the `pi` the
/// phone is handed in CarPlayStartSession), and the phones that paired with
/// it, each by its identifier with its long-term public key.
pub struct Store {
    dir: PathBuf,
    pub seed: [u8; 32],
    pub public: [u8; 32],
    pub pairing_id: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    // Byte pairs, not string slices: a non-ASCII character in a damaged
    // file would put a slice boundary inside it.
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|&[hi, lo]| Some(digit(hi)? << 4 | digit(lo)?))
        .collect()
}

fn uuid4() -> Option<String> {
    let mut r: [u8; 16] = crypto::random_array()?;
    r[6] = (r[6] & 0x0f) | 0x40;
    r[8] = (r[8] & 0x3f) | 0x80;
    let h = hex(&r);
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// A random UUID in upper case, the form lockdown ids take.
pub fn uuid_upper() -> Option<String> {
    uuid4().map(|u| u.to_uppercase())
}

impl Store {
    /// Load the identity from `dir/identity.txt`, making and saving one on
    /// the first run.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let file = dir.join("identity.txt");
        if let Ok(text) = fs::read_to_string(&file) {
            let (mut seed, mut public, mut pi) = (None, None, None);
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    match k {
                        "priv" => seed = unhex(v).and_then(|b| <[u8; 32]>::try_from(b).ok()),
                        "pub" => public = unhex(v).and_then(|b| <[u8; 32]>::try_from(b).ok()),
                        "pi" => pi = Some(v.to_string()),
                        _ => {}
                    }
                }
            }
            if let (Some(seed), Some(public), Some(pairing_id)) = (seed, public, pi) {
                if crypto::ed25519_public(&seed) == public {
                    return Ok(Self {
                        dir: dir.to_path_buf(),
                        seed,
                        public,
                        pairing_id,
                    });
                }
                log::warn!("carplay: the stored identity does not match its key, making a new one");
            }
        }
        let (seed, public) =
            crypto::ed25519_generate().ok_or_else(|| io::Error::other("no randomness"))?;
        let pairing_id = uuid4().ok_or_else(|| io::Error::other("no randomness"))?;
        fs::create_dir_all(dir)?;
        fs::write(
            &file,
            format!(
                "priv={}\npub={}\npi={pairing_id}\n",
                hex(&seed),
                hex(&public)
            ),
        )?;
        let _ = set_private(&file);
        log::info!("carplay: new accessory identity {pairing_id}");
        Ok(Self {
            dir: dir.to_path_buf(),
            seed,
            public,
            pairing_id,
        })
    }

    fn pairings_file(&self) -> PathBuf {
        self.dir.join("pairings.txt")
    }

    /// Remember a phone's long-term key, replacing an older one.
    pub fn save_pairing(&self, id: &str, ltpk: &[u8]) -> io::Result<()> {
        let mut lines: Vec<String> = fs::read_to_string(self.pairings_file())
            .unwrap_or_default()
            .lines()
            .filter(|l| l.split(' ').next() != Some(id))
            .map(str::to_string)
            .collect();
        lines.push(format!("{id} {}", hex(ltpk)));
        fs::create_dir_all(&self.dir)?;
        let file = self.pairings_file();
        fs::write(&file, lines.join("\n") + "\n")?;
        let _ = set_private(&file);
        Ok(())
    }

    /// Forget a phone's long-term key; true when there was one. The phone
    /// pairs again next time (it will show its Trust question).
    pub fn remove_pairing(&self, id: &str) -> io::Result<bool> {
        let text = fs::read_to_string(self.pairings_file()).unwrap_or_default();
        let kept: Vec<&str> = text
            .lines()
            .filter(|l| l.split(' ').next() != Some(id))
            .collect();
        let removed = kept.len() != text.lines().count();
        if removed {
            let file = self.pairings_file();
            fs::write(&file, kept.join("\n") + "\n")?;
            let _ = set_private(&file);
        }
        Ok(removed)
    }

    pub fn pairing(&self, id: &str) -> Option<[u8; 32]> {
        let text = fs::read_to_string(self.pairings_file()).ok()?;
        let line = text.lines().find(|l| l.split(' ').next() == Some(id))?;
        let (_, key) = line.split_once(' ')?;
        <[u8; 32]>::try_from(unhex(key)?).ok()
    }
}

fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn err(state: u8) -> Vec<u8> {
    tlv8::encode(&[(TLV_STATE, &[state]), (TLV_ERROR, &[ERR_AUTHENTICATION])])
}

// ── Pair-setup ────────────────────────────────────────────────────────────

/// M1 to M6. The phone's long-term key is stored at M5; our M6 carries ours.
#[derive(Default)]
pub struct PairSetup {
    srp: Option<srp::Server>,
    k: Option<[u8; 64]>,
    pub complete: bool,
}

impl PairSetup {
    /// One request body in, the response body out.
    pub fn handle(&mut self, body: &[u8], store: &Store) -> Vec<u8> {
        let tlv = tlv8::decode(body);
        match tlv.byte(TLV_STATE) {
            1 => self.m2(),
            3 => self.m4(&tlv),
            5 => self.m6(&tlv, store),
            s => err(s.wrapping_add(1)),
        }
    }

    fn m2(&mut self) -> Vec<u8> {
        let Some(srp) = srp::Server::start("Pair-Setup", SETUP_CODE) else {
            return err(2);
        };
        let out = tlv8::encode(&[
            (TLV_STATE, &[2]),
            (TLV_PUBLIC_KEY, srp.public()),
            (TLV_SALT, srp.salt()),
        ]);
        self.srp = Some(srp);
        out
    }

    fn m4(&mut self, tlv: &tlv8::Items) -> Vec<u8> {
        let (Some(srp), Some(a), Some(proof)) =
            (&self.srp, tlv.get(TLV_PUBLIC_KEY), tlv.get(TLV_PROOF))
        else {
            return err(4);
        };
        let Some((k, m2)) = srp.verify(a, proof) else {
            log::warn!("carplay: pair-setup proof rejected");
            return err(4);
        };
        self.k = Some(k);
        tlv8::encode(&[(TLV_STATE, &[4]), (TLV_PROOF, &m2)])
    }

    fn m6(&mut self, tlv: &tlv8::Items, store: &Store) -> Vec<u8> {
        let (Some(k), Some(sealed)) = (&self.k, tlv.get(TLV_ENCRYPTED)) else {
            return err(6);
        };
        let key = crypto::hkdf_sha512(k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let Some(plain) = crypto::chacha_open(&key, &crypto::nonce_label("PS-Msg05"), &[], sealed)
        else {
            return err(6);
        };
        let sub = tlv8::decode(&plain);
        let (Some(id), Some(ltpk), Some(sig)) = (
            sub.get(TLV_IDENTIFIER),
            sub.get(TLV_PUBLIC_KEY),
            sub.get(TLV_SIGNATURE),
        ) else {
            return err(6);
        };
        // The phone signed (its sign key || its id || its LTPK).
        let sign_key = crypto::hkdf_sha512(
            k,
            "Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
        );
        let signed = [&sign_key[..], id, ltpk].concat();
        if !crypto::ed25519_verify(ltpk, &signed, sig) {
            log::warn!("carplay: pair-setup controller signature invalid");
            return err(6);
        }
        let Ok(controller) = std::str::from_utf8(id) else {
            return err(6);
        };
        if let Err(e) = store.save_pairing(controller, ltpk) {
            log::error!("carplay: cannot save the pairing: {e}");
            return err(6);
        }
        // Ours: (our sign key || our id || our LTPK), signed with our key.
        let acc_sign_key = crypto::hkdf_sha512(
            k,
            "Pair-Setup-Accessory-Sign-Salt",
            "Pair-Setup-Accessory-Sign-Info",
        );
        let acc_id = store.pairing_id.as_bytes();
        let to_sign = [&acc_sign_key[..], acc_id, &store.public].concat();
        let acc_sig = crypto::ed25519_sign(&store.seed, &to_sign);
        let sub = tlv8::encode(&[
            (TLV_IDENTIFIER, acc_id),
            (TLV_PUBLIC_KEY, &store.public),
            (TLV_SIGNATURE, &acc_sig),
        ]);
        let sealed = crypto::chacha_seal(&key, &crypto::nonce_label("PS-Msg06"), &[], &sub);
        self.complete = true;
        log::info!("carplay: paired with {controller}");
        tlv8::encode(&[(TLV_STATE, &[6]), (TLV_ENCRYPTED, &sealed)])
    }
}

// ── Pair-verify ───────────────────────────────────────────────────────────

/// The control channel's keys. Named from the phone's side: we read with
/// its write key and write with its read key.
#[derive(Clone)]
pub struct ControlKeys {
    pub read: [u8; 32],
    pub write: [u8; 32],
}

#[derive(Default)]
pub struct PairVerify {
    eph: Option<X25519>,
    peer_eph: [u8; 32],
    shared: Option<[u8; 32]>,
    enc_key: [u8; 32],
    pub verified: bool,
    pub controller: String,
    /// The run's messages, the phone's and ours, for the MFi 4 binding,
    /// and whether that run completed.
    transcript: Vec<u8>,
    run_complete: bool,
}

/// A run is four short messages; more is not one.
const MAX_TRANSCRIPT: usize = 4096;

impl PairVerify {
    pub fn handle(&mut self, body: &[u8], store: &Store) -> Vec<u8> {
        let tlv = tlv8::decode(body);
        let state = tlv.byte(TLV_STATE);
        if state == 1 {
            self.transcript.clear();
            self.run_complete = false;
        }
        let out = match state {
            1 => self.m2(&tlv, store),
            3 => self.m4(&tlv, store),
            s => err(s.wrapping_add(1)),
        };
        // Each step that went through joins the transcript, the
        // request and then our answer.
        if tlv8::decode(&out).get(TLV_ERROR).is_none()
            && self.transcript.len() + body.len() + out.len() <= MAX_TRANSCRIPT
        {
            self.transcript.extend_from_slice(body);
            self.transcript.extend_from_slice(&out);
            self.run_complete = state == 3;
        }
        out
    }

    /// The proofs that bind this verify to an MFi 4 authentication: the
    /// phone's expected one and ours. Only after a completed run.
    pub fn transcript_proofs(&self) -> Option<([u8; 32], [u8; 32])> {
        let shared = self
            .shared
            .as_ref()
            .filter(|_| self.verified && self.run_complete)?;
        crate::mfi4::transcript_proofs(shared, &self.transcript)
    }

    /// The shared secret every stream key derives from.
    pub fn shared(&self) -> Option<&[u8; 32]> {
        self.shared.as_ref()
    }

    pub fn keys(&self) -> Option<ControlKeys> {
        let shared = self.shared.as_ref().filter(|_| self.verified)?;
        Some(ControlKeys {
            read: crypto::hkdf_sha512(shared, "Control-Salt", "Control-Write-Encryption-Key"),
            write: crypto::hkdf_sha512(shared, "Control-Salt", "Control-Read-Encryption-Key"),
        })
    }

    fn m2(&mut self, tlv: &tlv8::Items, store: &Store) -> Vec<u8> {
        let Some(peer) = tlv
            .get(TLV_PUBLIC_KEY)
            .and_then(|k| <[u8; 32]>::try_from(k).ok())
        else {
            return err(2);
        };
        let Some(eph) = X25519::generate() else {
            return err(2);
        };
        let Some(shared) = eph.shared(&peer) else {
            return err(2);
        };
        self.enc_key = crypto::hkdf_sha512(
            &shared,
            "Pair-Verify-Encrypt-Salt",
            "Pair-Verify-Encrypt-Info",
        );
        // Sign (our ephemeral || our id || their ephemeral) with our LTSK.
        let acc_id = store.pairing_id.as_bytes();
        let to_sign = [&eph.public[..], acc_id, &peer].concat();
        let sig = crypto::ed25519_sign(&store.seed, &to_sign);
        let sub = tlv8::encode(&[(TLV_IDENTIFIER, acc_id), (TLV_SIGNATURE, &sig)]);
        let sealed =
            crypto::chacha_seal(&self.enc_key, &crypto::nonce_label("PV-Msg02"), &[], &sub);
        let out = tlv8::encode(&[
            (TLV_STATE, &[2]),
            (TLV_PUBLIC_KEY, &eph.public),
            (TLV_ENCRYPTED, &sealed),
        ]);
        self.peer_eph = peer;
        self.shared = Some(shared);
        self.eph = Some(eph);
        out
    }

    fn m4(&mut self, tlv: &tlv8::Items, store: &Store) -> Vec<u8> {
        let (Some(eph), Some(_), Some(sealed)) = (&self.eph, &self.shared, tlv.get(TLV_ENCRYPTED))
        else {
            return err(4);
        };
        let Some(plain) =
            crypto::chacha_open(&self.enc_key, &crypto::nonce_label("PV-Msg03"), &[], sealed)
        else {
            return err(4);
        };
        let sub = tlv8::decode(&plain);
        let (Some(id), Some(sig)) = (sub.get(TLV_IDENTIFIER), sub.get(TLV_SIGNATURE)) else {
            return err(4);
        };
        let Ok(controller) = std::str::from_utf8(id) else {
            return err(4);
        };
        let Some(ltpk) = store.pairing(controller) else {
            log::warn!("carplay: pair-verify from an unknown phone {controller}");
            return err(4);
        };
        // The phone signed (its ephemeral || its id || our ephemeral).
        let signed = [&self.peer_eph[..], id, &eph.public].concat();
        if !crypto::ed25519_verify(&ltpk, &signed, sig) {
            log::warn!("carplay: pair-verify signature invalid");
            return err(4);
        }
        self.verified = true;
        self.controller = controller.to_string();
        log::info!("carplay: pair-verify done with {controller}");
        tlv8::encode(&[(TLV_STATE, &[4])])
    }
}

// ── MFiSAP /auth-setup ────────────────────────────────────────────────────

/// What signs for the head unit: the coprocessor, behind a trait so the
/// handshake knows nothing of i2c.
pub trait MfiSigner {
    fn certificate(&mut self) -> Option<Vec<u8>>;
    /// Sign a digest: SHA-256 (32 bytes) on a 3.0 chip, SHA-1 (20) on 2.0C.
    fn sign(&mut self, digest: &[u8]) -> Option<Vec<u8>>;
    fn protocol_major(&self) -> u8;
}

/// The request is `[version 1][32-byte X25519 public key]`; the response
/// `[our public key][cert length][cert][sig length][encrypted signature]`,
/// the signature over SHA(ours || theirs) under AES-128-CTR with a key and
/// iv from SHA-1 of labelled shared secret. `None` on a bad request or a
/// chip that does not answer.
pub fn auth_setup(request: &[u8], signer: &mut dyn MfiSigner) -> Option<Vec<u8>> {
    if request.len() != 33 || request[0] != 1 {
        return None;
    }
    let peer: [u8; 32] = request[1..].try_into().ok()?;
    let eph = X25519::generate()?;
    let shared = eph.shared(&peer)?;
    if bool::from(shared.ct_eq(&[0u8; 32])) {
        return None;
    }
    let key: [u8; 16] = crypto::sha1(&[b"AES-KEY", &shared[..]].concat())[..16]
        .try_into()
        .ok()?;
    let iv: [u8; 16] = crypto::sha1(&[b"AES-IV", &shared[..]].concat())[..16]
        .try_into()
        .ok()?;
    let both = [&eph.public[..], &peer].concat();
    let digest: Vec<u8> = if signer.protocol_major() == 2 {
        crypto::sha1(&both).to_vec()
    } else {
        crypto::sha256(&both).to_vec()
    };
    let cert = signer.certificate()?;
    let mut sig = signer.sign(&digest)?;
    crypto::aes128_ctr(&key, &iv, &mut sig);
    let mut out = Vec::with_capacity(32 + 8 + cert.len() + sig.len());
    out.extend_from_slice(&eph.public);
    out.extend_from_slice(&(cert.len() as u32).to_be_bytes());
    out.extend_from_slice(&cert);
    out.extend_from_slice(&(sig.len() as u32).to_be_bytes());
    out.extend_from_slice(&sig);
    Some(out)
}

/// The phone's side of the pairing, for the tests of this crate.
#[cfg(test)]
pub(crate) mod test_phone {
    use num_bigint::BigUint;

    use super::SETUP_CODE;
    use crate::crypto;

    /// The phone's side of SRP, for the test only.
    pub(crate) struct Client {
        a: BigUint,
        pub(crate) big_a: Vec<u8>,
    }

    const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

    fn pad(x: &BigUint) -> Vec<u8> {
        let b = x.to_bytes_be();
        let mut out = vec![0u8; 384 - b.len()];
        out.extend_from_slice(&b);
        out
    }

    impl Client {
        pub(crate) fn new() -> Self {
            let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
            let a = BigUint::from_bytes_be(&[0x33; 32]);
            let big_a = BigUint::from(5u32).modpow(&a, &n);
            Self {
                a,
                big_a: pad(&big_a),
            }
        }

        pub(crate) fn proof(&self, salt: &[u8], big_b: &[u8]) -> ([u8; 64], [u8; 64]) {
            let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
            let g = BigUint::from(5u32);
            let k = BigUint::from_bytes_be(&crypto::sha512(&[&n.to_bytes_be(), &pad(&g)]));
            let inner = crypto::sha512(&[b"Pair-Setup", b":", SETUP_CODE.as_bytes()]);
            let x = BigUint::from_bytes_be(&crypto::sha512(&[salt, &inner]));
            let u = BigUint::from_bytes_be(&crypto::sha512(&[&self.big_a, big_b]));
            let b = BigUint::from_bytes_be(big_b);
            // S = (B - k g^x) ^ (a + u x)
            let kgx = (k * g.modpow(&x, &n)) % &n;
            let base = (&b + &n - kgx) % &n;
            let s = base.modpow(&(&self.a + &u * &x), &n);
            let key = crypto::sha512(&[&s.to_bytes_be()]);
            let hn = crypto::sha512(&[&n.to_bytes_be()]);
            let hg = crypto::sha512(&[&g.to_bytes_be()]);
            let mut hx = [0u8; 64];
            for i in 0..64 {
                hx[i] = hn[i] ^ hg[i];
            }
            let hi = crypto::sha512(&[b"Pair-Setup"]);
            let m1 = crypto::sha512(&[&hx, &hi, salt, &self.big_a, big_b, &key]);
            (key, m1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_phone::Client;
    use super::*;

    #[test]
    fn a_pairing_can_be_forgotten() {
        let dir = std::env::temp_dir().join(format!("lehu-cp-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        store.save_pairing("phone-a", &[1; 32]).unwrap();
        store.save_pairing("phone-b", &[2; 32]).unwrap();
        assert!(store.remove_pairing("phone-a").unwrap());
        assert!(!store.remove_pairing("phone-a").unwrap(), "already gone");
        assert!(store.pairing("phone-a").is_none());
        assert_eq!(store.pairing("phone-b"), Some([2; 32]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lehu-cp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Store::open(&dir).expect("store"), dir)
    }

    #[test]
    fn pair_setup_then_verify_as_a_phone_would() {
        let (store, dir) = temp_store();
        let (phone_seed, phone_ltpk) = crypto::ed25519_generate().unwrap();
        let phone_id = "AA:BB:CC:DD:EE:FF";

        let mut setup = PairSetup::default();
        let m2 = tlv8::decode(&setup.handle(&tlv8::encode(&[(TLV_STATE, &[1])]), &store));
        assert_eq!(m2.byte(TLV_STATE), 2);
        let (salt, big_b) = (m2.get(TLV_SALT).unwrap(), m2.get(TLV_PUBLIC_KEY).unwrap());
        assert_eq!(big_b.len(), 384);
        let client = Client::new();
        let (key, m1) = client.proof(salt, big_b);
        let m4 = tlv8::decode(&setup.handle(
            &tlv8::encode(&[
                (TLV_STATE, &[3]),
                (TLV_PUBLIC_KEY, &client.big_a),
                (TLV_PROOF, &m1),
            ]),
            &store,
        ));
        assert_eq!(m4.byte(TLV_STATE), 4);
        assert_eq!(
            m4.get(TLV_PROOF).unwrap(),
            crypto::sha512(&[&client.big_a, &m1, &key])
        );
        let enc_key =
            crypto::hkdf_sha512(&key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let sign_key = crypto::hkdf_sha512(
            &key,
            "Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
        );
        let sig = crypto::ed25519_sign(
            &phone_seed,
            &[&sign_key[..], phone_id.as_bytes(), &phone_ltpk].concat(),
        );
        let sub = tlv8::encode(&[
            (TLV_IDENTIFIER, phone_id.as_bytes()),
            (TLV_PUBLIC_KEY, &phone_ltpk),
            (TLV_SIGNATURE, &sig),
        ]);
        let sealed = crypto::chacha_seal(&enc_key, &crypto::nonce_label("PS-Msg05"), &[], &sub);
        let m6 = tlv8::decode(&setup.handle(
            &tlv8::encode(&[(TLV_STATE, &[5]), (TLV_ENCRYPTED, &sealed)]),
            &store,
        ));
        assert_eq!(m6.byte(TLV_STATE), 6);
        assert!(setup.complete);
        let plain = crypto::chacha_open(
            &enc_key,
            &crypto::nonce_label("PS-Msg06"),
            &[],
            m6.get(TLV_ENCRYPTED).unwrap(),
        )
        .unwrap();
        let sub = tlv8::decode(&plain);
        assert_eq!(
            sub.get(TLV_IDENTIFIER).unwrap(),
            store.pairing_id.as_bytes()
        );
        assert_eq!(sub.get(TLV_PUBLIC_KEY).unwrap(), &store.public);
        assert_eq!(store.pairing(phone_id), Some(phone_ltpk));

        // Pair-verify with the stored key.
        let phone_eph = X25519::generate().unwrap();
        let mut verify = PairVerify::default();
        let m2 = tlv8::decode(&verify.handle(
            &tlv8::encode(&[(TLV_STATE, &[1]), (TLV_PUBLIC_KEY, &phone_eph.public)]),
            &store,
        ));
        assert_eq!(m2.byte(TLV_STATE), 2);
        let acc_eph: [u8; 32] = m2.get(TLV_PUBLIC_KEY).unwrap().try_into().unwrap();
        let shared = phone_eph.shared(&acc_eph).unwrap();
        let enc_key = crypto::hkdf_sha512(
            &shared,
            "Pair-Verify-Encrypt-Salt",
            "Pair-Verify-Encrypt-Info",
        );
        let plain = crypto::chacha_open(
            &enc_key,
            &crypto::nonce_label("PV-Msg02"),
            &[],
            m2.get(TLV_ENCRYPTED).unwrap(),
        )
        .unwrap();
        let sub = tlv8::decode(&plain);
        let signed = [&acc_eph[..], store.pairing_id.as_bytes(), &phone_eph.public].concat();
        assert!(crypto::ed25519_verify(
            &store.public,
            &signed,
            sub.get(TLV_SIGNATURE).unwrap()
        ));
        let sig = crypto::ed25519_sign(
            &phone_seed,
            &[&phone_eph.public[..], phone_id.as_bytes(), &acc_eph].concat(),
        );
        let sub = tlv8::encode(&[(TLV_IDENTIFIER, phone_id.as_bytes()), (TLV_SIGNATURE, &sig)]);
        let sealed = crypto::chacha_seal(&enc_key, &crypto::nonce_label("PV-Msg03"), &[], &sub);
        let m3 = tlv8::encode(&[(TLV_STATE, &[3]), (TLV_ENCRYPTED, &sealed)]);
        let m4_bytes = verify.handle(&m3, &store);
        let m4 = tlv8::decode(&m4_bytes);
        assert_eq!(m4.byte(TLV_STATE), 4);
        assert_eq!(m4.get(TLV_ERROR), None);
        assert!(verify.verified);
        // The transcript the MFi 4 binding proves: all four messages.
        let m1 = tlv8::encode(&[(TLV_STATE, &[1]), (TLV_PUBLIC_KEY, &phone_eph.public)]);
        let m2_bytes = tlv8::encode(&[
            (TLV_STATE, &[2]),
            (TLV_PUBLIC_KEY, &acc_eph),
            (TLV_ENCRYPTED, m2.get(TLV_ENCRYPTED).unwrap()),
        ]);
        let transcript = [&m1[..], &m2_bytes, &m3, &m4_bytes].concat();
        assert_eq!(
            verify.transcript_proofs(),
            crate::mfi4::transcript_proofs(&shared, &transcript)
        );
        assert_eq!(verify.controller, phone_id);
        assert_eq!(verify.shared(), Some(&shared));
        let keys = verify.keys().unwrap();
        assert_eq!(
            keys.read,
            crypto::hkdf_sha512(&shared, "Control-Salt", "Control-Write-Encryption-Key")
        );

        // A phone that never paired is refused at M3.
        let mut verify = PairVerify::default();
        verify.handle(
            &tlv8::encode(&[(TLV_STATE, &[1]), (TLV_PUBLIC_KEY, &phone_eph.public)]),
            &store,
        );
        let (other_seed, _) = crypto::ed25519_generate().unwrap();
        let sig = crypto::ed25519_sign(&other_seed, b"x");
        let sub = tlv8::encode(&[(TLV_IDENTIFIER, b"nobody"), (TLV_SIGNATURE, &sig)]);
        let sealed =
            crypto::chacha_seal(&verify.enc_key, &crypto::nonce_label("PV-Msg03"), &[], &sub);
        let m4 = tlv8::decode(&verify.handle(
            &tlv8::encode(&[(TLV_STATE, &[3]), (TLV_ENCRYPTED, &sealed)]),
            &store,
        ));
        assert_eq!(m4.byte(TLV_ERROR), ERR_AUTHENTICATION);
        assert!(!verify.verified);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_states_and_proofs_are_errors() {
        let (store, dir) = temp_store();
        let mut setup = PairSetup::default();
        let r = tlv8::decode(&setup.handle(&[], &store));
        assert_eq!(r.byte(TLV_ERROR), ERR_AUTHENTICATION);
        let r = tlv8::decode(&setup.handle(
            &tlv8::encode(&[
                (TLV_STATE, &[3]),
                (TLV_PUBLIC_KEY, &[1; 384]),
                (TLV_PROOF, &[2; 64]),
            ]),
            &store,
        ));
        assert_eq!(r.byte(TLV_STATE), 4);
        assert_eq!(r.byte(TLV_ERROR), ERR_AUTHENTICATION);
        let r = tlv8::decode(&setup.handle(
            &tlv8::encode(&[(TLV_STATE, &[5]), (TLV_ENCRYPTED, &[0; 40])]),
            &store,
        ));
        assert_eq!(r.byte(TLV_ERROR), ERR_AUTHENTICATION);
        let mut verify = PairVerify::default();
        let r = tlv8::decode(&verify.handle(
            &tlv8::encode(&[(TLV_STATE, &[1]), (TLV_PUBLIC_KEY, &[0; 32])]),
            &store,
        ));
        assert_eq!(r.byte(TLV_ERROR), ERR_AUTHENTICATION);
        let r = tlv8::decode(&verify.handle(
            &tlv8::encode(&[(TLV_STATE, &[1]), (TLV_PUBLIC_KEY, &[0; 31])]),
            &store,
        ));
        assert_eq!(r.byte(TLV_ERROR), ERR_AUTHENTICATION);
        let _ = fs::remove_dir_all(dir);
    }

    struct FakeChip;
    impl MfiSigner for FakeChip {
        fn certificate(&mut self) -> Option<Vec<u8>> {
            Some(vec![0x30, 0x82, 1, 2])
        }
        fn sign(&mut self, digest: &[u8]) -> Option<Vec<u8>> {
            assert_eq!(digest.len(), 32);
            Some(vec![9; 64])
        }
        fn protocol_major(&self) -> u8 {
            3
        }
    }

    #[test]
    fn auth_setup_layout() {
        let phone = X25519::generate().unwrap();
        let mut req = vec![1u8];
        req.extend_from_slice(&phone.public);
        let out = auth_setup(&req, &mut FakeChip).unwrap();
        let ours: [u8; 32] = out[..32].try_into().unwrap();
        assert_eq!(&out[32..36], &[0, 0, 0, 4]);
        assert_eq!(&out[36..40], &[0x30, 0x82, 1, 2]);
        assert_eq!(&out[40..44], &[0, 0, 0, 64]);
        let shared = phone.shared(&ours).unwrap();
        let key: [u8; 16] = crypto::sha1(&[b"AES-KEY", &shared[..]].concat())[..16]
            .try_into()
            .unwrap();
        let iv: [u8; 16] = crypto::sha1(&[b"AES-IV", &shared[..]].concat())[..16]
            .try_into()
            .unwrap();
        let mut sig = out[44..].to_vec();
        crypto::aes128_ctr(&key, &iv, &mut sig);
        assert_eq!(sig, vec![9; 64]);
        assert!(auth_setup(&req[..32], &mut FakeChip).is_none());
        req[0] = 2;
        assert!(auth_setup(&req, &mut FakeChip).is_none());
    }

    #[test]
    fn the_identity_persists() {
        let (a, dir) = temp_store();
        let b = Store::open(&dir).unwrap();
        assert_eq!(a.seed, b.seed);
        assert_eq!(a.pairing_id, b.pairing_id);
        assert_eq!(a.pairing_id.len(), 36);
        b.save_pairing("one", &[1; 32]).unwrap();
        b.save_pairing("two", &[2; 32]).unwrap();
        b.save_pairing("one", &[3; 32]).unwrap();
        assert_eq!(b.pairing("one"), Some([3; 32]));
        assert_eq!(b.pairing("two"), Some([2; 32]));
        assert_eq!(b.pairing("three"), None);
        let _ = fs::remove_dir_all(dir);
    }
}
