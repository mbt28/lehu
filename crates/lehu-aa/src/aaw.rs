//! The wireless setup conversation a phone holds with the head unit over
//! Bluetooth before it joins the access point: short frames of
//! `[len u16BE][id u16BE][protobuf]` on the Android Auto RFCOMM service.
//! The head unit opens with its version and the access point's frequency,
//! the phone answers with its identity, the head unit names the TCP
//! endpoint the phone is to connect to (WifiStartRequest), the phone asks
//! for the network (WifiInfoRequest) and is handed the credentials
//! (WifiInfoResponse); it then joins the access point and connects to the
//! endpoint, reporting how the join went (WifiConnectionStatus) and pinging
//! the link meanwhile.
//!
//! The ids and field numbers come from LIVI's wireless setup definitions
//! (aap_protobuf/aaw, later wireless_setup.proto; Lasse Heitgres, GPL-3.0;
//! see NOTICE), vendored by FastCarPlay; aasdk has none. The bytes of the
//! three messages the head unit sends are what FastCarPlay sent and phones
//! accepted. Builders encode into a caller's buffer, which keeps its
//! capacity across frames.

use prost::Message;

/// The RFCOMM service the phone looks for: its UUID, the channel the C++
/// bound and the name in the record.
pub const UUID: &str = "4de17a00-52cb-11e6-bdf4-0800200c9a66";
pub const CHANNEL: u8 = 8;
pub const SERVICE_NAME: &str = "Android Auto Wireless";

// --- Message ids ---
pub const MSG_START_REQUEST: u16 = 1;
pub const MSG_INFO_REQUEST: u16 = 2;
pub const MSG_INFO_RESPONSE: u16 = 3;
pub const MSG_VERSION_REQUEST: u16 = 4;
pub const MSG_VERSION_RESPONSE: u16 = 5;
pub const MSG_CONNECTION_STATUS: u16 = 6;
pub const MSG_START_RESPONSE: u16 = 7;
pub const MSG_PING: u16 = 8;
pub const MSG_PONG: u16 = 9;

/// The wireless projection protocol version the C++ announced.
pub const VERSION_MAJOR: u32 = 6;
pub const VERSION_MINOR: u32 = 0;

/// WifiSecurityMode and AccessPointType values, in the phone's numbering.
pub const SECURITY_WPA2_PERSONAL: i32 = 8;
pub const ACCESS_POINT_STATIC: i32 = 0;

/// The header: a body length and a message id, both big-endian u16.
pub const HEADER_LEN: usize = 4;
/// The longest body either side sends: the credentials with a 32-byte
/// name and a 63-byte passphrase are under 128 bytes, the phone's version
/// response with its identity a little more. An announced body over this
/// is not this protocol, and the link is closed.
pub const MAX_BODY: usize = 1024;

// --- Bodies ---

/// The phone's identity in its version response.
#[derive(Clone, PartialEq, Message)]
pub struct MobileDeviceIdentity {
    #[prost(string, optional, tag = "1")]
    pub mobile_device_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub connectivity_lifetime_id: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub bluetooth_name: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub bluetooth_address: Option<String>,
}

/// Our opening: the version and the access point's frequency, so the
/// phone knows which band to look on.
#[derive(Clone, PartialEq, Message)]
pub struct WifiVersionRequest {
    #[prost(uint32, required, tag = "1")]
    pub major_version: u32,
    #[prost(uint32, required, tag = "2")]
    pub minor_version: u32,
    #[prost(uint32, repeated, packed = "true", tag = "4")]
    pub supported_channel_frequencies_mhz: Vec<u32>,
}

/// The phone's answer: who it is, and whether it can speak our version.
#[derive(Clone, PartialEq, Message)]
pub struct WifiVersionResponse {
    #[prost(uint32, optional, tag = "1")]
    pub major_version: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub minor_version: Option<u32>,
    #[prost(string, optional, tag = "3")]
    pub device_serial: Option<String>,
    #[prost(int32, optional, tag = "4")]
    pub status: Option<i32>,
    #[prost(message, optional, tag = "6")]
    pub mobile_device_identity: Option<MobileDeviceIdentity>,
}

