//! Android Auto: the GAL frames and their reassembly, the hand-written
//! protobuf messages, the session machine behind them in the clear and
//! under TLS, and the Bluetooth handoff's framing.

use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, OnceLock};

use lehu_aa::aaw::{self, Network};
use lehu_aa::consts::*;
use lehu_aa::frame::{Assembler, Class, Fragments, Header, parse_total};
use lehu_aa::handoff::{self, Handoff};
use lehu_aa::machine::{Config, Machine, Work};
use lehu_aa::proto;
use lehu_aa::tls::Tls;
use lehu_core::poll::Wake;
use lehu_core::queue::Queue;
use lehu_core::settings::Settings;
use lehu_session::{AudioFormat, AudioKind, Event, MicFormat, Ports, Sinks};
use openssl::pkey::PKey;
use openssl::ssl::{Ssl, SslContext, SslMethod, SslVersion};
use openssl::x509::X509;
use prost::Message;

use crate::{Pipe, Rng, Target, bounded, ensure, fingerprint};

// ── Frames ────────────────────────────────────────────────────────────────

/// The frames of one message as the phone sends it.
fn frames_of(ch: u8, class: Class, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut frame = Vec::new();
    let mut f = Fragments::new(ch, class, payload);
    while f.next_into(&mut frame) {
        out.extend_from_slice(&frame);
    }
    out
}

/// One frame cut off the front of `b` as the read loop does: the header,
/// the total of a first fragment, the payload. `None` where the loop ends
/// the link (a desync, or the stream ran out).
fn next_frame(b: &[u8], at: &mut usize) -> Option<(Header, u32, std::ops::Range<usize>)> {
    let head: [u8; 4] = b.get(*at..*at + 4)?.try_into().ok()?;
    let header = Header::parse(head).ok()?;
    *at += 4;
    let total = if header.has_total() {
        let ext: [u8; 4] = b.get(*at..*at + 4)?.try_into().ok()?;
        *at += 4;
        parse_total(ext).ok()?
    } else {
        0
    };
    let end = *at + usize::from(header.len);
    if end > b.len() {
        return None;
    }
    let range = *at..end;
    *at = end;
    Some((header, total, range))
}

