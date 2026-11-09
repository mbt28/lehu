// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! lockdownd, the phone's service broker, over a mux connection: pair the
//! head unit with the phone the first time (the driver taps Trust), start
//! a session under TLS, and start `com.apple.carkit.service`, the iAP2
//! channel wired CarPlay identifies over. Length-prefixed XML plists.
//!
//! The pair record (the certificates and keys the phone trusts us by) is
//! kept under LEHU's state directory, and read from the system's
//! `/var/lib/lockdown` too, so a record made with `idevicepair` works.
//! TLS and the certificates come from OpenSSL, already on the image for
//! Android Auto: the lockdown TLS needs an RSA client certificate and the
//! pairing needs RSA key generation and X.509 signing, which the pure-Rust
//! TLS stacks either do not build for this core or would make a first
//! pairing take minutes on it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use openssl::asn1::{Asn1Integer, Asn1Time};
use openssl::bn::BigNum;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::ssl::{
    HandshakeError, Ssl, SslContext, SslMethod, SslOptions, SslStream, SslVerifyMode, SslVersion,
};
use openssl::x509::extension::{BasicConstraints, KeyUsage, SubjectKeyIdentifier};
use openssl::x509::{X509, X509Builder};

use crate::plist::{self, Value};
use crate::usbmux::{MuxHost, MuxStream};

pub const LOCKDOWN_PORT: u16 = 62078;
pub const CARKIT_SERVICE: &str = "com.apple.carkit.service";
const LABEL: &str = "lehu";
const PROTOCOL_VERSION: &str = "2";
const SYSTEM_STORE: &str = "/var/lib/lockdown";
/// A lockdown message longer than this is not one.
const MAX_MESSAGE: usize = 4 << 20;
/// How long one lockdown exchange may take: the phone answers at once, or
/// not at all when it is locked and the record stale. A session's stop
/// waits at most this long for the trigger thread.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The phone's answer was not a message, or not the one asked for.
    Protocol(String),
    /// The phone refused: `Error` in its answer, by name.
    Refused(String),
    /// The pairing dialog is on the phone's screen; try again later.
    TrustPending,
    /// The driver declined the pairing on the phone.
    UserDenied,
    /// The phone is locked; pairing needs it unlocked.
    PasswordProtected,
    /// The stored record is unknown to the phone.
    InvalidHostId,
    Tls(String),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<openssl::error::ErrorStack> for Error {
    fn from(e: openssl::error::ErrorStack) -> Self {
        Error::Tls(e.to_string())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(s) => write!(f, "lockdown protocol: {s}"),
            Error::Refused(s) => write!(f, "lockdown refused: {s}"),
            Error::TrustPending => f.write_str("the phone is asking whether to trust this car"),
            Error::UserDenied => f.write_str("the phone declined the pairing"),
            Error::PasswordProtected => f.write_str("the phone is locked; unlock it to pair"),
            Error::InvalidHostId => f.write_str("the phone does not know our pairing"),
            Error::Tls(s) => write!(f, "tls: {s}"),
        }
    }
}

/// A connection that may have become TLS.
pub enum Stream {
    Plain(MuxStream),
    Tls(Box<SslStream<MuxStream>>),
}

impl Stream {
    pub fn set_read_timeout(&mut self, t: Duration) {
        match self {
            Stream::Plain(s) => s.set_read_timeout(t),
            Stream::Tls(s) => s.get_mut().set_read_timeout(t),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

// ── The pair record store ─────────────────────────────────────────────────

/// The UDID as the records are named: the 24-character serial with a dash
/// after the eighth character; other serials as they are.
pub fn dashed(serial: &str) -> String {
    // A UDID is ASCII hex; anything else is cut nowhere (a byte index in
    // the middle of a character would panic).
    if serial.len() == 24 && serial.is_ascii() && !serial.contains('-') {
        format!("{}-{}", &serial[..8], &serial[8..])
    } else {
        serial.to_string()
    }
}

pub struct RecordStore {
    dir: PathBuf,
}

impl RecordStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("lockdown"),
        }
    }

