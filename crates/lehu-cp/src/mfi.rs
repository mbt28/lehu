//! The Apple authentication coprocessor (MFi 2.0C or 3.0) on an i2c bus:
//! identify it, read its certificate, have it sign a digest. The one piece
//! of CarPlay-specific hardware, behind `pairing::MfiSigner`.
//!
//! The chip's i2c manners, each learned the hard way on the real part:
//!
//! - No repeated-start reads. A combined pointer-write and read in one
//!   transaction gets a remote-I/O error; the pointer is written with a
//!   stop, then the read starts fresh, with a short settle between.
//! - It NAKs the first transaction after idle and wakes on it; retry.
//! - One byte per read transaction. A multi-byte read NAKs; the pointer
//!   auto-increments across separate one-byte reads.
//! - Never rewrite the pointer between retries of a read: that restarts
//!   the wake and the read never catches up.
//! - It clock-stretches through signing (tens of milliseconds), so the
//!   status read retries on a budget of about a second.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::thread;
use std::time::Duration;

use crate::pairing::MfiSigner;

const REG_DEVICE_VERSION: u8 = 0x00;
const REG_FIRMWARE_VERSION: u8 = 0x01;
const REG_PROTOCOL_MAJOR: u8 = 0x02;
const REG_PROTOCOL_MINOR: u8 = 0x03;
const REG_ERROR_CODE: u8 = 0x05;
const REG_AUTH_CONTROL: u8 = 0x10;
const REG_SIGNATURE_LENGTH: u8 = 0x11;
const REG_SIGNATURE_DATA: u8 = 0x12;
const REG_CHALLENGE_LENGTH: u8 = 0x20;
const REG_CHALLENGE_DATA: u8 = 0x21;
const REG_CERT_LENGTH: u8 = 0x30;
const REG_CERT_DATA_BASE: u8 = 0x31;
const REG_SELF_TEST: u8 = 0x40;

const AUTH_START_SIGNATURE: u8 = 0x01;
const CERT_PAGE: usize = 128;
const MAX_SIGNATURE: usize = 128;

const I2C_SLAVE: libc::c_ulong = 0x0703;
const SETTLE: Duration = Duration::from_millis(1);
/// Retries of about a millisecond each: the wake NAK and the signing
/// stretch both fit in a second.
const IO_RETRIES: u32 = 500;

#[derive(Clone, Copy, Debug, Default)]
pub struct Info {
    /// 0x05 for 2.0C, 0x07 for 3.0.
    pub device_version: u8,
    pub firmware_version: u8,
    pub protocol_major: u8,
    pub protocol_minor: u8,
    pub error_code: u8,
}

pub struct Mfi {
    fd: OwnedFd,
    pub address: u8,
    protocol_major: u8,
}