/// Where the phone connects once on the network.
#[derive(Clone, PartialEq, Message)]
pub struct WifiStartRequest {
    #[prost(string, required, tag = "1")]
    pub ip_address: String,
    #[prost(uint32, required, tag = "2")]
    pub port: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WifiStartResponse {
    #[prost(string, optional, tag = "1")]
    pub ip_address: Option<String>,
    #[prost(uint32, optional, tag = "2")]
    pub port: Option<u32>,
    #[prost(int32, optional, tag = "3")]
    pub status: Option<i32>,
}

/// The network the phone joins.
#[derive(Clone, PartialEq, Message)]
pub struct WifiInfoResponse {
    #[prost(string, required, tag = "1")]
    pub ssid: String,
    #[prost(string, required, tag = "2")]
    pub password: String,
    #[prost(string, required, tag = "3")]
    pub bssid: String,
    #[prost(int32, required, tag = "4")]
    pub security_mode: i32,
    #[prost(int32, optional, tag = "5")]
    pub access_point_type: Option<i32>,
}

/// How the join went: 0 is success, the negatives name the failure.
#[derive(Clone, PartialEq, Message)]
pub struct WifiConnectionStatus {
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub error_message: Option<String>,
}

/// The phone's status codes, named for the log.
pub fn status_name(status: i32) -> &'static str {
    match status {
        0 => "success",
        1 => "unsolicited message",
        -1 => "no compatible version",
        -2 => "inaccessible Wi-Fi channel",
        -3 => "incorrect credentials",
        -4 => "projection already started",
        -5 => "Wi-Fi disabled",
        -6 => "Wi-Fi not yet started",
        -7 => "invalid host",
        -8 => "no supported Wi-Fi channels",
        -9 => "check the phone",
        -10 => "the phone's Wi-Fi is disabled",
        -11 => "network unavailable",
        _ => "unknown status",
    }
}

/// A Wi-Fi channel's centre frequency in MHz, as the C++ computed it: the
/// 2.4 GHz plan for 1 to 14, the 5 GHz plan from 36 up. A channel outside
/// both (0: hostapd picks one) is told as channel 6: the phone goes by the
/// band, and this radio has only the one.
pub fn channel_mhz(channel: u32) -> u32 {
    match channel {
        1..=13 => 2412 + (channel - 1) * 5,
        14 => 2484,
        36..=196 => 5180 + (channel - 36) * 5,
        _ => 2437,
    }
}

/// What the phone is handed: the network and our endpoint on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    pub passphrase: String,
    /// The access point's MAC, as sysfs spells it.
    pub bssid: String,
    pub channel: u32,
    /// Our IPv4 on the access point; the phone connects to `port` on it.
    pub ip: String,
    pub port: u16,
}

impl Network {
    pub fn version_request(&self) -> WifiVersionRequest {
        WifiVersionRequest {
            major_version: VERSION_MAJOR,
            minor_version: VERSION_MINOR,
            supported_channel_frequencies_mhz: vec![channel_mhz(self.channel)],
        }
    }

    pub fn start_request(&self) -> WifiStartRequest {
        WifiStartRequest {
            ip_address: self.ip.clone(),
            port: u32::from(self.port),
        }
    }

    pub fn info_response(&self) -> WifiInfoResponse {
        WifiInfoResponse {
            ssid: self.ssid.clone(),
            password: self.passphrase.clone(),
            bssid: self.bssid.clone(),
            security_mode: SECURITY_WPA2_PERSONAL,
            access_point_type: Some(ACCESS_POINT_STATIC),
        }
    }
}

/// A body did not fit a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooBig;

/// `out` becomes the frame carrying `body` under `id`.
pub fn frame_into(out: &mut Vec<u8>, id: u16, body: &[u8]) -> Result<(), TooBig> {
    if body.len() > MAX_BODY {
        return Err(TooBig);
    }
    out.clear();
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

/// `out` becomes the frame carrying `m` encoded under `id`.
pub fn encode_frame(out: &mut Vec<u8>, id: u16, m: &impl Message) -> Result<(), TooBig> {
    let len = m.encoded_len();
    if len > MAX_BODY {
        return Err(TooBig);
    }
    out.clear();
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    // prost grows a Vec rather than fail; the length was checked above.
    m.encode(out).map_err(|_| TooBig)
}

/// Why a header cannot be followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadFrame {
    /// The announced body is longer than this protocol's messages.
    TooLong(u16),
}

impl std::fmt::Display for BadFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BadFrame::TooLong(n) => write!(f, "a body of {n} bytes announced"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub len: u16,
    pub id: u16,
}

/// The four bytes before a body.
pub fn parse_head(bytes: [u8; HEADER_LEN]) -> Result<Head, BadFrame> {
    let len = u16::from_be_bytes([bytes[0], bytes[1]]);
    if usize::from(len) > MAX_BODY {
        return Err(BadFrame::TooLong(len));
    }
    Ok(Head {
        len,
        id: u16::from_be_bytes([bytes[2], bytes[3]]),
    })
}

