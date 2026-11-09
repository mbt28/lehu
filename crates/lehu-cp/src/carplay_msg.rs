// SPDX-License-Identifier: GPL-3.0-or-later
//! The control-session messages of the CarPlay handshakes: the
//! identification the phone asks for (a USB host transport when wired, a
//! Bluetooth and a wireless CarPlay transport when not), the coprocessor's
//! certificate and challenge response, and CarPlayStartSession, which hands
//! the phone where the control server listens: its link-local address on
//! the USB network, or the access point's name, passphrase and channel
//! and our address on it.
//!
//! Parameter values: integers big-endian, strings UTF-8 with a trailing
//! NUL, booleans one byte, a group's value its own parameter body.
//!
//! The message ids and the identification follow LIVI's
//! iap2/control_session_message/*.py and bt/cp_handler.py
//! build_identification (Lasse Heitgres, GPL-3.0), through FastCarPlay's
//! cp_carplay_msg.cpp; changed for LEHU.

use crate::iap2::{pack_csm, pack_params, parse_csm};
use crate::media::{
    MSG_NOW_PLAYING_UPDATE, MSG_ROUTE_GUIDANCE_MANEUVER, MSG_ROUTE_GUIDANCE_UPDATE,
    MSG_START_NOW_PLAYING, MSG_START_ROUTE_GUIDANCE, MSG_STOP_NOW_PLAYING, MSG_STOP_ROUTE_GUIDANCE,
};
use crate::ultra::Theme;

pub const MSG_START_IDENTIFICATION: u16 = 0x1d00;
pub const MSG_IDENTIFICATION_INFORMATION: u16 = 0x1d01;
pub const MSG_IDENTIFICATION_ACCEPTED: u16 = 0x1d02;
pub const MSG_IDENTIFICATION_REJECTED: u16 = 0x1d03;
pub const MSG_REQUEST_AUTH_CERTIFICATE: u16 = 0xaa00;
pub const MSG_AUTH_CERTIFICATE: u16 = 0xaa01;
pub const MSG_REQUEST_AUTH_CHALLENGE_RESPONSE: u16 = 0xaa02;
pub const MSG_AUTH_RESPONSE: u16 = 0xaa03;
pub const MSG_AUTH_FAILED: u16 = 0xaa04;
pub const MSG_AUTH_SUCCEEDED: u16 = 0xaa05;
pub const MSG_CARPLAY_AVAILABILITY: u16 = 0x4300;
pub const MSG_CARPLAY_START_SESSION: u16 = 0x4301;
pub const MSG_DEVICE_TIME_UPDATE: u16 = 0x4e0b;
pub const MSG_WIRELESS_CARPLAY_UPDATE: u16 = 0x4e0d;
pub const MSG_DEVICE_TRANSPORT_ID: u16 = 0x4e0e;
pub const MSG_REQUEST_ACCESSORY_WIFI_CONFIG: u16 = 0x5702;
pub const MSG_ACCESSORY_WIFI_CONFIG: u16 = 0x5703;

fn str_param(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b
}

/// Message ids packed as a list of big-endian u16.
fn id_list(ids: &[u16]) -> Vec<u8> {
    ids.iter().flat_map(|id| id.to_be_bytes()).collect()
}

/// What an identification declares beyond what its path needs to start:
/// the now playing messages, and route guidance with the display
/// component the phone wants for it. The full set is offered first; a
/// phone that rejects it is offered what the path declared before
/// (`lean`), which phones accept, so a refusal costs the features, never
/// the session. The album art is asked for with the full set only: it
/// needs nothing of the identification (its file transfer session is the
/// link's), but a phone that turned the full set down gets nothing new.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declared {
    pub now_playing: bool,
    pub guidance: bool,
    pub artwork: bool,
}

impl Declared {
    pub const FULL: Declared = Declared {
        now_playing: true,
        guidance: true,
        artwork: true,
    };

    /// The set before route guidance: the wired one always declared now
    /// playing, the wireless one only the handoff.
    pub fn lean(wired: bool) -> Self {
        Self {
            now_playing: wired,
            guidance: false,
            artwork: false,
        }
    }
}