    fn candidates(&self, serial: &str) -> Vec<PathBuf> {
        let d = dashed(serial);
        let names = [
            d.clone(),
            d.to_uppercase(),
            d.to_lowercase(),
            serial.to_string(),
            serial.to_uppercase(),
            serial.to_lowercase(),
        ];
        let mut out = Vec::new();
        for dir in [self.dir.clone(), PathBuf::from(SYSTEM_STORE)] {
            for n in &names {
                let p = dir.join(format!("{n}.plist"));
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// The record for this phone, from our store or the system's.
    pub fn read(&self, serial: &str) -> Option<Value> {
        self.candidates(serial)
            .into_iter()
            .find_map(|p| fs::read(&p).ok().and_then(|b| plist::decode_any(&b)))
    }

    /// Save, durably: pairing is a one-time interactive step and this
    /// board does lock up; a record left in the page cache would be gone
    /// on the next unclean reboot and the driver asked to trust again.
    pub fn save(&self, serial: &str, record: &Value) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{}.plist", dashed(serial)));
        let tmp = self.dir.join(format!("{}.plist.tmp", dashed(serial)));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&plist::encode_xml(record))?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        log::info!("lockdown: stored the pair record for {}", dashed(serial));
        Ok(())
    }

    pub fn remove(&self, serial: &str) {
        let _ = fs::remove_file(self.dir.join(format!("{}.plist", dashed(serial))));
    }

    /// The SystemBUID every record of this host carries: the system's when
    /// usbmuxd made one, else ours, made once.
    pub fn system_buid(&self) -> String {
        for dir in [PathBuf::from(SYSTEM_STORE), self.dir.clone()] {
            if let Some(v) = fs::read(dir.join("SystemConfiguration.plist"))
                .ok()
                .and_then(|b| plist::decode_any(&b))
                && let Some(b) = v.get("SystemBUID").and_then(Value::as_str)
            {
                return b.to_string();
            }
        }
        let buid = crate::pairing::uuid_upper()
            .unwrap_or_else(|| "00000000-0000-4000-8000-000000000000".into());
        let v = Value::dict().with("SystemBUID", Value::str(&buid));
        if fs::create_dir_all(&self.dir).is_ok() {
            let _ = fs::write(
                self.dir.join("SystemConfiguration.plist"),
                plist::encode_xml(&v),
            );
        }
        buid
    }
}

// ── The client ────────────────────────────────────────────────────────────

pub struct Client {
    /// Taken out for the moment the TLS wrapper is put around it.
    stream: Option<Stream>,
}

impl Client {
    pub fn connect(host: &MuxHost) -> Result<Self, Error> {
        let mut s = host.connect(LOCKDOWN_PORT)?;
        s.set_read_timeout(STEP_TIMEOUT);
        let mut c = Self {
            stream: Some(Stream::Plain(s)),
        };
        let reply = c.request(Value::dict().with("Request", Value::str("QueryType")))?;
        if reply.str_or("Type", "") != "com.apple.mobile.lockdown" {
            return Err(Error::Protocol("not lockdownd".into()));
        }
        Ok(c)
    }

    fn stream(&mut self) -> Result<&mut Stream, Error> {
        self.stream
            .as_mut()
            .ok_or_else(|| Error::Protocol("no connection".into()))
    }

    fn send(&mut self, mut msg: Value) -> Result<(), Error> {
        msg.set("Label", Value::str(LABEL));
        let body = plist::encode_xml(&msg);
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        self.stream()?.write_all(&out)?;
        Ok(())
    }