/// Decode a body; `None` when it is not a valid message of that shape.
pub fn decode<M: Message + Default>(body: &[u8]) -> Option<M> {
    M::decode(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn network() -> Network {
        Network {
            ssid: "LEHU".into(),
            passphrase: "lehu1234".into(),
            bssid: "aa:bb:cc:dd:ee:ff".into(),
            channel: 6,
            ip: "192.168.53.1".into(),
            port: 5277,
        }
    }

    /// The frames FastCarPlay's builders produced for these inputs.
    #[test]
    fn the_three_frames_match_the_bytes_the_phones_took() {
        let n = network();
        let mut out = Vec::new();
        encode_frame(&mut out, MSG_VERSION_REQUEST, &n.version_request()).unwrap();
        // len 8, id 4; major 6, minor 0, packed field 4: 2437 = 0x985 -> 0x85 0x13
        assert_eq!(
            out,
            [0, 8, 0, 4, 0x08, 0x06, 0x10, 0x00, 0x22, 0x02, 0x85, 0x13]
        );
        encode_frame(&mut out, MSG_START_REQUEST, &n.start_request()).unwrap();
        let mut want = vec![0, 17, 0, 1, 0x0a, 12];
        want.extend_from_slice(b"192.168.53.1");
        want.extend_from_slice(&[0x10, 0x9d, 0x29]); // 5277 = 0x149d
        assert_eq!(out, want);
        encode_frame(&mut out, MSG_INFO_RESPONSE, &n.info_response()).unwrap();
        let mut want = vec![0, 39, 0, 3, 0x0a, 4];
        want.extend_from_slice(b"LEHU");
        want.extend_from_slice(&[0x12, 8]);
        want.extend_from_slice(b"lehu1234");
        want.extend_from_slice(&[0x1a, 17]);
        want.extend_from_slice(b"aa:bb:cc:dd:ee:ff");
        want.extend_from_slice(&[0x20, 0x08, 0x28, 0x00]);
        assert_eq!(out, want);
        // The buffer keeps its capacity across frames.
        let cap = out.capacity();
        frame_into(&mut out, MSG_PONG, &[1, 2, 3]).unwrap();
        assert_eq!(out, [0, 3, 0, 9, 1, 2, 3]);
        assert_eq!(out.capacity(), cap);
    }

    #[test]
    fn the_phones_messages_decode_and_the_heads_are_checked() {
        let r: WifiVersionResponse = decode(&[
            0x08, 0x06, 0x10, 0x00, 0x1a, 0x03, b'S', b'N', b'1', 0x20, 0x00, 0x32, 0x04, 0x0a,
            0x02, b'i', b'd',
        ])
        .unwrap();
        assert_eq!(r.device_serial.as_deref(), Some("SN1"));
        assert_eq!(r.status, Some(0));
        assert_eq!(
            r.mobile_device_identity
                .unwrap()
                .mobile_device_id
                .as_deref(),
            Some("id")
        );
        // A negative status is a ten-byte varint.
        let s: WifiConnectionStatus = decode(&[
            0x08, 0xfd, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ])
        .unwrap();
        assert_eq!(s.status, Some(-3));
        assert_eq!(status_name(-3), "incorrect credentials");
        assert_eq!(status_name(0), "success");
        assert_eq!(status_name(-4), "projection already started");
        assert_eq!(status_name(7), "unknown status");
        let st: WifiStartResponse = decode(&[0x18, 0x00]).unwrap();
        assert_eq!(st.status, Some(0));
        assert!(
            decode::<WifiVersionResponse>(&[0x08]).is_none(),
            "a torn varint"
        );
        assert_eq!(
            parse_head([0, 5, 0, 8]),
            Ok(Head {
                len: 5,
                id: MSG_PING
            })
        );
        assert_eq!(parse_head([0x04, 0x01, 0, 8]), Err(BadFrame::TooLong(1025)));
        assert_eq!(parse_head([0x04, 0x00, 0, 8]).map(|h| h.len), Ok(1024));
        assert_eq!(
            frame_into(&mut Vec::new(), 1, &[0; MAX_BODY + 1]),
            Err(TooBig)
        );
    }

    #[test]
    fn channels_map_to_their_frequencies() {
        assert_eq!(channel_mhz(1), 2412);
        assert_eq!(channel_mhz(6), 2437);
        assert_eq!(channel_mhz(13), 2472);
        assert_eq!(channel_mhz(14), 2484);
        assert_eq!(channel_mhz(36), 5180);
        assert_eq!(channel_mhz(40), 5200);
        assert_eq!(channel_mhz(0), 2437);
        assert_eq!(channel_mhz(20), 2437);
    }
}
