// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The wireless bootstrap, the sibling of the wired trigger: the phone
//! finds the head unit over Bluetooth (the iAP2 and CarPlay services in its
//! SDP), opens the iAP2 channel, and over it asks for the identification
//! (declaring a wireless CarPlay transport) and the MFi authentication;
//! when it declares CarPlay available, CarPlayStartSession hands it the
//! access point's name, passphrase and channel, our address on it and the
//! control port. The phone then joins the AP and connects to the same
//! control server the wired path uses.
//!
//! The iAP2 link stays open for the whole Bluetooth connection, not just
//! until the handoff: the phone treats it as the CarPlay control transport
//! while it joins the AP and brings up AV over IP, and closing it right
//! after the handoff aborts the session.
//!
//! One thread waits for connections on the three profiles and watches the
//! access point's carrier; each iAP2 connection gets its own thread. The
//! access point going down under a live session ends it (`LinkLost`); a
//! phone leaving Wi-Fi ends it through its control connection breaking or
//! going silent, which the control thread reports.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lehu_bt::{Address, RfcommConnection, RfcommHandle, RfcommProfile};
use lehu_session::{Fault, Phase};

use crate::carplay_msg as msg;
use crate::iap2::{self, Received};
use crate::session::Shared;
use crate::trigger;

/// iAP2 over Bluetooth, the accessory's server side and the phone's.
pub const UUID_IAP_SERVER: &str = "00000000-deca-fade-deca-deafdecacaff";
pub const UUID_IAP_CLIENT: &str = "00000000-deca-fade-deca-deafdecacafe";
/// The CarPlay service: its record in our SDP is what makes the phone list
/// the head unit under Settings > General > CarPlay.
pub const UUID_CARPLAY: &str = "ec884348-cd41-40a2-9727-575d50bf1fd3";
pub const IAP_CHANNEL: u8 = 3;
pub const CARPLAY_CHANNEL: u8 = 4;

/// The three profiles the bootstrap registers.
pub fn profiles() -> [RfcommProfile; 3] {
    [
        RfcommProfile::server(UUID_IAP_SERVER, "Wireless iAP", IAP_CHANNEL),
        RfcommProfile::client(UUID_IAP_CLIENT, "Wireless iAP"),
        RfcommProfile::server(UUID_CARPLAY, "CarPlay", CARPLAY_CHANNEL),
    ]
}

/// Bluetooth connections served at once: a phone's reconnection may
/// overlap its old connection.
const MAX_LINKS: usize = 2;

/// How long the phone has to answer the link synchronisation.
const NEGOTIATE_SECS: u64 = 10;

/// What the handoff tells the phone, fixed for the session.
pub struct Handoff {
    pub ap: lehu_wifi::Params,
    /// The access point interface's MAC when it has one, else the
    /// pairing-derived id: the device identifier of the start message.
    pub device_id: String,
    pub public_key: String,
    /// Reads the adapter's address when a link comes: bluetoothd may not
    /// have been up when the session started.
    pub adapter: lehu_bt::AdapterAddress,
}

impl Handoff {
    /// Our address on the AP as the phone should connect to it: the
    /// link-local IPv6, scoped to its own Wi-Fi link, else the IPv4.
    fn ip(&self) -> String {
        lehu_wifi::link_local_of(&self.ap.iface).unwrap_or_else(|| self.ap.ip.clone())
    }

    fn start_session(&self, port: u16, ultra: msg::UltraStart) -> Vec<u8> {
        let ip = self.ip();
        msg::wireless_start_session(&msg::WirelessSession {
            ssid: &self.ap.ssid,
            passphrase: &self.ap.passphrase,
            channel: self.ap.channel.min(255) as u8,
            ip: &ip,
            security: msg::WifiSecurity::WpaWpa2,
            port: u32::from(port),
            device_id: &self.device_id,
            public_key: &self.public_key,
            source_version: crate::info::SOURCE_VERSION,
            ultra,
        })
    }
}

/// The handles of the three profiles.
pub struct Links {
    pub iap: RfcommHandle,
    pub iap_client: RfcommHandle,
    pub carplay: RfcommHandle,
}