    fn read_exact_timed(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut got = 0;
        while got < buf.len() {
            match self.stream()?.read(&mut buf[got..]) {
                Ok(0) => {
                    return Err(
                        io::Error::new(io::ErrorKind::UnexpectedEof, "lockdown closed").into(),
                    );
                }
                Ok(n) => got += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "lockdown did not answer",
                        )
                        .into());
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Value, Error> {
        let mut len = [0u8; 4];
        self.read_exact_timed(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_MESSAGE {
            return Err(Error::Protocol(format!("message length {len}")));
        }
        let mut body = vec![0u8; len];
        self.read_exact_timed(&mut body)?;
        plist::decode_any(&body).ok_or_else(|| Error::Protocol("unreadable plist".into()))
    }

    /// One exchange; the reply checked for an `Error`.
    fn request(&mut self, msg: Value) -> Result<Value, Error> {
        let what = msg.str_or("Request", "").to_string();
        self.send(msg)?;
        let reply = self.receive()?;
        if let Some(e) = reply.get("Error").and_then(Value::as_str) {
            return Err(match e {
                "PairingDialogResponsePending" => Error::TrustPending,
                "UserDeniedPairing" => Error::UserDenied,
                "PasswordProtected" => Error::PasswordProtected,
                "InvalidHostID" => Error::InvalidHostId,
                other => Error::Refused(other.to_string()),
            });
        }
        if reply
            .get("Request")
            .and_then(Value::as_str)
            .is_some_and(|r| r != what)
        {
            return Err(Error::Protocol(format!(
                "answered a different request than {what}"
            )));
        }
        if reply
            .get("Result")
            .and_then(Value::as_str)
            .is_some_and(|r| r != "Success")
        {
            return Err(Error::Refused(format!("{what} failed")));
        }
        Ok(reply)
    }

    pub fn get_value(&mut self, key: &str) -> Result<Value, Error> {
        let reply = self.request(
            Value::dict()
                .with("Request", Value::str("GetValue"))
                .with("Key", Value::str(key)),
        )?;
        reply
            .get("Value")
            .cloned()
            .ok_or_else(|| Error::Protocol(format!("no value for {key}")))
    }

    /// Pair: a fresh record (keys, certificates, a host id) offered to the
    /// phone, which asks the driver to trust the car. Saved on success.
    pub fn pair(&mut self, serial: &str, store: &RecordStore) -> Result<Value, Error> {
        let public_key = self.get_value("DevicePublicKey")?;
        let pem = public_key
            .as_data()
            .map(<[u8]>::to_vec)
            .or_else(|| public_key.as_str().map(|s| s.as_bytes().to_vec()))
            .ok_or_else(|| Error::Protocol("no device public key".into()))?;
        let mut record = generate_record(&pem)?;
        record.set("SystemBUID", Value::str(&store.system_buid()));
        let host_id =
            crate::pairing::uuid_upper().ok_or_else(|| Error::Protocol("no randomness".into()))?;
        record.set("HostID", Value::str(&host_id));
        // The phone sees the certificates and ids, never the private keys.
        let mut offered = record.clone();
        if let Value::Dict(d) = &mut offered {
            d.retain(|(k, _)| k != "RootPrivateKey" && k != "HostPrivateKey");
        }
        let wifi = self.get_value("WiFiAddress").ok();
        let reply = self.request(
            Value::dict()
                .with("Request", Value::str("Pair"))
                .with("PairRecord", offered)
                .with("ProtocolVersion", Value::str(PROTOCOL_VERSION))
                .with(
                    "PairingOptions",
                    Value::dict().with("ExtendedPairingErrors", Value::Bool(true)),
                ),
        )?;
        if let Some(bag) = reply.get("EscrowBag") {
            record.set("EscrowBag", bag.clone());
        }
        if let Some(mac) = wifi.and_then(|v| v.as_str().map(str::to_string)) {
            record.set("WiFiMACAddress", Value::str(&mac));
        }
        store.save(serial, &record)?;
        Ok(record)
    }

    /// StartSession with the record's host id; the connection becomes TLS
    /// when the phone asks (it always does).
    pub fn start_session(&mut self, record: &Value) -> Result<(), Error> {
        let host_id = record.str_or("HostID", "").to_string();
        let buid = record.str_or("SystemBUID", "").to_string();
        if host_id.is_empty() {
            return Err(Error::InvalidHostId);
        }
        let reply = self.request(
            Value::dict()
                .with("Request", Value::str("StartSession"))
                .with("HostID", Value::str(&host_id))
                .with("SystemBUID", Value::str(&buid)),
        )?;
        if reply.bool_of("EnableSessionSSL") == Some(true) {
            let plain = match self.stream.take() {
                Some(Stream::Plain(s)) => s,
                _ => return Err(Error::Protocol("session already under TLS".into())),
            };
            self.stream = Some(Stream::Tls(Box::new(tls_connect(record, plain)?)));
        }
        Ok(())
    }

    /// StartService: the port to connect to and whether it wants TLS.
    pub fn start_service(&mut self, name: &str) -> Result<(u16, bool), Error> {
        let reply = self.request(
            Value::dict()
                .with("Request", Value::str("StartService"))
                .with("Service", Value::str(name)),
        )?;
        let port = reply.int_or("Port", 0);
        if !(1..=65535).contains(&port) {
            return Err(Error::Refused(format!("{name} has no port")));
        }
        Ok((port as u16, reply.bool_of("EnableServiceSSL") == Some(true)))
    }
}

// ── TLS ───────────────────────────────────────────────────────────────────

fn record_pem<'a>(record: &'a Value, key: &str) -> Result<&'a [u8], Error> {
    record
        .get(key)
        .and_then(|v| v.as_data().or_else(|| v.as_str().map(str::as_bytes)))
        .ok_or_else(|| Error::Protocol(format!("the pair record has no {key}")))
}

/// Wrap a mux connection in TLS as the phone expects it: our root
/// certificate and key as the client's (what libimobiledevice sends),
/// no verification of the phone's self-signed certificate, TLS 1.0 and
/// up at security level 0 for the older phones.
pub fn tls_connect(record: &Value, mut stream: MuxStream) -> Result<SslStream<MuxStream>, Error> {
    let cert = X509::from_pem(record_pem(record, "RootCertificate")?)?;
    let key = PKey::private_key_from_pem(record_pem(record, "RootPrivateKey")?)?;
    let mut b = SslContext::builder(SslMethod::tls_client())?;
    b.set_security_level(0);
    b.set_min_proto_version(Some(SslVersion::TLS1))?;
    // Without this OpenSSL 3 refuses the older phones' renegotiation, and
    // the phone closing without a close-notify is not an error.
    b.set_options(
        SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION | SslOptions::IGNORE_UNEXPECTED_EOF,
    );
    b.set_certificate(&cert)?;
    b.set_private_key(&key)?;
    b.set_verify(SslVerifyMode::NONE);
    let ssl = Ssl::new(&b.build())?;
    stream.set_read_timeout(Duration::from_millis(200));
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut pending = match ssl.connect(stream) {
        Ok(s) => return Ok(s),
        Err(HandshakeError::WouldBlock(mid)) => mid,
        Err(HandshakeError::Failure(mid)) => return Err(Error::Tls(mid.error().to_string())),
        Err(HandshakeError::SetupFailure(e)) => return Err(e.into()),
    };
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Tls("handshake timed out".into()));
        }
        pending = match pending.handshake() {
            Ok(s) => return Ok(s),
            Err(HandshakeError::WouldBlock(mid)) => mid,
            Err(HandshakeError::Failure(mid)) => return Err(Error::Tls(mid.error().to_string())),
            Err(HandshakeError::SetupFailure(e)) => return Err(e.into()),
        };
    }
}

