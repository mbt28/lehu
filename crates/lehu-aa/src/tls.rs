// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The TLS 1.2 client the GAL session runs inside its frames.
//!
//! Nothing here touches a socket: the records travel as the payload of
//! encrypted frames, so OpenSSL is driven over an in-memory stream the way
//! the C++ drove it over memory BIOs. Ciphertext from the phone is fed in,
//! the ciphertext OpenSSL produces is taken out, and the plaintext is read
//! straight into the caller's reassembly buffer. The phone does not verify
//! our certificate against a public CA and we do not verify its.
//!
//! The cipher is the costly part on this CPU, which has no AES
//! instructions: FastCarPlay measured AES-128-GCM at 39 % of the core
//! against 10 % for ChaCha20-Poly1305, which libssl carries ARMv4 assembly
//! for. The phone prefers AES-GCM by server preference and takes ChaCha20
//! only when we offer nothing else, hence `force_chacha20`: off by default,
//! since a phone that insists on AES-GCM would then fail to connect.

use std::io::{self, Read, Write};

use openssl::pkey::PKey;
use openssl::ssl::{ErrorCode, Ssl, SslContext, SslMethod, SslStream, SslVerifyMode, SslVersion};
use openssl::x509::X509;

use crate::cert;

/// The largest TLS record's plaintext: how much one `ssl_read` can yield.
const RECORD: usize = 16 * 1024;

/// The cipher list that leaves the phone only ChaCha20-Poly1305.
pub const CHACHA20_ONLY: &str = "ECDHE-RSA-CHACHA20-POLY1305";

/// Ciphertext both ways, in memory. OpenSSL reads the phone's bytes from
/// `incoming` and writes its own into `outgoing`; a read with nothing
/// pending says "would block", which OpenSSL reports as `WANT_READ`.
#[derive(Default)]
pub struct MemStream {
    incoming: Vec<u8>,
    pos: usize,
    outgoing: Vec<u8>,
}

impl MemStream {
    fn feed(&mut self, bytes: &[u8]) {
        if self.pos == self.incoming.len() {
            self.incoming.clear();
            self.pos = 0;
        }
        self.incoming.extend_from_slice(bytes);
    }

    fn drain_into(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.outgoing);
        self.outgoing.clear();
    }
}

