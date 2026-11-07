//! The TCP side of a wireless phone: the listener on port 5277 the phone
//! connects to once it has joined the access point, and the tuning of the
//! accepted socket. From the accepted connection on, the GAL session runs
//! over `StreamTransport::tcp` exactly as it does over the accessory's
//! bulk pipe.
//!
//! The C++ relied on the GAL pings alone to notice a phone that left
//! Wi-Fi. That covers the read side; a write to a phone that vanished
//! would sit in the kernel's retransmissions for minutes, with the write
//! thread, and the pings it sends, stuck behind it. So the socket also gets
//! keepalive and a user timeout (LIVI's five seconds): the kernel then
//! fails the socket, and both threads see the link go.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

/// Keepalive probes after this much idleness, this far apart, this many
/// before the connection is declared dead.
pub const KEEPALIVE_IDLE_S: i32 = 5;
pub const KEEPALIVE_INTERVAL_S: i32 = 5;
pub const KEEPALIVE_COUNT: i32 = 3;
/// Unacknowledged data older than this fails the socket.
pub const USER_TIMEOUT_MS: u32 = 15_000;

/// A restart right after the previous instance can find the port held by
/// its dying listener: tried this often, this far apart.
const BIND_RETRIES: u32 = 20;
const BIND_RETRY: Duration = Duration::from_millis(250);

/// The listener, dual-stack and non-blocking: the phone comes over IPv4
/// (the address it was handed), but nothing is lost by taking v6 too.
pub struct Listener {
    inner: TcpListener,
}

impl Listener {
    /// Bind `port` on every address (0 for any port, in tests).
    pub fn bind(port: u16) -> io::Result<Listener> {
        Self::bind_retrying(port, BIND_RETRIES)
    }

    fn bind_retrying(port: u16, retries: u32) -> io::Result<Listener> {
        let mut last = None;
        for _ in 0..retries {
            match TcpListener::bind(("::", port)) {
                Ok(l) => {
                    l.set_nonblocking(true)?;
                    return Ok(Listener { inner: l });
                }
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    last = Some(e);
                    thread::sleep(BIND_RETRY);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("bind failed")))
    }

    /// The port bound.
    pub fn port(&self) -> u16 {
        self.inner.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Whether a connection waits, after at most `timeout_ms`.
    pub fn poll(&self, timeout_ms: i32) -> bool {
        let mut p = libc::pollfd {
            fd: self.inner.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd.
        unsafe { libc::poll(&mut p, 1, timeout_ms) > 0 }
    }

    /// The waiting connection, tuned; `None` when there is none or the
    /// accept failed (logged).
    pub fn accept(&self) -> Option<(TcpStream, SocketAddr)> {
        match self.inner.accept() {
            Ok((stream, peer)) => {
                if let Err(e) = tune(&stream) {
                    log::warn!("aa-tcp: socket options for {peer}: {e}");
                }
                Some((stream, peer))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => {
                log::error!("aa-tcp: accept: {e}");
                None
            }
        }
    }
}

fn set<T>(fd: i32, level: i32, name: i32, value: T) -> io::Result<()> {
    // SAFETY: a live socket, and a value of the size setsockopt is told.
    let r = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&raw const value).cast(),
            std::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// No delay on the small frames, keepalive and the user timeout.
pub fn tune(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let fd = stream.as_raw_fd();
    set(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1 as libc::c_int)?;
    set(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, KEEPALIVE_IDLE_S)?;
    set(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        KEEPALIVE_INTERVAL_S,
    )?;
    set(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, KEEPALIVE_COUNT)?;
    set(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_USER_TIMEOUT,
        USER_TIMEOUT_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn get(fd: i32, level: i32, name: i32) -> i32 {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: a live socket and a c_int to fill.
        let r = unsafe { libc::getsockopt(fd, level, name, (&raw mut value).cast(), &raw mut len) };
        assert_eq!(r, 0);
        value
    }

    #[test]
    fn a_loopback_connection_is_accepted_tuned_and_carries_bytes() {
        let l = Listener::bind(0).unwrap();
        let port = l.port();
        assert_ne!(port, 0);
        assert!(!l.poll(0), "nothing waits yet");
        assert!(l.accept().is_none());
        // Over IPv4: the listener is dual-stack.
        let mut phone = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(l.poll(1000));
        let (mut ours, peer) = l.accept().unwrap();
        assert!(peer.ip().is_loopback() || peer.ip().to_string().ends_with("127.0.0.1"));
        let fd = ours.as_raw_fd();
        assert_eq!(get(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
        assert_eq!(
            get(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE),
            KEEPALIVE_IDLE_S
        );
        assert_eq!(
            get(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL),
            KEEPALIVE_INTERVAL_S
        );
        assert_eq!(
            get(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT),
            KEEPALIVE_COUNT
        );
        assert_eq!(
            get(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT),
            USER_TIMEOUT_MS as i32
        );
        assert_eq!(get(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY), 1);
        phone.write_all(&[1, 2, 3]).unwrap();
        let mut got = [0u8; 3];
        ours.read_exact(&mut got).unwrap();
        assert_eq!(got, [1, 2, 3]);
        ours.write_all(&[9]).unwrap();
        let mut back = [0u8; 1];
        phone.read_exact(&mut back).unwrap();
        assert_eq!(back, [9]);
    }

    #[test]
    fn a_held_port_is_reported_after_the_retries() {
        let l = Listener::bind(0).unwrap();
        let start = std::time::Instant::now();
        let e = Listener::bind_retrying(l.port(), 2)
            .err()
            .expect("the port is held");
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
        assert!(start.elapsed() >= 2 * BIND_RETRY);
    }
}