// ── Certificates for a first pairing ──────────────────────────────────────

fn cert_builder(pubkey: &PKey<impl openssl::pkey::HasPublic>) -> Result<X509Builder, Error> {
    let mut b = X509::builder()?;
    b.set_version(2)?;
    let zero = BigNum::from_u32(0)?;
    b.set_serial_number(Asn1Integer::from_bn(&zero)?.as_ref())?;
    b.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    b.set_not_after(Asn1Time::days_from_now(3650)?.as_ref())?;
    b.set_pubkey(pubkey)?;
    Ok(b)
}

/// The record libimobiledevice would make: a root key and its CA
/// certificate, a host key and certificate signed by the root, and a
/// certificate for the phone's own RSA key signed by the root, all PEM.
fn generate_record(device_public_pem: &[u8]) -> Result<Value, Error> {
    let root_key = PKey::from_rsa(Rsa::generate(2048)?)?;
    let host_key = PKey::from_rsa(Rsa::generate(2048)?)?;
    let device_key: PKey<openssl::pkey::Public> =
        match Rsa::public_key_from_pem_pkcs1(device_public_pem) {
            Ok(rsa) => PKey::from_rsa(rsa)?,
            Err(_) => PKey::public_key_from_pem(device_public_pem)?,
        };

    let mut root = cert_builder(&root_key)?;
    root.append_extension(BasicConstraints::new().critical().ca().build()?)?;
    root.sign(&root_key, MessageDigest::sha256())?;
    let root_cert = root.build();

    let mut host = cert_builder(&host_key)?;
    host.append_extension(BasicConstraints::new().critical().build()?)?;
    host.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;
    host.sign(&root_key, MessageDigest::sha256())?;
    let host_cert = host.build();

    let mut dev = cert_builder(&device_key)?;
    dev.append_extension(BasicConstraints::new().critical().build()?)?;
    let ski = SubjectKeyIdentifier::new().build(&dev.x509v3_context(None, None))?;
    dev.append_extension(ski)?;
    dev.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;
    dev.sign(&root_key, MessageDigest::sha256())?;
    let dev_cert = dev.build();

    Ok(Value::dict()
        .with("DeviceCertificate", Value::Data(dev_cert.to_pem()?))
        .with("HostCertificate", Value::Data(host_cert.to_pem()?))
        .with(
            "HostPrivateKey",
            Value::Data(host_key.private_key_to_pem_pkcs8()?),
        )
        .with("RootCertificate", Value::Data(root_cert.to_pem()?))
        .with(
            "RootPrivateKey",
            Value::Data(root_key.private_key_to_pem_pkcs8()?),
        ))
}

