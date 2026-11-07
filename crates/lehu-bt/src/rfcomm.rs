//! RFCOMM services for the phones' wireless bootstraps: a profile (its
//! UUID, channel and SDP record) registered with bluetoothd, which binds
//! the channel, publishes the record and hands every accepted connection to
//! us as a descriptor. CarPlay registers its iAP2 and CarPlay services here;
//! Android Auto's wireless bootstrap will register its own UUID the same way.
//!
//! BlueZ's `Profile1` is used rather than our own RFCOMM sockets: binding a
//! fixed channel needs `CAP_NET_BIND_SERVICE`, and the SDP server lives in
//! bluetoothd, which registers records only for profiles it was asked for
//! over D-Bus. The unprivileged `lehu` user has exactly the D-Bus path.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use lehu_core::queue::Queue;

use crate::device::Address;
use crate::manager::{Command, Commands};

/// Connections accepted and not yet taken: a phone's reconnection may
/// overlap its old connection; a third is closed unread.
pub const ACCEPT_DEPTH: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// bluetoothd listens on `channel` and publishes `record`.
    Server,
    /// bluetoothd connects to the phone's service of this UUID when the
    /// phone connects, and the connection reaches us the same way.
    Client,
}

/// One RFCOMM service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RfcommProfile {
    pub uuid: String,
    /// What the SDP record calls the service; also BlueZ's `Name` option.
    pub name: String,
    pub role: Role,
    /// The server channel; unused for a client.
    pub channel: u8,
    /// The SDP record, as BlueZ's XML; `None` for a client.
    pub record: Option<String>,
}

impl RfcommProfile {
    /// A server on `channel` with the record `sdp_record` builds.
    pub fn server(uuid: &str, name: &str, channel: u8) -> Self {
        Self {
            uuid: uuid.to_string(),
            name: name.to_string(),
            role: Role::Server,
            channel,
            record: Some(sdp_record(uuid, channel, name)),
        }
    }

    pub fn client(uuid: &str, name: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            name: name.to_string(),
            role: Role::Client,
            channel: 0,
            record: None,
        }
    }
}

/// The SDP record bluetoothd publishes for a serial-port service on
/// `channel`: the service class `uuid`, L2CAP then RFCOMM in the protocol
/// descriptor list, the public browse group, every language, the serial
/// port profile 1.0 and the name. A Role-and-Channel registration alone is
/// invisible to the phone's SDP query on some bluetoothd versions, so the
/// record is given whole. Byte for byte what the C++ sent.
pub fn sdp_record(uuid: &str, channel: u8, name: &str) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>",
            "<record>",
            "  <attribute id=\"0x0001\"><sequence><uuid value=\"{uuid}\" /></sequence></attribute>",
            "  <attribute id=\"0x0004\"><sequence>",
            "    <sequence><uuid value=\"0x0100\" /></sequence>",
            "    <sequence><uuid value=\"0x0003\" /><uint8 value=\"0x{channel:02x}\" /></sequence>",
            "  </sequence></attribute>",
            "  <attribute id=\"0x0005\"><sequence><uuid value=\"0x1002\" /></sequence></attribute>",
            "  <attribute id=\"0x0008\"><uint8 value=\"0xff\" /></attribute>",
            "  <attribute id=\"0x0009\"><sequence>",
            "    <sequence><uuid value=\"0x1101\" /><uint16 value=\"0x0100\" /></sequence>",
            "  </sequence></attribute>",
            "  <attribute id=\"0x0100\"><text value=\"{name}\" /></attribute>",
            "</record>",
        ),
        uuid = uuid,
        channel = channel,
        name = xml_text(name),
    )
}

/// `s` with XML's five special characters escaped, so a name with `&` or
/// `<` cannot break the record.
fn xml_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// An accepted connection: the socket and who is on the other end.
#[derive(Debug)]
pub struct RfcommConnection {
    pub fd: OwnedFd,
    pub peer: Address,
}

impl RfcommConnection {
    /// The socket as a stream with a read timeout.
    pub fn into_stream(self, read_timeout: Option<Duration>) -> RfcommStream {
        RfcommStream {
            fd: self.fd,
            read_timeout,
        }
    }
}

/// A registered profile; its connections arrive here. Dropping it
/// unregisters the profile.
pub struct RfcommHandle {
    pub(crate) id: u32,
    pub(crate) accepted: Arc<Queue<RfcommConnection>>,
    pub(crate) commands: Arc<Commands>,
}

impl RfcommHandle {
    /// The next accepted connection, waiting up to `timeout` for one.
    pub fn accept(&self, timeout: Duration) -> Option<RfcommConnection> {
        self.accepted.pop_wait(timeout)
    }

    /// An accepted connection, if one waits.
    pub fn try_accept(&self) -> Option<RfcommConnection> {
        self.accepted.pop()
    }