/// The sent and received ids of `base` with what `extra` adds.
fn with_features(sent: &[u16], received: &[u16], extra: Declared) -> (Vec<u8>, Vec<u8>) {
    let mut s = sent.to_vec();
    let mut r = received.to_vec();
    let add = |list: &mut Vec<u16>, ids: &[u16]| {
        for id in ids {
            if !list.contains(id) {
                list.push(*id);
            }
        }
    };
    if extra.now_playing {
        add(&mut s, &[MSG_START_NOW_PLAYING, MSG_STOP_NOW_PLAYING]);
        add(&mut r, &[MSG_NOW_PLAYING_UPDATE]);
    }
    if extra.guidance {
        add(&mut s, &[MSG_START_ROUTE_GUIDANCE, MSG_STOP_ROUTE_GUIDANCE]);
        add(
            &mut r,
            &[MSG_ROUTE_GUIDANCE_UPDATE, MSG_ROUTE_GUIDANCE_MANEUVER],
        );
    }
    (id_list(&s), id_list(&r))
}

/// The identification's field for the route guidance display component.
pub const ROUTE_GUIDANCE_COMPONENT: u16 = 30;

/// The RouteGuidanceDisplayComponent: id 0 (StartRouteGuidanceUpdates
/// without an id means it), our name, and the longest texts we take. A
/// component without the lengths gets no guidance (seen with phones); a storage
/// capacity of 0 keeps the phone from sending the whole manoeuvre list.
fn route_guidance_component(name: &str) -> Vec<u8> {
    let len = 128u16.to_be_bytes();
    pack_params(&[
        (0, &0u16.to_be_bytes()),
        (1, &str_param(name)),
        (2, &len),
        (3, &len),
        (4, &len),
        (5, &len),
        (6, &0u16.to_be_bytes()),
    ])
}

/// The messages a wired accessory declares. This is the set a phone
/// accepts at the first try: none needs a transport component beyond the
/// USB host transport (declaring vehicle or navigation messages without
/// their components makes the phone reject the identification). Route
/// guidance is added with its component (`Declared`).
const WIRED_SENT: &[u16] = &[
    0x5703, 0x5000, 0x5002, 0xae00, 0xae02, 0x4157, 0x4159, 0x4154, 0x4156, 0xae03, 0x4301,
];
const WIRED_RECEIVED: &[u16] = &[
    0xea00, 0xea01, 0x4e0b, 0x4e0d, 0x4e0e, 0x5702, 0x5001, 0xae01, 0x4158, 0x4155, 0x4300,
];

/// The wired identification with everything declared, as first offered.
pub fn wired_identification(name: &str, model: &str, manufacturer: &str) -> Vec<u8> {
    wired_identification_for(name, model, manufacturer, Declared::FULL)
}

/// IdentificationInformation for the wired path: a USBHostTransport
/// component naming the NCM interface (3) as the CarPlay interface, power
/// providing advanced, and the message sets above with what `extra`
/// declares.
pub fn wired_identification_for(
    name: &str,
    model: &str,
    manufacturer: &str,
    extra: Declared,
) -> Vec<u8> {
    wired_identification_themed(name, model, manufacturer, extra, None)
}

/// The wired identification, the transport component naming the
/// CarPlay Ultra theme's asset when there is one.
pub fn wired_identification_themed(
    name: &str,
    model: &str,
    manufacturer: &str,
    extra: Declared,
    theme_asset: Option<&str>,
) -> Vec<u8> {
    let (id, kind, asset) = (
        0u16.to_be_bytes(),
        str_param("USBHostTransport"),
        theme_asset.map(str_param),
    );
    let mut usb_host: Vec<(u16, &[u8])> = vec![(0, &id), (1, &kind), (2, &[]), (3, &[3]), (4, &[])];
    if let Some(asset) = &asset {
        usb_host.push((THEME_ASSET_PARAM, asset));
    }
    let usb_host = pack_params(&usb_host);
    let ea_protocol = pack_params(&[(0, &[1]), (1, &str_param("en.opencarplay.test")), (2, &[0])]);
    let languages = [str_param("en"), str_param("de")].concat();
    let (sent, received) = with_features(WIRED_SENT, WIRED_RECEIVED, extra);
    let guidance = route_guidance_component(name);
    let (name, model, manufacturer) = (str_param(name), str_param(model), str_param(manufacturer));
    let (serial, firmware, hardware, language) = (
        str_param("0123456"),
        str_param("1.0.0"),
        str_param("1.0"),
        str_param("en"),
    );
    let current = 20u16.to_be_bytes();
    let mut params: Vec<(u16, &[u8])> = vec![
        (0, &name),
        (1, &model),
        (2, &manufacturer),
        (3, &serial),
        (4, &firmware),
        (5, &hardware),
        (6, &sent),
        (7, &received),
        (8, &[2]),
        (9, &current),
        (10, &ea_protocol),
        (12, &language),
        (13, &languages),
        (16, &usb_host),
    ];
    if extra.guidance {
        params.push((ROUTE_GUIDANCE_COMPONENT, &guidance));
    }
    pack_csm(MSG_IDENTIFICATION_INFORMATION, &params)
}

