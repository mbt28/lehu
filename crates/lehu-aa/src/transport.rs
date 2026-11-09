// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! The byte transport under the GAL session. The session speaks the same
//! protocol (framing, TLS, discovery, channels) whether the bytes ride the
//! accessory's bulk pipe (wired) or a TCP socket (wireless), so
//! this is the only piece that differs.
//!
//! The USB transport keeps several bulk IN transfers in flight into the
//! slot ring on its own thread and writes with the retry the board needs:
//! on the F1C200s the video's receive DMA holds the shared USB FIFO, so a
//! PIO bulk write in that window times out having sent nothing; reissuing
//! it is safe and lands in a DMA gap. (The kernel is booted with
//! `musb_hdrc.use_dma=0`: the sunxi MUSB's DMA silently dropped bulk OUT
//! and was three times slower than PIO.) Only that clean timeout is
//! retried; any other error, or a partial write, is a dead link.
//!
//! Opening the accessory's pipe (`bulk_pair`, the claim retry) follows
//! LIVI's livi-session-io/src/usb.rs (Lasse Heitgres, GPL-3.0); the rest
//! is FastCarPlay's aa_usb_transport.cpp, changed for LEHU.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, Direction, In, Out, TransferError};
use nusb::{Endpoint, Interface};

use crate::aoap;
use crate::ring::Ring;

/// A byte stream to the phone, shared by the session's threads.
pub trait Transport: Send + Sync {
    /// Fill `dst` exactly; false once the link is gone.
    fn read_exact(&self, dst: &mut [u8]) -> bool;
    /// Write it all; false on error, after which the link is down.
    fn write_all(&self, src: &[u8]) -> bool;
    /// Tear the link down and end any waiting read. Idempotent.
    fn close(&self);
    fn connected(&self) -> bool;
    fn name(&self) -> &'static str;
}

/// The `usb.*` settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbTuning {
    /// Bulk IN transfers in flight.
    pub async_calls: u32,
    /// Bytes each asks for; a multiple of the packet size.
    pub transfer_size: u32,
    /// Slots in the ring, in flight ones included.
    pub buffer: u32,
    pub write_timeout_ms: u32,
    pub write_retries: u32,
    pub write_retry_delay_ms: u32,
}