/// The accept-and-watch thread. Ends when the session does; the handles
/// (and with them the profiles) go with it.
pub fn run(shared: Arc<Shared>, links: Links, handoff: Arc<Handoff>, port: u16) {
    let mut threads: Vec<JoinHandle<()>> = Vec::new();
    // The carrier is watched only once it was seen up with a phone in the
    // session: the AP coming up late is not a loss.
    let mut carrier_seen = false;
    while shared.is_running() && !shared.closing.load(Ordering::Acquire) {
        threads.retain(|t| !t.is_finished());
        let mut incoming: Vec<(RfcommConnection, bool)> = Vec::new();
        if let Some(c) = links.iap.accept(Duration::from_millis(250)) {
            incoming.push((c, true));
        }
        if let Some(c) = links.iap_client.try_accept() {
            incoming.push((c, true));
        }
        if let Some(c) = links.carplay.try_accept() {
            incoming.push((c, false));
        }
        for (conn, iap) in incoming {
            if threads.len() >= MAX_LINKS {
                log::warn!(
                    "carplay: a third Bluetooth connection from {}, closed",
                    conn.peer
                );
                continue;
            }
            let shared = Arc::clone(&shared);
            let handoff = Arc::clone(&handoff);
            let name = if iap { "cp-iap2" } else { "cp-bt-hold" };
            let spawned = thread::Builder::new().name(name.into()).spawn(move || {
                if iap {
                    bootstrap(shared, conn, handoff, port);
                } else {
                    hold(shared, conn);
                }
            });
            match spawned {
                Ok(t) => threads.push(t),
                Err(e) => log::error!("carplay: cannot serve the Bluetooth connection: {e}"),
            }
        }
        // The access point going down under the session ends it; a phone
        // that left Wi-Fi is seen by the control thread.
        let in_session =
            shared.control_seen.load(Ordering::Acquire) && shared.phase() != Phase::Closed;
        match lehu_wifi::carrier(&handoff.ap.iface) {
            Some(true) => carrier_seen = true,
            Some(false) | None if carrier_seen && in_session => {
                log::warn!("carplay: the access point went down; the wireless session ends");
                shared.fail(Fault::LinkLost);
                break;
            }
            _ => {}
        }
    }
    // Asked to close before any phone connected: nothing else will say so.
    if shared.closing.load(Ordering::Acquire) && !shared.control_seen.load(Ordering::Acquire) {
        shared.set_phase(Phase::Closed);
    }
    drop(links);
    for t in threads {
        let _ = t.join();
    }
}

/// A connection on the CarPlay channel: held open until the phone closes
/// it (nothing is spoken on it), so the phone sees the service answer.
fn hold(shared: Arc<Shared>, conn: RfcommConnection) {
    let peer = conn.peer;
    let mut stream = conn.into_stream(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 256];
    while shared.is_running() {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    log::debug!("carplay: the CarPlay channel from {peer} closed");
}

/// One iAP2 connection from a phone, until it ends.
fn bootstrap(shared: Arc<Shared>, conn: RfcommConnection, handoff: Arc<Handoff>, port: u16) {
    let peer = conn.peer;
    log::info!("carplay: iAP2 over Bluetooth from {peer}");
    *shared.bt_peer.lock().unwrap_or_else(|e| e.into_inner()) = peer.to_string();
    let stream = conn.into_stream(Some(Duration::from_millis(200)));
    let mut link = iap2::Link::new(stream);
    if let Err(e) = link.negotiate(Instant::now() + Duration::from_secs(NEGOTIATE_SECS)) {
        log::warn!("carplay: iAP2 over Bluetooth: {e}");
        return;
    }
    let bt_mac = handoff.adapter.get().unwrap_or_default();
    let mut session = Bootstrap {
        shared: &shared,
        handoff: &handoff,
        port,
        bt_mac,
        handed_off: false,
        state: trigger::LinkState::new(false),
    };
    let mut csm = Vec::with_capacity(2048);
    while shared.is_running() && !shared.closing.load(Ordering::Acquire) {
        match link.recv_control(&mut csm) {
            Ok(Received::Control) => {
                if let Err(e) = session.answer(&mut link, &csm) {
                    log::warn!("carplay: iAP2 over Bluetooth: {e}");
                    break;
                }
            }
            Ok(Received::File) => {
                if let Err(e) = trigger::file_datagram(&shared, &mut link, &mut session.state, &csm)
                {
                    log::warn!("carplay: iAP2 over Bluetooth: {e}");
                    break;
                }
            }
            Ok(Received::Nothing) => {}
            Err(e) => {
                log::info!(
                    "carplay: the Bluetooth iAP2 link from {peer} ended ({e}, handoff {})",
                    if session.handed_off {
                        "delivered"
                    } else {
                        "not delivered"
                    }
                );
                break;
            }
        }
    }
}

/// The state of one bootstrap: what was already told.
struct Bootstrap<'a> {
    shared: &'a Arc<Shared>,
    handoff: &'a Handoff,
    port: u16,
    bt_mac: Address,
    handed_off: bool,
    state: trigger::LinkState,
}