pub fn auth_certificate(cert: &[u8]) -> Vec<u8> {
    pack_csm(MSG_AUTH_CERTIFICATE, &[(0, cert)])
}

pub fn auth_response(signature: &[u8]) -> Vec<u8> {
    pack_csm(MSG_AUTH_RESPONSE, &[(0, signature)])
}

/// The challenge inside a RequestAuthenticationChallengeResponse.
/// The phone's time, as it tells it once it knows we take the message:
/// the field is named "seconds since the reference date", and an iPhone
/// on iOS 26 counts them from the Unix epoch (checked against a phone on
/// the day); the zone's offset from UTC and the daylight-saving offset
/// follow, both in minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceTime {
    pub unix_secs: i64,
    pub zone_offset_min: i16,
    pub dst_offset_min: i8,
}

impl DeviceTime {
    /// Milliseconds since the Unix epoch, UTC. The parser keeps the
    /// seconds within 2000..2100, so this cannot overflow.
    pub fn unix_ms(&self) -> i64 {
        self.unix_secs.saturating_mul(1000)
    }
}

pub fn device_time_update(csm: &[u8]) -> Option<DeviceTime> {
    let (id, params) = parse_csm(csm)?;
    if id != MSG_DEVICE_TIME_UPDATE {
        return None;
    }
    let mut t = DeviceTime {
        unix_secs: 0,
        zone_offset_min: 0,
        dst_offset_min: 0,
    };
    let mut seen = false;
    for (k, v) in &params {
        match (*k, v.len()) {
            (0, 8) => {
                t.unix_secs = i64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]);
                seen = true;
            }
            (1, 2) => t.zone_offset_min = i16::from_be_bytes([v[0], v[1]]),
            (2, 1) => t.dst_offset_min = v[0] as i8,
            _ => {}
        }
    }
    // A clock outside this century is a broken message, not a time to
    // set the head unit's clock to.
    const Y2000: i64 = 946_684_800;
    const Y2100: i64 = 4_102_444_800;
    (seen && (Y2000..Y2100).contains(&t.unix_secs)).then_some(t)
}

pub fn auth_challenge(csm: &[u8]) -> Option<Vec<u8>> {
    let (id, params) = parse_csm(csm)?;
    if id != MSG_REQUEST_AUTH_CHALLENGE_RESPONSE {
        return None;
    }
    params
        .iter()
        .find(|(k, _)| *k == 0)
        .map(|(_, v)| v.to_vec())
}

/// Everything the phone needs to reach the control server over USB.
pub struct WiredSession<'a> {
    /// Our link-local IPv6 on the NCM interface.
    pub ip: &'a str,
    pub port: u32,
    /// The pairing id the phone stores us under.
    pub device_id: &'a str,
    /// Our Ed25519 public key, hex.
    pub public_key: &'a str,
    pub source_version: &'a str,
    pub ultra: UltraStart<'a>,
}

/// The transport components' parameter naming a CarPlay Ultra theme.
const THEME_ASSET_PARAM: u16 = 6;

/// What CarPlay Ultra adds to the start: the head unit takes the phone's
/// MFi 4 mutual authentication (an MFi 3.0 chip answers it), and the
/// theme the phone is to load, by its catalog ids. Nothing, for classic.
#[derive(Clone, Copy, Debug, Default)]
pub struct UltraStart<'a> {
    pub mutual_auth: bool,
    pub theme: Option<&'a Theme>,
}