    /// Accepted connections that found the queue full and were closed.
    pub fn refused(&self) -> u32 {
        self.accepted.drops()
    }
}

impl Drop for RfcommHandle {
    fn drop(&mut self) {
        self.commands.send(Command::Unregister(self.id));
    }
}

/// The RFCOMM socket as `Read + Write`. bluetoothd hands it over
/// non-blocking, so a bare read would return nothing with no data waiting:
/// reads poll first and report a quiet timeout as `TimedOut`, which the
/// iAP2 link takes as a quiet moment; writes ride out a full buffer by
/// polling for room, two seconds at most.
pub struct RfcommStream {
    fd: OwnedFd,
    read_timeout: Option<Duration>,
}

impl RfcommStream {
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    fn wait(&self, events: libc::c_short, timeout: Option<Duration>) -> io::Result<libc::c_short> {
        let mut p = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let ms = timeout.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as libc::c_int);
        loop {
            // SAFETY: one live pollfd.
            let n = unsafe { libc::poll(&mut p, 1, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "rfcomm: quiet"));
            }
            return Ok(p.revents);
        }
    }
}

impl Read for RfcommStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let revents = self.wait(libc::POLLIN, self.read_timeout)?;
        // SAFETY: reading into the caller's live buffer.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n > 0 {
            return Ok(n as usize);
        }
        if n == 0 || revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            // A hang-up with nothing left to read: the phone went.
            return Ok(0);
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            // Readable said poll, nothing said recv: a quiet moment.
            return Err(io::Error::new(io::ErrorKind::TimedOut, "rfcomm: quiet"));
        }
        Err(e)
    }
}

impl Write for RfcommStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            // SAFETY: sending the caller's live buffer; MSG_NOSIGNAL so a
            // mid-write hang-up does not raise SIGPIPE.
            let n = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    buf.as_ptr().cast(),
                    buf.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => {
                    let revents = self.wait(libc::POLLOUT, Some(Duration::from_secs(2)))?;
                    if revents & libc::POLLOUT == 0 {
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "rfcomm: hung up"));
                    }
                }
                _ => return Err(e),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// The record the C++ sent for the iAP2 server, verbatim.
    const IAP_RECORD: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>",
        "<record>",
        "  <attribute id=\"0x0001\"><sequence><uuid value=\"00000000-deca-fade-deca-deafdecacaff\" /></sequence></attribute>",
        "  <attribute id=\"0x0004\"><sequence>",
        "    <sequence><uuid value=\"0x0100\" /></sequence>",
        "    <sequence><uuid value=\"0x0003\" /><uint8 value=\"0x03\" /></sequence>",
        "  </sequence></attribute>",
        "  <attribute id=\"0x0005\"><sequence><uuid value=\"0x1002\" /></sequence></attribute>",
        "  <attribute id=\"0x0008\"><uint8 value=\"0xff\" /></attribute>",
        "  <attribute id=\"0x0009\"><sequence>",
        "    <sequence><uuid value=\"0x1101\" /><uint16 value=\"0x0100\" /></sequence>",
        "  </sequence></attribute>",
        "  <attribute id=\"0x0100\"><text value=\"Wireless iAP\" /></attribute>",
        "</record>",
    );

    #[test]
    fn the_record_matches_the_one_that_worked() {
        assert_eq!(
            sdp_record("00000000-deca-fade-deca-deafdecacaff", 3, "Wireless iAP"),
            IAP_RECORD
        );
        let cp = sdp_record("ec884348-cd41-40a2-9727-575d50bf1fd3", 4, "CarPlay");
        assert!(cp.contains("<uint8 value=\"0x04\" />"));
        assert!(cp.contains("<text value=\"CarPlay\" />"));
        assert!(sdp_record("x", 1, "A & B <c>").contains("A &amp; B &lt;c&gt;"));
        let p = RfcommProfile::server("u", "n", 5);
        assert_eq!(p.role, Role::Server);
        assert_eq!(p.channel, 5);
        assert!(p.record.is_some());
        let c = RfcommProfile::client("u", "n");
        assert_eq!(c.role, Role::Client);
        assert!(c.record.is_none());
    }

    #[test]
    fn the_stream_reads_with_a_timeout_and_sees_the_end() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a live pair for the socketpair to fill.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: fresh descriptors we own.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut ours = RfcommConnection {
            fd: a,
            peer: Address::default(),
        }
        .into_stream(Some(Duration::from_millis(20)));
        let mut theirs = RfcommConnection {
            fd: b,
            peer: Address::default(),
        }
        .into_stream(None);
        let mut buf = [0u8; 16];
        assert_eq!(
            ours.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        theirs.write_all(b"hello").unwrap();
        assert_eq!(ours.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
        drop(theirs);
        assert_eq!(
            ours.read(&mut buf).unwrap(),
            0,
            "a hang-up reads as the end"
        );
        assert!(ours.write_all(b"x").is_err());
    }
}