fn frame_seeds() -> Vec<Vec<u8>> {
    let mut msg = MSG_VERSION_RESPONSE.to_be_bytes().to_vec();
    msg.extend_from_slice(&[0, 1, 0, 7, 0, 0]);
    let big = vec![0x33u8; MAX_FRAME_PAYLOAD + 100];
    vec![
        frames_of(CH_CONTROL, Class::Plaintext, &msg),
        frames_of(CH_VIDEO, Class::EncSignal, &big),
        [
            frames_of(
                CH_MEDIA_AUDIO,
                Class::Plaintext,
                &[0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            ),
            frames_of(CH_SENSOR, Class::EncControl, &[0x80, 1, 8, 13]),
        ]
        .concat(),
    ]
}

#[test]
fn aa_reassembly() {
    Target::new("aa_reassembly")
        .seeds(frame_seeds())
        .max_len(40_000)
        .run(|b| {
            let mut asm = Assembler::new();
            let mut at = 0;
            let mut appended = 0;
            let mut messages = 0;
            while let Some((h, total, range)) = next_frame(b, &mut at) {
                let Some(buf) = asm.target(&h, total) else {
                    continue;
                };
                buf.extend_from_slice(&b[range.clone()]);
                appended += range.len();
                // The reserve follows the announced total, capped.
                ensure(
                    buf.capacity() <= ASSEMBLY_RESERVE.max(2 * appended) + 64,
                    || format!("a reassembly buffer of {} bytes", buf.capacity()),
                )?;
                match asm.complete(&h) {
                    Ok(Some(m)) => messages += m.len(),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            bounded("reassembled messages", messages, b.len(), 1, 0)
        });
}

// ── The protobuf messages ─────────────────────────────────────────────────

/// Decode `b` as `M`; what decodes re-encodes and decodes the same.
fn round_trip<M: Message + Default + PartialEq + std::fmt::Debug>(b: &[u8]) -> Result<(), String> {
    if let Some(m) = proto::decode::<M>(b) {
        let again = m.encode_to_vec();
        let back = M::decode(again.as_slice()).map_err(|e| e.to_string())?;
        ensure(back == m, || {
            format!("{} does not round trip", std::any::type_name::<M>())
        })?;
        // An encoded message is no bigger than... what it was decoded from
        // plus what defaults and packing add.
        bounded("re-encoded message", again.len(), b.len(), 2, 16)?;
    }
    Ok(())
}

fn proto_seeds() -> Vec<Vec<u8>> {
    // Two video configurations, as a board decoding both codecs offers.
    let offer = proto::Offer {
        resolution: 1,
        codecs: [
            Some(lehu_session::Codec::H264),
            Some(lehu_session::Codec::Hevc),
        ],
        fps: 30,
        dpi: 120,
        left_hand_drive: true,
        max_unacked: 4,
    };
    vec![
        proto::ServiceDiscoveryRequest {
            device_name: Some("Pixel 8".into()),
            phone_info: Some(proto::PhoneInfo {
                instance_id: Some("abc123".into()),
            }),
        }
        .encode_to_vec(),
        proto::service_discovery_response(&offer).encode_to_vec(),
        proto::VersionResponseOptions {
            connection_configuration: Some(proto::ConnectionConfiguration {
                ping_configuration: Some(proto::PingConfiguration {
                    timeout_ms: Some(5000),
                    interval_ms: Some(1000),
                    high_latency_threshold_ms: Some(200),
                    tracked_ping_count: Some(5),
                }),
            }),
        }
        .encode_to_vec(),
        proto::MediaConfig {
            status: 2,
            max_unacked: Some(4),
            configuration_indices: vec![0, 1, 300],
        }
        .encode_to_vec(),
        proto::VideoFocusRequestNotification {
            mode: Some(1),
            reason: Some(2),
        }
        .encode_to_vec(),
        // Metadata with album art in field 4, as the phone sends it.
        [
            proto::MediaPlaybackMetadata {
                title: Some("Song".into()),
                subtitle: Some("Artist".into()),
                description: Some("Album".into()),
                duration_seconds: Some(215),
            }
            .encode_to_vec(),
            vec![0x22, 6, 0x89, b'P', b'N', b'G', 0x0d, 0x0a],
        ]
        .concat(),
    ]
}

#[test]
fn aa_proto() {
    Target::new("aa_proto").seeds(proto_seeds()).run(|b| {
        round_trip::<proto::ServiceDiscoveryRequest>(b)?;
        round_trip::<proto::ServiceDiscoveryResponse>(b)?;
        round_trip::<proto::VersionResponseOptions>(b)?;
        round_trip::<proto::ChannelOpenRequest>(b)?;
        round_trip::<proto::PingRequest>(b)?;
        round_trip::<proto::AudioFocusRequest>(b)?;
        round_trip::<proto::NavFocusRequestNotification>(b)?;
        round_trip::<proto::ByeByeRequest>(b)?;
        round_trip::<proto::VoiceSessionNotification>(b)?;
        round_trip::<proto::MediaSetup>(b)?;
        round_trip::<proto::MediaStart>(b)?;
        round_trip::<proto::MediaConfig>(b)?;
        round_trip::<proto::MicrophoneRequest>(b)?;
        round_trip::<proto::VideoFocusRequestNotification>(b)?;
        round_trip::<proto::SensorRequest>(b)?;
        round_trip::<proto::InputReport>(b)?;
        round_trip::<proto::SensorBatch>(b)?;
        let _ = proto::ping_config(b);
        // The album art, read in place out of a metadata message.
        if let Some(Some(art)) = proto::bytes_field(b, 4) {
            bounded("album art", art.len(), b.len(), 1, 0)?;
        }
        let mut np = lehu_session::NowPlaying::default();
        let mut art = lehu_session::Artwork::default();
        for id in [
            lehu_aa::status::MSG_PLAYBACK_STATUS,
            lehu_aa::status::MSG_PLAYBACK_METADATA,
        ] {
            let _ = lehu_aa::status::playback(id, b, &mut np, 0);
            let _ = lehu_aa::status::artwork(id, b, &mut art);
        }
        bounded("album art kept", art.data.len(), b.len(), 1, 0)?;
        Ok(())
    });
}

// ── The session machine ───────────────────────────────────────────────────

/// Sinks that count what they are given.
struct CountingSinks;

impl Sinks for CountingSinks {
    fn audio_pcm(&mut self, _: AudioKind, _: u32, _: u8, _: &[i16]) {}
    fn audio_packet(&mut self, _: AudioKind, _: AudioFormat, _: &[u8]) {}
    fn mic(&mut self, _: Option<MicFormat>) {}
}

struct Rig {
    m: Machine,
    outbox: Arc<Queue<Work>>,
    events: Arc<Queue<Event>>,
    now: u64,
    /// The TLS records the machine queued for the phone, and whether it
    /// queued its auth-complete.
    records: Vec<u8>,
    auth_complete: bool,
}

impl Rig {
    fn new(tls: Tls) -> Self {
        let wake = Arc::new(Wake::new().expect("an eventfd"));
        let ports = Ports::new(wake, Arc::new(Queue::new(8)), Arc::new(Queue::new(8)));
        let events = Arc::clone(&ports.events);
        let outbox = Arc::new(Queue::new(lehu_aa::machine::OUTBOX_DEPTH));
        let mut m = Machine::new(
            Config::from_settings(&Settings::default()),
            tls,
            Arc::clone(&outbox),
            ports,
            Box::new(CountingSinks),
            Arc::new(AtomicU8::new(0)),
            Arc::new(OnceLock::new()),
            Arc::new(OnceLock::new()),
            1000,
        );
        m.start();
        Self {
            m,
            outbox,
            events,
            now: 1000,
            records: Vec::new(),
            auth_complete: false,
        }
    }

    /// Feed a frame stream; the bytes the machine queued for the phone.
    fn feed_frames(&mut self, b: &[u8]) -> Result<usize, String> {
        let mut at = 0;
        let mut queued = 0;
        while let Some((h, total, range)) = next_frame(b, &mut at) {
            self.now += 7;
            let r = self.m.on_frame(self.now, &h, total, &b[range]);
            queued += self.drain();
            match r {
                Ok(Some(unit)) => bounded("video unit", unit.len(), b.len(), 1, 0)?,
                Ok(None) => {}
                Err(_) => break,
            }
        }
        Ok(queued)
    }

    /// Empty the outbox and the events; the bytes of the bodies queued.
    fn drain(&mut self) -> usize {
        let mut n = 0;
        while let Some(w) = self.outbox.pop() {
            if let Work::Msg(m) = w {
                n += m.body().len();
                if m.channel == CH_CONTROL && m.class == Class::Plaintext {
                    match m.msg_id {
                        MSG_SSL_HANDSHAKE => self.records.extend_from_slice(m.body()),
                        MSG_AUTH_COMPLETE => self.auth_complete = true,
                        _ => {}
                    }
                }
            }
        }
        while self.events.pop().is_some() {}
        n
    }
}

/// A message as the phone frames it, plaintext.
fn plain_message(ch: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let mut m = id.to_be_bytes().to_vec();
    m.extend_from_slice(body);
    frames_of(ch, Class::Plaintext, &m)
}

#[test]
fn aa_machine_plaintext() {
    let seeds = vec![
        plain_message(CH_CONTROL, MSG_VERSION_RESPONSE, &[0, 1, 0, 7, 0, 0]),
        [
            plain_message(CH_CONTROL, MSG_VERSION_RESPONSE, &[0, 1, 0, 7, 0, 0]),
            plain_message(
                CH_CONTROL,
                MSG_SSL_HANDSHAKE,
                &[0x16, 3, 3, 0, 4, 2, 0, 0, 0],
            ),
        ]
        .concat(),
        [
            plain_message(
                CH_CONTROL,
                MSG_PING_REQUEST,
                &proto::PingRequest { timestamp: 9 }.encode_to_vec(),
            ),
            plain_message(
                CH_VIDEO,
                MSG_MEDIA_SETUP,
                &proto::MediaSetup { codec_type: 3 }.encode_to_vec(),
            ),
            plain_message(
                CH_VIDEO,
                MSG_MEDIA_DATA,
                &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0x65],
            ),
            plain_message(CH_MEDIA_AUDIO, MSG_MEDIA_DATA, &[0; 40]),
            plain_message(
                CH_MIC,
                MSG_MICROPHONE_REQUEST,
                &proto::MicrophoneRequest { open: true }.encode_to_vec(),
            ),
        ]
        .concat(),
    ];
    let ctx = lehu_aa::tls::client_context(false).expect("the client context");
    Target::new("aa_machine_plaintext")
        .seeds(seeds)
        .max_len(8192)
        .cost(4)
        .run(|b| {
            let tls = Tls::from_ssl(Ssl::new(&ctx).map_err(|e| e.to_string())?).map_err(|e| e.0)?;
            let mut rig = Rig::new(tls);
            rig.drain();
            let queued = rig.feed_frames(b)?;
            // Answers are a few hundred bytes each, at most one per frame
            // of four bytes or more.
            bounded("queued answers", queued, b.len(), 256, 4096)
        });
}

/// The phone's end of the TLS session.
fn phone_tls() -> Tls {
    let mut b = SslContext::builder(SslMethod::tls_server()).expect("a server context");
    b.set_min_proto_version(Some(SslVersion::TLS1_2))
        .expect("TLS 1.2");
    b.set_max_proto_version(Some(SslVersion::TLS1_2))
        .expect("TLS 1.2");
    let cert = X509::from_pem(lehu_aa::cert::HU_CERT_PEM.as_bytes()).expect("the certificate");
    let key = PKey::private_key_from_pem(lehu_aa::cert::HU_KEY_PEM.as_bytes()).expect("the key");
    b.set_certificate(&cert).expect("the certificate");
    b.set_private_key(&key).expect("the key");
    let ctx = b.build();
    Tls::from_ssl(Ssl::new(&ctx).expect("a session")).expect("the phone")
}

/// A machine past the version exchange and the TLS handshake.
fn authenticated() -> (Rig, Tls) {
    let mut rig = Rig::new(Tls::new(false).expect("the head unit's TLS"));
    let mut phone = phone_tls();
    rig.drain();
    rig.feed_frames(&plain_message(
        CH_CONTROL,
        MSG_VERSION_RESPONSE,
        &[0, 1, 0, 7, 0, 0],
    ))
    .expect("the version");
    for _ in 0..8 {
        if rig.auth_complete {
            rig.records.clear();
            return (rig, phone);
        }
        let records = std::mem::take(&mut rig.records);
        let mut reply = Vec::new();
        phone
            .accept_step(&records, &mut reply)
            .expect("the phone's handshake");
        if !reply.is_empty() {
            let frames = plain_message(CH_CONTROL, MSG_SSL_HANDSHAKE, &reply);
            rig.feed_frames(&frames).expect("a handshake step");
        }
    }
    panic!("the handshake does not converge");
}

/// The encrypted target's input: messages as `[channel][class][id: u16]
/// [length: u16][body]`, the class byte's low bit set for a sealed one.
fn sealed_input(msgs: &[(u8, bool, u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (ch, sealed, id, body) in msgs {
        out.push(*ch);
        out.push(u8::from(*sealed));
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

#[test]
fn aa_machine_tls() {
    let sdr = proto::ServiceDiscoveryRequest {
        device_name: Some("Pixel 8".into()),
        phone_info: Some(proto::PhoneInfo {
            instance_id: Some("abc123".into()),
        }),
    }
    .encode_to_vec();
    let open = proto::ChannelOpenRequest {
        priority: 0,
        service_id: 3,
    }
    .encode_to_vec();
    let seeds = vec![
        sealed_input(&[
            (CH_CONTROL, true, MSG_SERVICE_DISCOVERY_REQUEST, sdr),
            (CH_VIDEO, true, MSG_CHANNEL_OPEN_REQUEST, open.clone()),
            (CH_SENSOR, true, MSG_CHANNEL_OPEN_REQUEST, open.clone()),
            (
                CH_SENSOR,
                true,
                MSG_SENSOR_REQUEST,
                proto::SensorRequest {
                    sensor_type: 13,
                    min_update_period: 0,
                }
                .encode_to_vec(),
            ),
            (
                CH_VIDEO,
                true,
                MSG_MEDIA_SETUP,
                proto::MediaSetup { codec_type: 7 }.encode_to_vec(),
            ),
            (
                CH_VIDEO,
                true,
                MSG_MEDIA_START,
                proto::MediaStart {
                    session_id: 1,
                    configuration_index: 0,
                }
                .encode_to_vec(),
            ),
            (
                CH_VIDEO,
                true,
                MSG_MEDIA_CODEC_CONFIG,
                vec![0, 0, 0, 1, 0x67, 0x42],
            ),
            (
                CH_VIDEO,
                true,
                MSG_MEDIA_DATA,
                [vec![0; 8], vec![0, 0, 0, 1, 0x65, 1, 2, 3]].concat(),
            ),
            (
                CH_VIDEO,
                true,
                MSG_VIDEO_FOCUS_REQUEST,
                proto::VideoFocusRequestNotification {
                    mode: Some(2),
                    reason: None,
                }
                .encode_to_vec(),
            ),
        ]),
        sealed_input(&[
            (
                CH_CONTROL,
                true,
                MSG_AUDIO_FOCUS_REQUEST,
                proto::AudioFocusRequest {
                    audio_focus_type: 3,
                }
                .encode_to_vec(),
            ),
            (
                CH_CONTROL,
                true,
                MSG_NAV_FOCUS_REQUEST,
                proto::NavFocusRequestNotification {
                    focus_type: Some(2),
                }
                .encode_to_vec(),
            ),
            (
                CH_CONTROL,
                true,
                MSG_VOICE_SESSION_NOTIFICATION,
                proto::VoiceSessionNotification { status: Some(1) }.encode_to_vec(),
            ),
            (
                CH_MEDIA_AUDIO,
                true,
                MSG_MEDIA_START,
                proto::MediaStart {
                    session_id: 2,
                    configuration_index: 0,
                }
                .encode_to_vec(),
            ),
            (CH_MEDIA_AUDIO, true, MSG_MEDIA_DATA, vec![0x11; 200]),
            (
                CH_MIC,
                true,
                MSG_MICROPHONE_REQUEST,
                proto::MicrophoneRequest { open: true }.encode_to_vec(),
            ),
            (CH_INPUT, true, MSG_KEY_BINDING_REQUEST, vec![8, 3]),
            (
                CH_CONTROL,
                false,
                MSG_PING_REQUEST,
                proto::PingRequest { timestamp: 5 }.encode_to_vec(),
            ),
            (
                CH_CONTROL,
                true,
                MSG_BYEBYE_REQUEST,
                proto::ByeByeRequest { reason: 1 }.encode_to_vec(),
            ),
        ]),
        sealed_input(&[(CH_VIDEO, true, MSG_MEDIA_DATA, vec![0x44; 20_000])]),
    ];
    let (mut rig, mut phone) = authenticated();
    let mut rebuilt = 0;
    Target::new("aa_machine_tls")
        .seeds(seeds)
        .max_len(24_000)
        .cost(4)
        .run(|b| {
            let mut at = 0;
            let mut queued = 0;
            let mut fatal = false;
            while at + 6 <= b.len() && !fatal {
                let ch = b[at] % CH_COUNT as u8;
                let sealed = b[at + 1] & 1 != 0;
                let id = u16::from_be_bytes([b[at + 2], b[at + 3]]);
                let len = usize::from(u16::from_be_bytes([b[at + 4], b[at + 5]]));
                at += 6;
                let body = &b[at..(at + len).min(b.len())];
                at += body.len();
                let mut plain = id.to_be_bytes().to_vec();
                plain.extend_from_slice(body);
                let (class, payload) = if sealed {
                    let mut c = Vec::new();
                    phone.encrypt(&plain, &mut c).map_err(|e| e.0)?;
                    (Class::EncSignal, c)
                } else {
                    (Class::Plaintext, plain)
                };
                let frames = frames_of(ch, class, &payload);
                let mut fat = 0;
                while let Some((h, total, range)) = next_frame(&frames, &mut fat) {
                    rig.now += 3;
                    let r = rig.m.on_frame(rig.now, &h, total, &frames[range]);
                    queued += rig.drain();
                    match r {
                        Ok(Some(unit)) => bounded("video unit", unit.len(), b.len(), 1, 0)?,
                        Ok(None) => {}
                        Err(_) => {
                            fatal = true;
                            break;
                        }
                    }
                }
                rig.m.on_tick(rig.now, rig.now * 1_000_000);
                queued += rig.drain();
            }
            if fatal || rig.m.ending() {
                // A goodbye or a fatal message ends the session: the next
                // input gets a fresh one.
                (rig, phone) = authenticated();
                rebuilt += 1;
            }
            bounded("queued answers", queued, b.len(), 256, 8192)
        });
    eprintln!("aa_machine_tls: {rebuilt} sessions rebuilt");
}

#[test]
fn aa_tls_handshake() {
    // A real server flight for the first ClientHello, as a seed.
    let mut client = Tls::new(false).expect("the head unit's TLS");
    let mut hello = Vec::new();
    client.handshake(&[], &mut hello).expect("a ClientHello");
    let mut phone = phone_tls();
    let mut flight = Vec::new();
    phone
        .accept_step(&hello, &mut flight)
        .expect("the server flight");
    let ctx = lehu_aa::tls::client_context(true).expect("the client context");
    Target::new("aa_tls_handshake")
        .seed(flight)
        .seed(vec![0x16, 3, 3, 0, 0])
        .max_len(8192)
        .cost(4)
        .run(|b| {
            let mut tls =
                Tls::from_ssl(Ssl::new(&ctx).map_err(|e| e.to_string())?).map_err(|e| e.0)?;
            let mut out = Vec::new();
            let _ = tls.handshake(&[], &mut out);
            let split = b.len() / 2;
            for part in [&b[..split], &b[split..]] {
                if tls.handshake(part, &mut out).is_err() {
                    break;
                }
            }
            let mut plain = Vec::new();
            let _ = tls.decrypt(b, &mut plain);
            bounded("plaintext before a handshake", plain.len(), b.len(), 1, 0)
        });
}

// ── The Bluetooth handoff ─────────────────────────────────────────────────

fn network() -> Network {
    Network {
        ssid: "LEHU".into(),
        passphrase: "lehu1234".into(),
        bssid: "aa:bb:cc:dd:ee:ff".into(),
        channel: 36,
        ip: "192.168.53.1".into(),
        port: 5277,
    }
}

fn aaw_frame(id: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    aaw::frame_into(&mut out, id, body).expect("fits");
    out
}

fn handoff_seeds() -> Vec<Vec<u8>> {
    let version = aaw::WifiVersionResponse {
        major_version: Some(6),
        minor_version: Some(0),
        device_serial: Some("R58M1234".into()),
        status: Some(0),
        mobile_device_identity: Some(aaw::MobileDeviceIdentity {
            mobile_device_id: Some("abc123".into()),
            connectivity_lifetime_id: None,
            bluetooth_name: Some("Pixel 8".into()),
            bluetooth_address: Some("AA:BB:CC:DD:EE:FF".into()),
        }),
    }
    .encode_to_vec();
    let status = aaw::WifiConnectionStatus {
        status: Some(-3),
        error_message: Some("no".into()),
    }
    .encode_to_vec();
    vec![
        [
            aaw_frame(aaw::MSG_VERSION_RESPONSE, &version),
            aaw_frame(aaw::MSG_INFO_REQUEST, &[]),
            aaw_frame(aaw::MSG_START_RESPONSE, &[8, 0]),
            aaw_frame(aaw::MSG_CONNECTION_STATUS, &status),
        ]
        .concat(),
        [
            aaw_frame(aaw::MSG_PING, &[1, 2, 3]),
            aaw_frame(aaw::MSG_INFO_REQUEST, &[]),
        ]
        .concat(),
    ]
}

#[test]
fn aaw_messages() {
    let mut seeds = handoff_seeds();
    seeds.push(network().info_response().encode_to_vec());
    Target::new("aaw_messages").seeds(seeds).run(|b| {
        if b.len() >= aaw::HEADER_LEN {
            let head: [u8; 4] = b[..4].try_into().map_err(|_| "four bytes")?;
            if let Ok(h) = aaw::parse_head(head) {
                ensure(usize::from(h.len) <= aaw::MAX_BODY, || {
                    "an aaw body over the cap".into()
                })?;
            }
        }
        round_trip::<aaw::WifiVersionResponse>(b)?;
        round_trip::<aaw::WifiStartResponse>(b)?;
        round_trip::<aaw::WifiConnectionStatus>(b)?;
        round_trip::<aaw::WifiInfoResponse>(b)?;
        round_trip::<aaw::WifiVersionRequest>(b)?;
        round_trip::<aaw::WifiStartRequest>(b)
    });
}

#[test]
fn aa_handoff() {
    let net = network();
    Target::new("aa_handoff")
        .seeds(handoff_seeds())
        .max_len(8192)
        .run(|b| {
            // The run loop over a stream that delivers the bytes torn.
            let mut pipe = Pipe::new(b.to_vec(), fingerprint(b));
            let running = AtomicBool::new(true);
            let outcome = handoff::run(&mut pipe, &net, &running);
            bounded("handoff answers", pipe.written.len(), b.len(), 1, 1024)?;
            let id = &outcome.identity;
            bounded(
                "phone identity",
                id.id.len() + id.serial.len() + id.name.len(),
                b.len(),
                1,
                0,
            )?;
            // The machine itself, frame by frame with ids from the input.
            let mut out = Vec::new();
            let mut h = Handoff::begin(&net, 0, &mut out);
            let mut rng = Rng::new(fingerprint(b));
            let mut at = 0;
            while at + 2 <= b.len() {
                let id = u16::from_be_bytes([b[at], b[at + 1]]) % 12;
                let n = rng.below(64).min(b.len() - at - 2);
                h.on_frame(at as u64, id, &b[at + 2..at + 2 + n], &mut out);
                at += 2 + n;
                let _ = h.on_tick(at as u64);
            }
            let _ = (h.step(), h.handed_off(), h.join_status(), h.next_due(0));
            bounded("handoff frames", out.len(), b.len(), 2, 2048)
        });
}