impl UltraStart<'_> {
    /// The start's parameters: the theme's SDK and asset (identifier and
    /// version), and the mutual authentication flag.
    fn params(&self) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        if let Some(t) = self.theme {
            out.push((6, str_param(&t.sdk)));
            let asset = pack_params(&[(0, &str_param(&t.asset)), (1, &t.version.to_be_bytes())]);
            out.push((7, asset));
        }
        if self.mutual_auth {
            out.push((8, vec![1]));
        }
        out
    }
}

/// A start message: the common parameters, then what Ultra adds.
fn start_session(params: &[(u16, &[u8])], ultra: &UltraStart) -> Vec<u8> {
    let extra = ultra.params();
    let mut all = params.to_vec();
    all.extend(extra.iter().map(|(k, v)| (*k, v.as_slice())));
    pack_csm(MSG_CARPLAY_START_SESSION, &all)
}

/// CarPlayStartSession with wired attributes.
pub fn wired_start_session(s: &WiredSession) -> Vec<u8> {
    let wired = pack_params(&[(0, &str_param(s.ip))]);
    start_session(
        &[
            (0, &wired),
            (2, &s.port.to_be_bytes()),
            (3, &str_param(s.device_id)),
            (4, &str_param(s.public_key)),
            (5, &str_param(s.source_version)),
        ],
        &s.ultra,
    )
}

// ── The wireless path ─────────────────────────────────────────────────────

/// The messages a wireless accessory declares: the phone validates them
/// against the transport components, so only what the Wi-Fi handoff needs
/// is declared; now playing and route guidance are added (`Declared`).
const WIRELESS_SENT: &[u16] = &[MSG_ACCESSORY_WIFI_CONFIG, MSG_CARPLAY_START_SESSION];
const WIRELESS_RECEIVED: &[u16] = &[
    MSG_DEVICE_TIME_UPDATE,
    MSG_REQUEST_ACCESSORY_WIFI_CONFIG,
    MSG_CARPLAY_AVAILABILITY,
    MSG_WIRELESS_CARPLAY_UPDATE,
    MSG_DEVICE_TRANSPORT_ID,
];

/// How the access point is secured, as iAP2 numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WifiSecurity {
    None = 0,
    Wep = 1,
    WpaWpa2 = 2,
    Wpa3Transition = 3,
    Wpa3Only = 4,
}

/// The wireless identification with everything declared, as first offered.
pub fn wireless_identification(
    name: &str,
    model: &str,
    manufacturer: &str,
    bt_mac: &[u8; 6],
) -> Vec<u8> {
    wireless_identification_for(name, model, manufacturer, bt_mac, Declared::FULL)
}

/// IdentificationInformation for the wireless path: a Bluetooth transport
/// component keyed to the adapter's address (the phone matches transports
/// by it) and the wireless CarPlay transport component that makes the phone
/// offer wireless CarPlay, no power providing, one language.
pub fn wireless_identification_for(
    name: &str,
    model: &str,
    manufacturer: &str,
    bt_mac: &[u8; 6],
    extra: Declared,
) -> Vec<u8> {
    wireless_identification_themed(name, model, manufacturer, bt_mac, extra, None)
}