/// What a pair record must hold to open a TLS session.
pub fn record_usable(record: &Value) -> bool {
    ["RootCertificate", "RootPrivateKey", "HostID"]
        .iter()
        .all(|k| record.get(k).is_some())
}

/// The whole way to the carkit channel: pair if the phone does not know
/// us (or rejects what we stored), start the session, start the service,
/// connect to it under TLS.
pub fn open_carkit(host: &MuxHost, serial: &str, store: &RecordStore) -> Result<Stream, Error> {
    let mut record = store.read(serial).filter(record_usable);
    if record.is_none() {
        log::info!(
            "lockdown: no pair record for {}, pairing (tap Trust on the phone)",
            dashed(serial)
        );
        let mut c = Client::connect(host)?;
        record = Some(c.pair(serial, store)?);
    }
    let Some(record) = record else {
        return Err(Error::InvalidHostId);
    };
    let mut c = Client::connect(host)?;
    let record = match c.start_session(&record) {
        Ok(()) => record,
        Err(Error::InvalidHostId) => {
            log::info!("lockdown: the phone rejected our pair record, pairing again");
            store.remove(serial);
            let mut p = Client::connect(host)?;
            let fresh = p.pair(serial, store)?;
            c = Client::connect(host)?;
            c.start_session(&fresh)?;
            fresh
        }
        Err(e) => return Err(e),
    };
    let (port, ssl) = c.start_service(CARKIT_SERVICE)?;
    let s = host.connect(port)?;
    Ok(if ssl {
        Stream::Tls(Box::new(tls_connect(&record, s)?))
    } else {
        Stream::Plain(s)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udids_are_dashed_once() {
        assert_eq!(
            dashed("00008150001C31300A63401C"),
            "00008150-001C31300A63401C"
        );
        assert_eq!(
            dashed("00008150-001C31300A63401C"),
            "00008150-001C31300A63401C"
        );
        assert_eq!(dashed("abc"), "abc");
    }

    #[test]
    fn a_record_is_generated_and_usable() {
        let phone = Rsa::generate(1024).unwrap();
        let pem = phone.public_key_to_pem_pkcs1().unwrap();
        let record = generate_record(&pem).unwrap();
        assert!(!record_usable(&record));
        let record = record.with("HostID", Value::str("X"));
        assert!(record_usable(&record));
        let root = X509::from_pem(record_pem(&record, "RootCertificate").unwrap()).unwrap();
        let host = X509::from_pem(record_pem(&record, "HostCertificate").unwrap()).unwrap();
        let dev = X509::from_pem(record_pem(&record, "DeviceCertificate").unwrap()).unwrap();
        let root_key = root.public_key().unwrap();
        assert!(host.verify(&root_key).unwrap());
        assert!(dev.verify(&root_key).unwrap());
        assert!(
            dev.public_key()
                .unwrap()
                .public_eq(&PKey::from_rsa(phone).unwrap())
        );
        // The store writes XML and reads it back.
        let dir = std::env::temp_dir().join(format!("lehu-lockdown-{}", std::process::id()));
        let store = RecordStore::new(&dir);
        store.save("00008150001C31300A63401C", &record).unwrap();
        let back = store.read("00008150001C31300A63401C").unwrap();
        assert_eq!(back.str_or("HostID", ""), "X");
        assert_eq!(
            record_pem(&back, "RootPrivateKey").unwrap(),
            record_pem(&record, "RootPrivateKey").unwrap()
        );
        let buid = store.system_buid();
        assert_eq!(buid.len(), 36);
        assert_eq!(store.system_buid(), buid);
        let _ = fs::remove_dir_all(dir);
    }
}