impl UsbTuning {
    pub fn from_settings(s: &lehu_core::settings::Usb) -> Self {
        Self {
            async_calls: s.async_calls,
            transfer_size: s.transfer_size,
            buffer: s.buffer,
            write_timeout_ms: s.write_timeout_ms,
            write_retries: s.write_retries,
            write_retry_delay_ms: s.write_retry_delay_ms,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Open(aoap::Error),
    /// The accessory has no interface with a bulk pair, or it cannot be
    /// claimed.
    Link(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Open(e) => write!(f, "{e}"),
            Error::Link(e) => f.write_str(e),
        }
    }
}

/// How long the read thread waits for a completion before it looks at the
/// stop flag again; completions wake it at once.
const READ_WAIT: Duration = Duration::from_millis(250);
const CLAIM_RETRIES: u32 = 5;
const CLAIM_RETRY: Duration = Duration::from_millis(100);

struct Writer {
    ep: Endpoint<Bulk, Out>,
    /// The one outgoing buffer, handed to the endpoint and taken back.
    scratch: Vec<u8>,
}

pub struct UsbTransport {
    device: nusb::Device,
    _interface: Interface,
    ring: Arc<Ring>,
    writer: Mutex<Writer>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    /// `close` runs from more than one thread (the loop's stop, the
    /// session's own end); joining the read thread from two at once is
    /// not allowed, so the teardown is serialised.
    closing: Mutex<bool>,
    tuning: UsbTuning,
}

impl UsbTransport {
    /// Link to a phone already in accessory mode: the configuration is set
    /// only if it is not active, since setting the active one again acts
    /// as a device reset and makes an accessory re-enumerate in a loop;
    /// then the interface with the bulk pair is claimed and the read
    /// thread starts.
    pub fn open(device: &lehu_usb::Device, tuning: &UsbTuning) -> Result<Self, Error> {
        let dev = aoap::open_node(device).map_err(Error::Open)?;
        let active = dev
            .active_configuration()
            .map(|c| c.configuration_value())
            .ok();
        if active != Some(1)
            && let Err(e) = dev.set_configuration(1).wait()
        {
            log::debug!("aa-usb: set configuration: {e} (going on)");
        }
        let (number, ep_in, ep_out) = {
            let config = dev
                .active_configuration()
                .map_err(|e| Error::Link(format!("configuration: {e}")))?;
            bulk_pair(&config).ok_or_else(|| Error::Link("no bulk endpoint pair".into()))?
        };
        let mut last = String::new();
        let mut interface = None;
        for _ in 0..CLAIM_RETRIES {
            match dev.detach_and_claim_interface(number).wait() {
                Ok(i) => {
                    interface = Some(i);
                    break;
                }
                Err(e) => {
                    last = e.to_string();
                    thread::sleep(CLAIM_RETRY);
                }
            }
        }
        let interface =
            interface.ok_or_else(|| Error::Link(format!("claiming interface {number}: {last}")))?;
        let ep_in = interface
            .endpoint::<Bulk, In>(ep_in)
            .map_err(|e| Error::Link(format!("bulk in: {e}")))?;
        let ep_out = interface
            .endpoint::<Bulk, Out>(ep_out)
            .map_err(|e| Error::Link(format!("bulk out: {e}")))?;
        let size = (tuning.transfer_size as usize).max(ep_in.max_packet_size());
        let size = size - size % ep_in.max_packet_size().max(1);
        let slots = (tuning.buffer as usize).max(tuning.async_calls as usize + 2);
        let ring = Arc::new(Ring::new(slots, size));
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(true));
        log::info!(
            "aa-usb: accessory {device} linked, {:?} speed, {} x {} bytes in flight",
            dev.speed(),
            tuning.async_calls,
            size
        );
        let reader = {
            let ring = Arc::clone(&ring);
            let stop = Arc::clone(&stop);
            let connected = Arc::clone(&connected);
            let in_flight = tuning.async_calls.max(1) as usize;
            thread::Builder::new()
                .name("aa-usb-read".into())
                .spawn(move || read_loop(ep_in, ring, stop, connected, in_flight))
                .map_err(|e| Error::Link(format!("read thread: {e}")))?
        };
        Ok(Self {
            device: dev,
            _interface: interface,
            ring,
            writer: Mutex::new(Writer {
                ep: ep_out,
                scratch: Vec::with_capacity(crate::consts::MAX_FRAME_PAYLOAD + 8),
            }),
            reader: Mutex::new(Some(reader)),
            stop,
            connected,
            closing: Mutex::new(false),
            tuning: *tuning,
        })
    }

    /// Filled transfers the reader has not consumed, for the log.
    pub fn backlog(&self) -> usize {
        self.ring.backlog()
    }
}

/// The first interface with a bulk IN and OUT endpoint: (number, in, out).
fn bulk_pair(config: &nusb::descriptors::ConfigurationDescriptor<'_>) -> Option<(u8, u8, u8)> {
    for intf in config.interfaces() {
        let Some(alt) = intf.alt_settings().next() else {
            continue;
        };
        let (mut ep_in, mut ep_out) = (None, None);
        for ep in alt.endpoints() {
            if ep.transfer_type() != TransferType::Bulk {
                continue;
            }
            match ep.direction() {
                Direction::In => ep_in = Some(ep.address()),
                Direction::Out => ep_out = Some(ep.address()),
            }
        }
        if let (Some(i), Some(o)) = (ep_in, ep_out) {
            return Some((intf.interface_number(), i, o));
        }
    }
    None
}