/// The wireless identification, the wireless CarPlay transport naming
/// the CarPlay Ultra theme's asset when there is one.
pub fn wireless_identification_themed(
    name: &str,
    model: &str,
    manufacturer: &str,
    bt_mac: &[u8; 6],
    extra: Declared,
    theme_asset: Option<&str>,
) -> Vec<u8> {
    let bluetooth = pack_params(&[
        (0, &1u16.to_be_bytes()),
        (1, &str_param("Bluetooth")),
        (2, &[]),
        (3, bt_mac),
    ]);
    let (id, kind, asset) = (
        2u16.to_be_bytes(),
        str_param("CarPlay"),
        theme_asset.map(str_param),
    );
    let mut wireless: Vec<(u16, &[u8])> = vec![(0, &id), (1, &kind), (2, &[]), (4, &[])];
    if let Some(asset) = &asset {
        wireless.push((THEME_ASSET_PARAM, asset));
    }
    let wireless = pack_params(&wireless);
    let (sent, received) = with_features(WIRELESS_SENT, WIRELESS_RECEIVED, extra);
    let guidance = route_guidance_component(name);
    let (name, model, manufacturer) = (str_param(name), str_param(model), str_param(manufacturer));
    let (serial, version, language) = (
        str_param("0000000000000000"),
        str_param("1.0"),
        str_param("en"),
    );
    let current = 0u16.to_be_bytes();
    let mut params: Vec<(u16, &[u8])> = vec![
        (0, &name),
        (1, &model),
        (2, &manufacturer),
        (3, &serial),
        (4, &version),
        (5, &version),
        (6, &sent),
        (7, &received),
        (8, &[0]),
        (9, &current),
        (12, &language),
        (13, &language),
        (17, &bluetooth),
        (24, &wireless),
    ];
    if extra.guidance {
        params.push((ROUTE_GUIDANCE_COMPONENT, &guidance));
    }
    pack_csm(MSG_IDENTIFICATION_INFORMATION, &params)
}

/// Everything the phone needs to join the access point and reach the
/// control server over it.
pub struct WirelessSession<'a> {
    pub ssid: &'a str,
    pub passphrase: &'a str,
    pub channel: u8,
    /// Our address on the access point: the interface's link-local IPv6
    /// when it has one, else its IPv4.
    pub ip: &'a str,
    pub security: WifiSecurity,
    pub port: u32,
    /// The access point interface's MAC, or the pairing-derived id.
    pub device_id: &'a str,
    /// Our Ed25519 public key, hex.
    pub public_key: &'a str,
    pub source_version: &'a str,
    pub ultra: UltraStart<'a>,
}

/// CarPlayStartSession with wireless attributes.
pub fn wireless_start_session(s: &WirelessSession) -> Vec<u8> {
    let wireless = pack_params(&[
        (0, &str_param(s.ssid)),
        (1, &str_param(s.passphrase)),
        (2, &[s.channel]),
        (3, &str_param(s.ip)),
        (4, &[s.security as u8]),
    ]);
    start_session(
        &[
            (1, &wireless),
            (2, &s.port.to_be_bytes()),
            (3, &str_param(s.device_id)),
            (4, &str_param(s.public_key)),
            (5, &str_param(s.source_version)),
        ],
        &s.ultra,
    )
}

/// AccessoryWiFiConfigurationInformation, the answer to the phone's
/// RequestAccessoryWiFiConfigurationInformation: the other way the phone
/// learns the credentials.
pub fn accessory_wifi_config(
    ssid: &str,
    passphrase: &str,
    security: WifiSecurity,
    channel: u8,
) -> Vec<u8> {
    pack_csm(
        MSG_ACCESSORY_WIFI_CONFIG,
        &[
            (1, &str_param(ssid)),
            (2, &str_param(passphrase)),
            (3, &[security as u8]),
            (4, &[channel]),
        ],
    )
}

/// CarPlayAvailability as the phone sends it for a wireless transport:
/// wireless attributes with available and the Bluetooth transport id. Built
/// here for the tests of the bootstrap.
pub fn carplay_availability(bt_transport_id: &str) -> Vec<u8> {
    let wireless = pack_params(&[(0, &[1]), (1, &str_param(bt_transport_id))]);
    pack_csm(MSG_CARPLAY_AVAILABILITY, &[(1, &wireless)])
}

#[cfg(test)]
mod wireless_tests {
    use super::*;
    use crate::iap2::parse_params;

    #[test]
    fn the_time_update_parses_as_the_phone_sent_it() {
        // 2771229404 s and UTC+60 min, as an iPhone sent on 2026-10-25.
        let secs: i64 = 2_771_229_404;
        let csm = pack_csm(
            MSG_DEVICE_TIME_UPDATE,
            &[
                (0, &secs.to_be_bytes()),
                (1, &60i16.to_be_bytes()),
                (2, &[0]),
            ],
        );
        let t = device_time_update(&csm).unwrap();
        assert_eq!(t.unix_ms(), secs * 1000);
        assert_eq!((t.zone_offset_min, t.dst_offset_min), (60, 0));
        let short = pack_csm(MSG_DEVICE_TIME_UPDATE, &[(1, &60i16.to_be_bytes())]);
        assert!(device_time_update(&short).is_none(), "no seconds, no time");
        assert!(device_time_update(&carplay_availability("x")).is_none());
    }