impl Read for MemStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pending = &self.incoming[self.pos..];
        if pending.is_empty() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = pending.len().min(buf.len());
        buf[..n].copy_from_slice(&pending[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for MemStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.outgoing.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<openssl::error::ErrorStack> for Error {
    fn from(e: openssl::error::ErrorStack) -> Self {
        Error(e.to_string())
    }
}

impl From<openssl::ssl::Error> for Error {
    fn from(e: openssl::ssl::Error) -> Self {
        Error(e.to_string())
    }
}

pub struct Tls {
    stream: SslStream<MemStream>,
    done: bool,
}

/// A TLS 1.2 client context carrying the head unit certificate.
pub fn client_context(force_chacha20: bool) -> Result<SslContext, Error> {
    let mut b = SslContext::builder(SslMethod::tls_client())?;
    b.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    b.set_max_proto_version(Some(SslVersion::TLS1_2))?;
    b.set_verify(SslVerifyMode::NONE);
    if force_chacha20 {
        b.set_cipher_list(CHACHA20_ONLY)?;
    }
    let cert = X509::from_pem(cert::HU_CERT_PEM.as_bytes())?;
    let key = PKey::private_key_from_pem(cert::HU_KEY_PEM.as_bytes())?;
    b.set_certificate(&cert)?;
    b.set_private_key(&key)?;
    Ok(b.build())
}

impl Tls {
    pub fn new(force_chacha20: bool) -> Result<Self, Error> {
        let ctx = client_context(force_chacha20)?;
        Self::from_ssl(Ssl::new(&ctx)?)
    }

    /// A session over any configured `Ssl`, for the test server as well.
    pub fn from_ssl(ssl: Ssl) -> Result<Self, Error> {
        Ok(Self {
            stream: SslStream::new(ssl, MemStream::default())?,
            done: false,
        })
    }

    pub fn ready(&self) -> bool {
        self.done
    }

    /// One handshake step as the client: feed the phone's record (empty
    /// for the first step), append our records to `out`. True once the
    /// handshake is complete.
    pub fn handshake(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<bool, Error> {
        self.step(input, out, true)
    }

    /// The same as the server (tests).
    pub fn accept_step(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<bool, Error> {
        self.step(input, out, false)
    }

    fn step(&mut self, input: &[u8], out: &mut Vec<u8>, client: bool) -> Result<bool, Error> {
        if self.done {
            return Ok(true);
        }
        self.stream.get_mut().feed(input);
        let r = if client {
            self.stream.connect()
        } else {
            self.stream.accept()
        };
        match r {
            Ok(()) => self.done = true,
            Err(e) if e.code() == ErrorCode::WANT_READ || e.code() == ErrorCode::WANT_WRITE => {}
            Err(e) => return Err(Error(format!("handshake: {e}"))),
        }
        self.stream.get_mut().drain_into(out);
        Ok(self.done)
    }

    /// Ciphertext in, plaintext appended to `out`, which grows by the
    /// record size at most once per record and keeps its capacity.
    pub fn decrypt(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        self.stream.get_mut().feed(input);
        loop {
            let start = out.len();
            out.resize(start + RECORD, 0);
            match self.stream.ssl_read(&mut out[start..]) {
                Ok(0) => {
                    out.truncate(start);
                    return Err(Error("the phone closed the TLS session".into()));
                }
                Ok(n) => out.truncate(start + n),
                Err(e) if e.code() == ErrorCode::WANT_READ => {
                    out.truncate(start);
                    return Ok(());
                }
                Err(e) => {
                    out.truncate(start);
                    return Err(Error(format!("decrypt: {e}")));
                }
            }
        }
    }

    /// Plaintext in, ciphertext appended to `out`.
    pub fn encrypt(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        let mut done = 0;
        while done < plain.len() {
            let n = self
                .stream
                .ssl_write(&plain[done..])
                .map_err(|e| Error(format!("encrypt: {e}")))?;
            if n == 0 {
                return Err(Error("encrypt: nothing written".into()));
            }
            done += n;
        }
        self.stream.get_mut().drain_into(out);
        Ok(())
    }

    /// The negotiated cipher, for the log.
    pub fn cipher_name(&self) -> &'static str {
        self.stream
            .ssl()
            .current_cipher()
            .map(|c| c.name())
            .unwrap_or("none")
    }
}

/// The phone's end for the tests: a server with the same certificate,
/// so the two can shake hands in process.
#[cfg(test)]
pub(crate) fn test_phone(cipher: Option<&str>) -> Tls {
    let mut b = SslContext::builder(SslMethod::tls_server()).unwrap();
    b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
    b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
    if let Some(c) = cipher {
        b.set_cipher_list(c).unwrap();
    }
    let cert = X509::from_pem(cert::HU_CERT_PEM.as_bytes()).unwrap();
    let key = PKey::private_key_from_pem(cert::HU_KEY_PEM.as_bytes()).unwrap();
    b.set_certificate(&cert).unwrap();
    b.set_private_key(&key).unwrap();
    let ctx = b.build();
    Tls::from_ssl(Ssl::new(&ctx).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(cipher: Option<&str>) -> Tls {
        test_phone(cipher)
    }

    /// Drive both ends to completion; how many records went each way.
    pub(crate) fn shake(client: &mut Tls, server: &mut Tls) -> usize {
        let mut to_server = Vec::new();
        let mut to_client = Vec::new();
        let mut rounds = 0;
        assert!(!client.handshake(&[], &mut to_server).unwrap());
        while !(client.ready() && server.ready()) {
            rounds += 1;
            assert!(rounds < 10, "the handshake does not converge");
            to_client.clear();
            server.accept_step(&to_server, &mut to_client).unwrap();
            to_server.clear();
            client.handshake(&to_client, &mut to_server).unwrap();
        }
        rounds
    }

    #[test]
    fn the_handshake_completes_in_memory_and_records_cross() {
        let mut client = Tls::new(false).unwrap();
        let mut server = phone(None);
        shake(&mut client, &mut server);
        assert!(client.ready());
        let mut cipher = Vec::new();
        client.encrypt(b"hello phone", &mut cipher).unwrap();
        assert!(cipher.len() > 11);
        let mut plain = vec![0xaa; 3];
        server.decrypt(&cipher, &mut plain).unwrap();
        assert_eq!(&plain[3..], b"hello phone");
        cipher.clear();
        server.encrypt(&[7u8; 40_000], &mut cipher).unwrap();
        let mut got = Vec::new();
        // The ciphertext arrives in frame-sized pieces, records spanning
        // them.
        for piece in cipher.chunks(0x4000) {
            client.decrypt(piece, &mut got).unwrap();
        }
        assert_eq!(got.len(), 40_000);
        assert!(got.iter().all(|&b| b == 7));
        assert_eq!(got.capacity() % RECORD, 0, "grown by whole records");
    }

    #[test]
    fn forcing_chacha20_leaves_the_phone_no_other_choice() {
        let mut client = Tls::new(true).unwrap();
        // A server that prefers AES-GCM still ends up on ChaCha20.
        let mut server = phone(Some(
            "ECDHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-CHACHA20-POLY1305",
        ));
        shake(&mut client, &mut server);
        assert_eq!(client.cipher_name(), "ECDHE-RSA-CHACHA20-POLY1305");
        let mut aes_only = phone(Some("ECDHE-RSA-AES128-GCM-SHA256"));
        let mut client = Tls::new(true).unwrap();
        let mut hello = Vec::new();
        client.handshake(&[], &mut hello).unwrap();
        let mut reply = Vec::new();
        assert!(aes_only.accept_step(&hello, &mut reply).is_err());
    }

    #[test]
    fn garbage_is_rejected_not_read() {
        let mut client = Tls::new(false).unwrap();
        let mut server = phone(None);
        shake(&mut client, &mut server);
        let mut out = Vec::new();
        assert!(
            client
                .decrypt(&[0x17, 0x03, 0x03, 0x00, 0x05, 1, 2, 3, 4, 5], &mut out)
                .is_err()
        );
        assert!(out.is_empty());
        assert_eq!(Tls::new(false).unwrap().cipher_name(), "none");
    }
}