/// The read thread: `in_flight` transfers queued at once, each completed
/// buffer committed to the ring and a free one submitted in its place.
/// It blocks in the kernel until a transfer completes; the wait's timeout
/// only bounds how late it sees the stop flag.
fn read_loop(
    mut ep: Endpoint<Bulk, In>,
    ring: Arc<Ring>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    in_flight: usize,
) {
    for _ in 0..in_flight {
        let Some(buf) = ring.take_free() else {
            break;
        };
        ep.submit(buf);
    }
    while !stop.load(Ordering::Acquire) && ep.pending() > 0 {
        let Some(done) = ep.wait_next_complete(READ_WAIT) else {
            continue;
        };
        match done.status {
            Ok(()) => {
                log::trace!("aa-usb: transfer {} bytes", done.actual_len);
                ring.commit(done.buffer);
                match ring.take_free() {
                    Some(buf) => ep.submit(buf),
                    None => break,
                }
            }
            Err(TransferError::Cancelled) => break,
            Err(e) => {
                log::info!("aa-usb: read: {e}");
                break;
            }
        }
    }
    connected.store(false, Ordering::Release);
    ring.close();
    // Every cancelled transfer comes back at once; the buffers go with
    // the endpoint.
    ep.cancel_all();
    while ep.pending() > 0 {
        if ep.wait_next_complete(Duration::from_secs(1)).is_none() {
            log::warn!("aa-usb: a cancelled transfer did not return");
            break;
        }
    }
}

impl Transport for UsbTransport {
    fn read_exact(&self, dst: &mut [u8]) -> bool {
        self.ring.read_exact(dst)
    }

    fn write_all(&self, src: &[u8]) -> bool {
        if !self.connected() {
            return false;
        }
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let timeout = Duration::from_millis(u64::from(self.tuning.write_timeout_ms.max(1)));
        let retries = self.tuning.write_retries;
        let mut attempt = 0;
        let outcome = loop {
            let mut scratch = std::mem::take(&mut w.scratch);
            scratch.clear();
            scratch.extend_from_slice(src);
            let done = w.ep.transfer_blocking(Buffer::from(scratch), timeout);
            w.scratch = done.buffer.into_vec();
            match done.status {
                Ok(()) if done.actual_len == src.len() => break Ok(()),
                Ok(()) => break Err(format!("short write, {} of {}", done.actual_len, src.len())),
                // The timeout cancels the transfer; nothing sent is the
                // clean case the board produces, and the only one retried.
                Err(TransferError::Cancelled) if done.actual_len == 0 && attempt < retries => {
                    attempt += 1;
                    log::debug!(
                        "aa-usb: write of {} bytes timed out, retry {attempt}/{retries}",
                        src.len()
                    );
                    if self.tuning.write_retry_delay_ms > 0 {
                        thread::sleep(Duration::from_millis(u64::from(
                            self.tuning.write_retry_delay_ms,
                        )));
                    }
                    if !self.connected() {
                        break Err("link closed".into());
                    }
                }
                Err(TransferError::Cancelled) => break Err("timed out".into()),
                Err(e) => break Err(e.to_string()),
            }
        };
        match outcome {
            Ok(()) => true,
            Err(e) => {
                log::warn!("aa-usb: write of {} bytes failed: {e}", src.len());
                drop(w);
                self.connected.store(false, Ordering::Release);
                self.ring.close();
                false
            }
        }
    }

    /// Stop the read thread, release the pipe and reset the device: the
    /// reset takes the phone out of accessory mode, so the next link is a
    /// fresh accessory switch rather than a half-open pipe that refuses
    /// every write. The main loop sees the re-enumeration as a detach and
    /// an attach.
    fn close(&self) {
        let mut closing = self.closing.lock().unwrap_or_else(|e| e.into_inner());
        if *closing {
            return;
        }
        *closing = true;
        self.connected.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        self.ring.close();
        let reader = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = reader {
            let _ = t.join();
        }
        if let Err(e) = self.device.reset().wait() {
            log::debug!("aa-usb: reset: {e}");
        }
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire) && !self.ring.is_closed()
    }

    fn name(&self) -> &'static str {
        "usb"
    }
}