    #[test]
    fn the_identification_carries_both_transports() {
        let csm = wireless_identification_for(
            "LEHU",
            "LEHU1,1",
            "LEHU",
            &[1, 2, 3, 4, 5, 6],
            Declared::lean(false),
        );
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, MSG_IDENTIFICATION_INFORMATION);
        let find = |k: u16| params.iter().find(|(p, _)| *p == k).map(|(_, v)| *v);
        assert_eq!(find(6).unwrap(), &[0x57, 0x03, 0x43, 0x01]);
        assert_eq!(
            find(7).unwrap(),
            &[0x4e, 0x0b, 0x57, 0x02, 0x43, 0x00, 0x4e, 0x0d, 0x4e, 0x0e]
        );
        assert_eq!(find(8).unwrap(), &[0]);
        assert_eq!(find(16), None, "no USB host transport");
        let bt = parse_params(find(17).unwrap()).unwrap();
        assert_eq!(bt[0], (0, &[0, 1][..]));
        assert_eq!(bt[1], (1, &b"Bluetooth\0"[..]));
        assert_eq!(bt[2], (2, &[][..]));
        assert_eq!(bt[3], (3, &[1, 2, 3, 4, 5, 6][..]));
        let wcp = parse_params(find(24).unwrap()).unwrap();
        assert_eq!(wcp[0], (0, &[0, 2][..]));
        assert_eq!(wcp[1], (1, &b"CarPlay\0"[..]));
        assert_eq!(wcp[3], (4, &[][..]));
    }

    #[test]
    fn the_start_session_bytes() {
        let csm = wireless_start_session(&WirelessSession {
            ssid: "LEHU",
            passphrase: "lehu1234",
            channel: 6,
            ip: "fe80::1",
            security: WifiSecurity::WpaWpa2,
            port: 7000,
            device_id: "AA:BB:CC:DD:EE:FF",
            public_key: "ab",
            source_version: "280.33.8",
            ultra: UltraStart::default(),
        });
        // The whole message, as the phone sees it.
        let mut expect = vec![0x40, 0x40, 0x00, 0x00, 0x43, 0x01];
        let wireless = [
            0x00, 0x09, 0x00, 0x00, b'L', b'E', b'H', b'U', 0x00, // ssid
            0x00, 0x0d, 0x00, 0x01, b'l', b'e', b'h', b'u', b'1', b'2', b'3', b'4',
            0x00, // passphrase
            0x00, 0x05, 0x00, 0x02, 0x06, // channel
            0x00, 0x0c, 0x00, 0x03, b'f', b'e', b'8', b'0', b':', b':', b'1', 0x00, // ip
            0x00, 0x05, 0x00, 0x04, 0x02, // security
        ];
        expect.extend_from_slice(&[0x00, (4 + wireless.len()) as u8, 0x00, 0x01]);
        expect.extend_from_slice(&wireless);
        expect.extend_from_slice(&[0x00, 0x08, 0x00, 0x02, 0x00, 0x00, 0x1b, 0x58]);
        expect.extend_from_slice(&[0x00, 0x16, 0x00, 0x03]);
        expect.extend_from_slice(b"AA:BB:CC:DD:EE:FF\0");
        expect.extend_from_slice(&[0x00, 0x07, 0x00, 0x04, b'a', b'b', 0x00]);
        expect.extend_from_slice(&[0x00, 0x0d, 0x00, 0x05]);
        expect.extend_from_slice(b"280.33.8\0");
        let len = expect.len() as u16;
        expect[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(csm, expect);
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, MSG_CARPLAY_START_SESSION);
        assert_eq!(params[0].0, 1, "wireless attributes, not wired");
    }

    #[test]
    fn the_wifi_config_and_availability() {
        let csm = accessory_wifi_config("LEHU", "lehu1234", WifiSecurity::WpaWpa2, 11);
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, MSG_ACCESSORY_WIFI_CONFIG);
        assert_eq!(params[0], (1, &b"LEHU\0"[..]));
        assert_eq!(params[1], (2, &b"lehu1234\0"[..]));
        assert_eq!(params[2], (3, &[2][..]));
        assert_eq!(params[3], (4, &[11][..]));
        let availability = carplay_availability("AA:BB");
        let (id, params) = parse_csm(&availability).unwrap();
        assert_eq!(id, MSG_CARPLAY_AVAILABILITY);
        let w = parse_params(params[0].1).unwrap();
        assert_eq!(w[0], (0, &[1][..]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iap2::parse_params;

    #[test]
    fn identification_carries_the_usb_host_transport() {
        let csm = wired_identification_for("LEHU", "LEHU1,1", "LEHU", Declared::lean(true));
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, MSG_IDENTIFICATION_INFORMATION);
        assert_eq!(params[0], (0, &b"LEHU\0"[..]));
        let sent = params.iter().find(|(k, _)| *k == 6).unwrap().1;
        assert_eq!(sent.len(), WIRED_SENT.len() * 2);
        assert_eq!(&sent[sent.len() - 2..], &[0x43, 0x01]);
        let usb = params.iter().find(|(k, _)| *k == 16).unwrap().1;
        let sub = parse_params(usb).unwrap();
        assert_eq!(sub[1], (1, &b"USBHostTransport\0"[..]));
        assert_eq!(sub[3], (3, &[3u8][..]));
        assert_eq!(params.iter().find(|(k, _)| *k == 8).unwrap().1, &[2]);
        assert!(params.iter().all(|(k, _)| *k != ROUTE_GUIDANCE_COMPONENT));
    }

    /// The ids of a list parameter.
    fn ids(v: &[u8]) -> Vec<u16> {
        v.chunks(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn the_full_identifications_add_now_playing_and_guidance() {
        assert_eq!(
            wired_identification("L", "M", "L"),
            wired_identification_for("L", "M", "L", Declared::FULL)
        );
        assert_eq!(
            wireless_identification("L", "M", "L", &[2; 6]),
            wireless_identification_for("L", "M", "L", &[2; 6], Declared::FULL)
        );
        for (csm, before) in [
            (
                wired_identification_for("LEHU", "M", "L", Declared::FULL),
                wired_identification_for("LEHU", "M", "L", Declared::lean(true)),
            ),
            (
                wireless_identification_for("LEHU", "M", "L", &[1; 6], Declared::FULL),
                wireless_identification_for("LEHU", "M", "L", &[1; 6], Declared::lean(false)),
            ),
        ] {
            let (_, params) = parse_csm(&csm).unwrap();
            let find = |k: u16| params.iter().find(|(p, _)| *p == k).map(|(_, v)| *v);
            let sent = ids(find(6).unwrap());
            let received = ids(find(7).unwrap());
            for id in [0x5000, 0x5002, 0x5200, 0x5203] {
                assert_eq!(sent.iter().filter(|&&s| s == id).count(), 1, "{id:#06x}");
            }
            for id in [0x5001, 0x5201, 0x5202] {
                assert_eq!(
                    received.iter().filter(|&&r| r == id).count(),
                    1,
                    "{id:#06x}"
                );
            }
            let rg = parse_params(find(ROUTE_GUIDANCE_COMPONENT).unwrap()).unwrap();
            assert_eq!(rg[0], (0, &[0, 0][..]));
            assert_eq!(rg[1], (1, &b"LEHU\0"[..]));
            assert_eq!(rg[2], (2, &[0, 128][..]));
            assert_eq!(rg[6], (6, &[0, 0][..]));
            // The lean one is the full one without the additions, in the
            // same order: what phones accepted before.
            let (_, lean) = parse_csm(&before).unwrap();
            let kept: Vec<u16> = params.iter().map(|(k, _)| *k).collect();
            let mut want: Vec<u16> = lean.iter().map(|(k, _)| *k).collect();
            want.push(ROUTE_GUIDANCE_COMPONENT);
            assert_eq!(kept, want);
        }
        // The lean wired one still has now playing, as it always had.
        let lean_csm = wired_identification_for("L", "M", "L", Declared::lean(true));
        let (_, lean) = parse_csm(&lean_csm).unwrap();
        let sent = ids(lean.iter().find(|(k, _)| *k == 6).unwrap().1);
        assert!(sent.contains(&0x5000) && !sent.contains(&0x5200));
    }

    #[test]
    fn start_session_and_auth_messages() {
        let csm = wired_start_session(&WiredSession {
            ip: "fe80::1",
            port: 7000,
            device_id: "02:11:22:33:44:55",
            public_key: "ab",
            source_version: "280.33.8",
            ultra: UltraStart::default(),
        });
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, MSG_CARPLAY_START_SESSION);
        let wired = parse_params(params[0].1).unwrap();
        assert_eq!(wired[0], (0, &b"fe80::1\0"[..]));
        assert_eq!(params[1], (2, &[0, 0, 0x1b, 0x58][..]));
        assert_eq!(params[4], (5, &b"280.33.8\0"[..]));
        assert_eq!(params.len(), 5, "nothing of Ultra");
        let challenge = pack_csm(MSG_REQUEST_AUTH_CHALLENGE_RESPONSE, &[(0, &[1, 2, 3])]);
        assert_eq!(auth_challenge(&challenge), Some(vec![1, 2, 3]));
        assert_eq!(auth_challenge(&pack_csm(MSG_AUTH_FAILED, &[])), None);
        assert_eq!(
            parse_csm(&auth_certificate(b"c")).unwrap().1,
            vec![(0, &b"c"[..])]
        );
        assert_eq!(
            parse_csm(&auth_response(b"s")).unwrap().0,
            MSG_AUTH_RESPONSE
        );
    }

    #[test]
    fn an_ultra_start_names_mutual_authentication_and_the_theme() {
        let theme = Theme {
            asset: "44C9FA31EC".into(),
            version: 985,
            sdk: "19.0".into(),
            center: "Center_Display".into(),
            layout: None,
        };
        let session = |ultra| {
            wired_start_session(&WiredSession {
                ip: "fe80::1",
                port: 7000,
                device_id: "02:11:22:33:44:55",
                public_key: "ab",
                source_version: "280.33.8",
                ultra,
            })
        };
        let plain = session(UltraStart {
            mutual_auth: true,
            theme: None,
        });
        let (_, params) = parse_csm(&plain).unwrap();
        assert_eq!(params.len(), 6);
        assert_eq!(params[5], (8, &[1][..]), "supports mutual authentication");
        let themed = session(UltraStart {
            mutual_auth: true,
            theme: Some(&theme),
        });
        let (_, params) = parse_csm(&themed).unwrap();
        assert_eq!(params[5], (6, &b"19.0\0"[..]));
        let asset = parse_params(params[6].1).unwrap();
        assert_eq!(
            asset,
            [(0, &b"44C9FA31EC\0"[..]), (1, &985u32.to_be_bytes()[..])]
        );
        assert_eq!(params[7].0, 8);

        // The identification's transport component names the asset.
        let component = |csm: &[u8], field: u16| {
            let (_, p) = parse_csm(csm).unwrap();
            let (_, v) = p.iter().find(|(k, _)| *k == field).copied().unwrap();
            parse_params(v)
                .unwrap()
                .iter()
                .map(|(k, v)| (*k, v.to_vec()))
                .collect::<Vec<_>>()
        };
        let wired = wired_identification_themed("L", "M", "L", Declared::FULL, Some("44C9FA31EC"));
        assert_eq!(
            component(&wired, 16).last().unwrap(),
            &(6, b"44C9FA31EC\0".to_vec())
        );
        assert_eq!(
            component(&wired_identification_for("L", "M", "L", Declared::FULL), 16).len(),
            5,
            "no theme, no asset"
        );
        let wireless =
            wireless_identification_themed("L", "M", "L", &[1; 6], Declared::FULL, Some("A1"));
        assert_eq!(
            component(&wireless, 24).last().unwrap(),
            &(6, b"A1\0".to_vec())
        );
        assert_eq!(
            wired_identification_themed("L", "M", "L", Declared::FULL, None),
            wired_identification_for("L", "M", "L", Declared::FULL)
        );
    }
}