impl Mfi {
    /// Open the bus at `address`, or at the two documented addresses when
    /// it is 0, keeping the first that answers an identity read.
    pub fn open(bus: &str, address: u8) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(bus)?;
        let mut chip = Self {
            fd: file.into(),
            address: 0,
            protocol_major: 0,
        };
        let candidates: &[u8] = if address != 0 {
            &[address]
        } else {
            &[0x10, 0x11]
        };
        for &a in candidates {
            if chip.select(a)? {
                return Ok(chip);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no MFi coprocessor on {bus} (check its reset and power)"),
        ))
    }

    fn select(&mut self, address: u8) -> io::Result<bool> {
        // SAFETY: a plain ioctl with an integer argument on our own fd.
        if unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                I2C_SLAVE as _,
                libc::c_ulong::from(address),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        self.address = address;
        // A known device version is the cheapest proof the chip is there,
        // awake and read correctly.
        let mut v = [0u8];
        Ok(self.read_reg(REG_DEVICE_VERSION, &mut v).is_ok() && matches!(v[0], 0x05 | 0x07))
    }

    fn write_raw(&self, bytes: &[u8]) -> bool {
        // SAFETY: the pointer and the length describe a live slice.
        let n = unsafe { libc::write(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        n == bytes.len() as isize
    }

    fn read_raw(&self, byte: &mut u8) -> bool {
        // SAFETY: one byte into a live location.
        unsafe { libc::read(self.fd.as_raw_fd(), (byte as *mut u8).cast(), 1) == 1 }
    }

    fn read_reg(&self, reg: u8, out: &mut [u8]) -> io::Result<()> {
        for _ in 0..8 {
            // The pointer, once, retried until it ACKs.
            let mut set = false;
            for _ in 0..IO_RETRIES {
                if self.write_raw(&[reg]) {
                    set = true;
                    break;
                }
                thread::sleep(SETTLE);
            }
            if !set {
                continue;
            }
            thread::sleep(SETTLE);
            // Each byte its own transaction, retried in place: the pointer
            // only advances on a byte that was read.
            let mut got = 0;
            'bytes: for b in out.iter_mut() {
                for _ in 0..IO_RETRIES {
                    if self.read_raw(b) {
                        got += 1;
                        continue 'bytes;
                    }
                    thread::sleep(SETTLE);
                }
                break;
            }
            if got == out.len() {
                return Ok(());
            }
        }
        Err(io::Error::other(format!(
            "read of register {reg:#04x} failed"
        )))
    }

    fn write_reg(&self, reg: u8, data: &[u8]) -> io::Result<()> {
        if data.len() > 256 {
            return Err(io::Error::other("write too large"));
        }
        let mut buf = [0u8; 257];
        buf[0] = reg;
        buf[1..=data.len()].copy_from_slice(data);
        for _ in 0..IO_RETRIES {
            if self.write_raw(&buf[..=data.len()]) {
                thread::sleep(SETTLE);
                return Ok(());
            }
            thread::sleep(SETTLE);
        }
        Err(io::Error::other(format!(
            "write of register {reg:#04x} failed"
        )))
    }

    /// The identity registers, one read each: the chip does not advance
    /// across arbitrary registers, only through the certificate and the
    /// signature. Sets the protocol the sign flow follows.
    pub fn identify(&mut self) -> io::Result<Info> {
        let mut info = Info::default();
        let mut b = [0u8];
        for (reg, slot) in [
            (REG_DEVICE_VERSION, &mut info.device_version),
            (REG_FIRMWARE_VERSION, &mut info.firmware_version),
            (REG_PROTOCOL_MAJOR, &mut info.protocol_major),
            (REG_PROTOCOL_MINOR, &mut info.protocol_minor),
            (REG_ERROR_CODE, &mut info.error_code),
        ] {
            self.read_reg(reg, &mut b)?;
            *slot = b[0];
        }
        self.protocol_major = info.protocol_major;
        Ok(info)
    }

    /// The accessory certificate the phone validates: read once.
    pub fn read_certificate(&self) -> io::Result<Vec<u8>> {
        // The certificate length NAKs until the self-test status was read;
        // on 2.0C the test is started by writing 1 first.
        if self.protocol_major == 2 {
            self.write_reg(REG_SELF_TEST, &[1])?;
        }
        let mut st = [0u8];
        self.read_reg(REG_SELF_TEST, &mut st)?;
        if st[0] >> 6 != 3 {
            return Err(io::Error::other(format!(
                "self-test not passed ({:#04x})",
                st[0]
            )));
        }
        let mut len = [0u8; 2];
        self.read_reg(REG_CERT_LENGTH, &mut len)?;
        let cert_len = usize::from(u16::from_be_bytes(len));
        let plausible = if self.protocol_major == 3 {
            (607..=609).contains(&cert_len)
        } else {
            cert_len > 0 && cert_len < 1280
        };
        if !plausible {
            return Err(io::Error::other(format!(
                "implausible certificate length {cert_len}"
            )));
        }
        let mut out = vec![0u8; cert_len];
        for (page, chunk) in out.chunks_mut(CERT_PAGE).enumerate() {
            self.read_reg(REG_CERT_DATA_BASE + page as u8, chunk)?;
        }
        Ok(out)
    }

    /// Sign a digest: 32 bytes (SHA-256) on 3.0 for a 64-byte ECDSA
    /// signature, 20 bytes (SHA-1) on 2.0C for a 128-byte RSA one.
    pub fn sign(&self, digest: &[u8]) -> io::Result<Vec<u8>> {
        let expect = if self.protocol_major == 3 { 32 } else { 20 };
        if digest.len() != expect {
            return Err(io::Error::other(format!(
                "a {expect}-byte digest is signed on protocol {}",
                self.protocol_major
            )));
        }
        // 2.0C wants the challenge and response lengths first; 3.0 not.
        if self.protocol_major == 2 {
            self.write_reg(REG_CHALLENGE_LENGTH, &(digest.len() as u16).to_be_bytes())?;
            self.write_reg(REG_SIGNATURE_LENGTH, &128u16.to_be_bytes())?;
        }
        self.write_reg(REG_CHALLENGE_DATA, digest)?;
        self.write_reg(REG_AUTH_CONTROL, &[AUTH_START_SIGNATURE])?;
        // The status read returns once the chip stops stretching the clock:
        // 0x10 is success, anything else its error code.
        let mut status = [0u8];
        self.read_reg(REG_AUTH_CONTROL, &mut status)?;
        if status[0] != 0x10 {
            return Err(io::Error::other(format!(
                "signing failed, status {:#04x}",
                status[0]
            )));
        }
        let mut len = [0u8; 2];
        self.read_reg(REG_SIGNATURE_LENGTH, &mut len)?;
        let sig_len = usize::from(u16::from_be_bytes(len));
        if sig_len == 0 || sig_len > MAX_SIGNATURE {
            return Err(io::Error::other(format!(
                "implausible signature length {sig_len}"
            )));
        }
        let mut sig = vec![0u8; sig_len];
        self.read_reg(REG_SIGNATURE_DATA, &mut sig)?;
        Ok(sig)
    }
}

/// The chip with its certificate cached: `MfiSigner` for the handshakes.
pub struct Chip {
    pub mfi: Mfi,
    pub info: Info,
    pub certificate: Vec<u8>,
}

impl Chip {
    /// Open, identify and read the certificate; what the session keeps.
    pub fn open(bus: &str, address: u8) -> io::Result<Self> {
        let mut mfi = Mfi::open(bus, address)?;
        let info = mfi.identify()?;
        let certificate = mfi.read_certificate()?;
        Ok(Self {
            mfi,
            info,
            certificate,
        })
    }

    pub fn name(&self) -> &'static str {
        if self.info.protocol_major == 3 {
            "MFi 3.0"
        } else {
            "MFi 2.0C"
        }
    }
}

impl MfiSigner for Chip {
    fn certificate(&mut self) -> Option<Vec<u8>> {
        Some(self.certificate.clone())
    }

    fn sign(&mut self, digest: &[u8]) -> Option<Vec<u8>> {
        match self.mfi.sign(digest) {
            Ok(sig) => Some(sig),
            Err(e) => {
                log::warn!("mfi: {e}");
                None
            }
        }
    }

    fn protocol_major(&self) -> u8 {
        self.info.protocol_major
    }
}