impl Drop for UsbTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// A connected stream: TCP for a wireless phone, a Unix pair in tests.
pub trait Stream: Send + Sync {
    fn read(&self, dst: &mut [u8]) -> std::io::Result<usize>;
    fn write(&self, src: &[u8]) -> std::io::Result<usize>;
    fn shutdown(&self);
}

impl Stream for TcpStream {
    fn read(&self, dst: &mut [u8]) -> std::io::Result<usize> {
        Read::read(&mut &*self, dst)
    }
    fn write(&self, src: &[u8]) -> std::io::Result<usize> {
        Write::write(&mut &*self, src)
    }
    fn shutdown(&self) {
        let _ = TcpStream::shutdown(self, Shutdown::Both);
    }
}

impl Stream for UnixStream {
    fn read(&self, dst: &mut [u8]) -> std::io::Result<usize> {
        Read::read(&mut &*self, dst)
    }
    fn write(&self, src: &[u8]) -> std::io::Result<usize> {
        Write::write(&mut &*self, src)
    }
    fn shutdown(&self) {
        let _ = UnixStream::shutdown(self, Shutdown::Both);
    }
}

/// The session over a stream the caller connected (the accept is the
/// caller's: the wireless session listens on `TCP_PORT`).
pub struct StreamTransport<S: Stream> {
    stream: S,
    connected: AtomicBool,
    name: &'static str,
}

impl<S: Stream> StreamTransport<S> {
    pub fn new(stream: S, name: &'static str) -> Self {
        Self {
            stream,
            connected: AtomicBool::new(true),
            name,
        }
    }
}

impl StreamTransport<TcpStream> {
    pub fn tcp(stream: TcpStream) -> Self {
        let _ = stream.set_nodelay(true);
        Self::new(stream, "tcp")
    }
}

impl<S: Stream> Transport for StreamTransport<S> {
    fn read_exact(&self, dst: &mut [u8]) -> bool {
        let mut got = 0;
        while got < dst.len() && self.connected() {
            match self.stream.read(&mut dst[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if got < dst.len() {
            self.connected.store(false, Ordering::Release);
            return false;
        }
        true
    }

    fn write_all(&self, src: &[u8]) -> bool {
        let mut sent = 0;
        while sent < src.len() {
            match self.stream.write(&src[sent..]) {
                Ok(0) => break,
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if sent < src.len() {
            self.connected.store(false, Ordering::Release);
            return false;
        }
        true
    }

    fn close(&self) {
        self.connected.store(false, Ordering::Release);
        self.stream.shutdown();
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_transport_reads_exact_and_ends_on_close() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let t = StreamTransport::new(ours, "pair");
        assert_eq!(t.name(), "pair");
        (&theirs).write_all(&[1, 2, 3, 4, 5]).unwrap();
        let mut head = [0u8; 2];
        assert!(t.read_exact(&mut head));
        assert_eq!(head, [1, 2]);
        assert!(t.write_all(&[9, 9]));
        let mut echo = [0u8; 2];
        (&theirs).read_exact(&mut echo).unwrap();
        assert_eq!(echo, [9, 9]);
        let t = Arc::new(t);
        let r = Arc::clone(&t);
        let reader = thread::spawn(move || {
            let mut rest = [0u8; 10];
            r.read_exact(&mut rest)
        });
        thread::sleep(Duration::from_millis(20));
        t.close();
        assert!(!reader.join().unwrap());
        assert!(!t.connected());
        assert!(!t.write_all(&[1]));
    }

    #[test]
    fn tuning_comes_from_the_settings() {
        let t = UsbTuning::from_settings(&lehu_core::settings::Usb::default());
        assert_eq!(t.write_retries, 6);
        assert_eq!(t.transfer_size, 2048);
    }
}