impl Bootstrap<'_> {
    /// One control-session message from the phone and its answer.
    fn answer<S: Read + Write>(
        &mut self,
        link: &mut iap2::Link<S>,
        csm: &[u8],
    ) -> std::io::Result<()> {
        let Some((id, _)) = iap2::parse_csm(csm) else {
            return Ok(());
        };
        let cfg = &self.shared.config;
        let bt_mac = &self.bt_mac.0;
        let identification = |declared| {
            msg::wireless_identification_themed(
                &cfg.name,
                &cfg.model,
                &cfg.manufacturer,
                bt_mac,
                declared,
                cfg.theme_asset(),
            )
        };
        if id == msg::MSG_IDENTIFICATION_REJECTED && self.state.retry_lean(csm) {
            return link.send_control(&identification(self.state.declared));
        }
        if let Some(r) = trigger::answer_common(self.shared, link, &mut self.state, id, csm) {
            return r;
        }
        match id {
            msg::MSG_START_IDENTIFICATION => {
                log::info!("carplay: identification requested (wireless)");
                link.send_control(&identification(self.state.declared))
            }
            msg::MSG_REQUEST_ACCESSORY_WIFI_CONFIG => {
                let ap = &self.handoff.ap;
                log::info!("carplay: the phone asks for the access point ({})", ap.ssid);
                link.send_control(&msg::accessory_wifi_config(
                    &ap.ssid,
                    &ap.passphrase,
                    msg::WifiSecurity::WpaWpa2,
                    ap.channel.min(255) as u8,
                ))
            }
            msg::MSG_CARPLAY_AVAILABILITY => {
                // The phone is ready: hand it the network and where we are
                // on it. It joins the AP and connects to the control port.
                let start = self
                    .handoff
                    .start_session(self.port, self.shared.config.ultra_start());
                log::info!(
                    "carplay: CarPlayStartSession: join {} and connect to [{}]:{}",
                    self.handoff.ap.ssid,
                    self.handoff.ip(),
                    self.port
                );
                link.send_control(&start)?;
                self.handed_off = true;
                Ok(())
            }
            msg::MSG_WIRELESS_CARPLAY_UPDATE | msg::MSG_DEVICE_TRANSPORT_ID => {
                log::debug!("carplay: iAP2 message {id:#06x} noted");
                Ok(())
            }
            other => {
                log::debug!("carplay: iAP2 message {other:#06x} ignored");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use lehu_core::poll::Wake;
    use lehu_core::queue::Queue;
    use lehu_session::{AudioFormat, AudioKind, MicFormat, Ports, Sinks};
    use lehu_video::Codec;

    use crate::Config;
    use crate::iap2::{
        CONTROL_ACK, CONTROL_SESSION, CONTROL_SYN, LinkSync, pack_packet, parse_csm, parse_packet,
    };

    struct NoSinks;

    impl Sinks for NoSinks {
        fn audio_pcm(&mut self, _: AudioKind, _: u32, _: u8, _: &[i16]) {}
        fn audio_packet(&mut self, _: AudioKind, _: AudioFormat, _: &[u8]) {}
        fn mic(&mut self, _: Option<MicFormat>) {}
    }

    /// The phone's end of the link: what we write is kept, what it says
    /// is queued.
    #[derive(Clone, Default)]
    struct Pipe {
        inner: Arc<Mutex<(VecDeque<u8>, Vec<u8>)>>,
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut g = self.inner.lock().unwrap();
            if g.0.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "quiet"));
            }
            let n = buf.len().min(g.0.len());
            for b in buf.iter_mut().take(n) {
                *b = g.0.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.lock().unwrap().1.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The CSMs in what we wrote, in order.
    fn sent_csms(pipe: &Pipe) -> Vec<Vec<u8>> {
        let bytes = std::mem::take(&mut pipe.inner.lock().unwrap().1);
        let mut out = Vec::new();
        let mut off = 0;
        while off + 9 <= bytes.len() {
            if bytes[off..].starts_with(&iap2::DETECT_MARKER) {
                off += 6;
                continue;
            }
            let len = usize::from(u16::from_be_bytes([bytes[off + 2], bytes[off + 3]]));
            let (h, p) = parse_packet(&bytes[off..off + len]).expect("a whole packet");
            if h.session == CONTROL_SESSION && !p.is_empty() {
                out.push(p.to_vec());
            }
            off += len;
        }
        out
    }

    #[test]
    fn the_bootstrap_identifies_and_hands_off_the_access_point() {
        let wake = Arc::new(Wake::new().unwrap());
        let units = Arc::new(Queue::new(1));
        let ports = Ports::new(wake, units.clone(), units);
        let shared = Shared::for_test(Config::for_test(Codec::H264), Box::new(NoSinks), ports);
        let pipe = Pipe::default();
        let mut link = iap2::Link::new(pipe.clone());
        pipe.inner.lock().unwrap().0.extend(pack_packet(
            CONTROL_SYN,
            0x2b,
            0,
            0,
            &LinkSync::proposal().pack(),
        ));
        pipe.inner
            .lock()
            .unwrap()
            .0
            .extend(pack_packet(CONTROL_ACK, 0x2b, 99, 0, &[]));
        link.negotiate(Instant::now() + Duration::from_secs(1))
            .unwrap();
        sent_csms(&pipe);

        let handoff = Handoff {
            ap: lehu_wifi::Params {
                iface: "no-such-interface".into(),
                ssid: "LEHU".into(),
                passphrase: "lehu1234".into(),
                channel: 6,
                ip: "192.168.53.1".into(),
                bssid: "AA:BB:CC:DD:EE:FF".into(),
            },
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            public_key: "ab".into(),
            adapter: lehu_bt::Manager::start(&Default::default(), |_| {})
                .unwrap()
                .adapter(),
        };
        let mut b = Bootstrap {
            shared: &shared,
            handoff: &handoff,
            port: 7000,
            bt_mac: Address([1, 2, 3, 4, 5, 6]),
            handed_off: false,
            state: trigger::LinkState::new(false),
        };
        b.answer(
            &mut link,
            &iap2::pack_csm(msg::MSG_START_IDENTIFICATION, &[]),
        )
        .unwrap();
        b.answer(
            &mut link,
            &iap2::pack_csm(msg::MSG_IDENTIFICATION_ACCEPTED, &[]),
        )
        .unwrap();
        b.answer(
            &mut link,
            &iap2::pack_csm(msg::MSG_REQUEST_ACCESSORY_WIFI_CONFIG, &[]),
        )
        .unwrap();
        assert!(!b.handed_off);
        b.answer(&mut link, &msg::carplay_availability("11:22"))
            .unwrap();
        assert!(b.handed_off);
        // An unknown message is ignored, and the authentication without a
        // chip is answered with nothing.
        b.answer(&mut link, &iap2::pack_csm(0x4e0d, &[(0, &[1])]))
            .unwrap();
        b.answer(
            &mut link,
            &iap2::pack_csm(msg::MSG_REQUEST_AUTH_CERTIFICATE, &[]),
        )
        .unwrap();
        b.answer(&mut link, &[1, 2, 3]).unwrap();

        let sent = sent_csms(&pipe);
        // The acknowledgement of the phone's SYN carried no CSM, so three
        // messages: the identification, the Wi-Fi config, the start.
        let ids: Vec<u16> = sent.iter().map(|c| parse_csm(c).unwrap().0).collect();
        assert_eq!(
            ids,
            vec![
                msg::MSG_IDENTIFICATION_INFORMATION,
                msg::MSG_ACCESSORY_WIFI_CONFIG,
                msg::MSG_CARPLAY_START_SESSION
            ]
        );
        assert_eq!(
            sent[0],
            msg::wireless_identification_for(
                "LEHU",
                "LEHU1,1",
                "LEHU",
                &[1, 2, 3, 4, 5, 6],
                msg::Declared::FULL
            )
        );
        assert_eq!(
            sent[2],
            msg::wireless_start_session(&msg::WirelessSession {
                ssid: "LEHU",
                passphrase: "lehu1234",
                channel: 6,
                ip: "192.168.53.1",
                security: msg::WifiSecurity::WpaWpa2,
                port: 7000,
                device_id: "AA:BB:CC:DD:EE:FF",
                public_key: "ab",
                source_version: crate::info::SOURCE_VERSION,
                ultra: msg::UltraStart::default(),
            })
        );
    }

    #[test]
    fn after_identification_and_authentication_the_phone_is_asked_for_now_playing_and_guidance() {
        let wake = Arc::new(Wake::new().unwrap());
        let units = Arc::new(Queue::new(1));
        let ports = Ports::new(wake, units.clone(), units);
        let shared = Shared::for_test(
            Config::for_test(Codec::H264),
            Box::new(NoSinks),
            ports.clone(),
        );
        let pipe = Pipe::default();
        let mut link = iap2::Link::new(pipe.clone());
        pipe.inner.lock().unwrap().0.extend(pack_packet(
            CONTROL_SYN,
            0x2b,
            0,
            0,
            &LinkSync::proposal().pack(),
        ));
        pipe.inner
            .lock()
            .unwrap()
            .0
            .extend(pack_packet(CONTROL_ACK, 0x2b, 99, 0, &[]));
        link.negotiate(Instant::now() + Duration::from_secs(1))
            .unwrap();
        sent_csms(&pipe);
        let handoff = Handoff {
            ap: lehu_wifi::Params {
                iface: "no-such-interface".into(),
                ssid: "LEHU".into(),
                passphrase: "lehu1234".into(),
                channel: 6,
                ip: "192.168.53.1".into(),
                bssid: "AA:BB:CC:DD:EE:FF".into(),
            },
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            public_key: "ab".into(),
            adapter: lehu_bt::Manager::start(&Default::default(), |_| {})
                .unwrap()
                .adapter(),
        };
        let mut b = Bootstrap {
            shared: &shared,
            handoff: &handoff,
            port: 7000,
            bt_mac: Address([1, 2, 3, 4, 5, 6]),
            handed_off: false,
            state: trigger::LinkState::new(false),
        };
        let mut say = |id: u16| b.answer(&mut link, &iap2::pack_csm(id, &[])).unwrap();
        say(msg::MSG_START_IDENTIFICATION);
        // Rejected: offered again without the additions, once.
        say(msg::MSG_IDENTIFICATION_REJECTED);
        say(msg::MSG_IDENTIFICATION_REJECTED);
        say(msg::MSG_START_IDENTIFICATION);
        let ident =
            |d| msg::wireless_identification_for("LEHU", "LEHU1,1", "LEHU", &[1, 2, 3, 4, 5, 6], d);
        let sent = sent_csms(&pipe);
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0], ident(msg::Declared::FULL));
        assert_eq!(sent[1], ident(msg::Declared::lean(false)));
        assert_eq!(sent[2], sent[1], "asked again, the lean one again");
        // Only the second rejection was the phone's last word.
        let events: Vec<_> = std::iter::from_fn(|| ports.events.pop()).collect();
        assert_eq!(
            events,
            [lehu_session::Event::Fault(lehu_session::Fault::Rejected)]
        );

        // A fresh link, the full set accepted: once both steps are done the
        // subscriptions go, in either order of the two.
        b.state = trigger::LinkState::new(false);
        let mut say = |id: u16| b.answer(&mut link, &iap2::pack_csm(id, &[])).unwrap();
        say(msg::MSG_AUTH_SUCCEEDED);
        assert!(sent_csms(&pipe).is_empty());
        say(msg::MSG_IDENTIFICATION_ACCEPTED);
        say(msg::MSG_IDENTIFICATION_ACCEPTED);
        let sent = sent_csms(&pipe);
        assert_eq!(
            sent,
            [
                crate::media::start_now_playing(true),
                crate::media::start_route_guidance()
            ]
        );

        // The phone's updates land in the side channel, named by an event.
        let mut title = b"Song".to_vec();
        title.push(0);
        let item = crate::iap2::pack_params(&[(1, &title)]);
        let update = iap2::pack_csm(crate::media::MSG_NOW_PLAYING_UPDATE, &[(0, &item)]);
        b.answer(&mut link, &update).unwrap();
        let guidance = iap2::pack_csm(crate::media::MSG_ROUTE_GUIDANCE_UPDATE, &[(1, &[1])]);
        b.answer(&mut link, &guidance).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| ports.events.pop()).collect();
        assert_eq!(
            events,
            [
                lehu_session::Event::NowPlaying,
                lehu_session::Event::Guidance
            ]
        );
        let mut np = lehu_session::NowPlaying::default();
        assert!(ports.now_playing.read_into(0, &mut np).is_some());
        assert_eq!(np.title, "Song");
        let mut g = lehu_session::Guidance::default();
        assert!(ports.guidance.read_into(0, &mut g).is_some());
        assert!(g.active);

        // The art: named by an update, then sent over the link's file
        // transfer session and acknowledged there.
        let item = crate::iap2::pack_params(&[(1, &title), (26, &[5])]);
        let update = iap2::pack_csm(crate::media::MSG_NOW_PLAYING_UPDATE, &[(0, &item)]);
        b.answer(&mut link, &update).unwrap();
        trigger::file_datagram(&shared, &mut link, &mut b.state, &[5, 0xc0, 0xff, 0xd8]).unwrap();
        let replies: Vec<_> = {
            let bytes = std::mem::take(&mut pipe.inner.lock().unwrap().1);
            let mut out = Vec::new();
            let mut off = 0;
            while off + 9 <= bytes.len() {
                let len = usize::from(u16::from_be_bytes([bytes[off + 2], bytes[off + 3]]));
                let (h, p) = parse_packet(&bytes[off..off + len]).unwrap();
                if h.session == 12 {
                    out.push(p.to_vec());
                }
                off += len;
            }
            out
        };
        assert_eq!(replies, [vec![5, 0x05]]);
        let mut art = lehu_session::Artwork::default();
        let seen = ports.artwork.read_into(0, &mut art).expect("the art");
        assert_eq!(art.data, [0xff, 0xd8]);
        // Another song, no art named: the last one's goes.
        let mut other = b"Other".to_vec();
        other.push(0);
        let item = crate::iap2::pack_params(&[(1, &other)]);
        let update = iap2::pack_csm(crate::media::MSG_NOW_PLAYING_UPDATE, &[(0, &item)]);
        b.answer(&mut link, &update).unwrap();
        assert!(ports.artwork.read_into(seen, &mut art).is_some());
        assert!(art.data.is_empty());
    }

    #[test]
    fn the_profiles_are_the_three_the_phone_looks_for() {
        let [iap, client, cp] = profiles();
        assert_eq!(iap.channel, 3);
        assert_eq!(cp.channel, 4);
        assert_eq!(client.role, lehu_bt::Role::Client);
        assert!(iap.record.as_deref().unwrap().contains(UUID_IAP_SERVER));
        assert!(cp.record.as_deref().unwrap().contains("CarPlay"));
    }
}
