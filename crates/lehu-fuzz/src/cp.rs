// SPDX-License-Identifier: GPL-3.0-or-later
//! CarPlay: what the phone sends over usbmux, iAP2 and the control and AV
//! channels, and the pair records lockdown reads from disk.

use std::net::Ipv4Addr;

use lehu_cp::carplay_msg::{
    self as msg, MSG_DEVICE_TIME_UPDATE, MSG_REQUEST_AUTH_CHALLENGE_RESPONSE,
};
use lehu_cp::cipher::FrameCipher;
use lehu_cp::file::{ArtTransfer, Step};
use lehu_cp::iap2::{self, CONTROL_ACK, CONTROL_EAK, CONTROL_SESSION, CONTROL_SYN, LinkSync};
use lehu_cp::lockdown::{self, RecordStore};
use lehu_cp::mdns::{self, Service};
use lehu_cp::media;
use lehu_cp::nalu;
use lehu_cp::pairing::{self, MfiSigner, PairSetup, PairVerify, Store};
use lehu_cp::plist::{self, Value};
use lehu_cp::{av, rtsp, tlv8};

use crate::{Pipe, Rng, Target, TempDir, bounded, ensure, fingerprint};

// ── plist ─────────────────────────────────────────────────────────────────

/// A value tree like the phone's SETUP and the lockdown answers.
pub(crate) fn sample_plist() -> Value {
    Value::dict()
        .with(
            "streams",
            Value::Array(vec![
                Value::dict()
                    .with("type", Value::Int(110))
                    .with("streamConnectionID", Value::Int(-6_123_456_789_012))
                    .with("audioFormat", Value::Int(0x40_0000)),
                Value::dict()
                    .with("type", Value::Int(100))
                    .with("audioType", Value::str("media"))
                    .with("latency", Value::Real(0.25)),
            ]),
        )
        .with("name", Value::str("iPhone"))
        .with("wide", Value::str("Ü€𝄞"))
        .with("ok", Value::Bool(true))
        .with("no", Value::Bool(false))
        .with("key", Value::Data((0u8..=255).collect()))
        .with("empty", Value::Data(Vec::new()))
        .with("big", Value::Int(0x1_0000_0000))
        .with("small", Value::Real(f64::from(1.5f32)))
}

/// Values, and the bytes in their strings, keys and data: what decoding
/// allocated.
pub(crate) fn weight(v: &Value) -> usize {
    match v {
        Value::Str(s) => 1 + s.len(),
        Value::Data(d) => 1 + d.len(),
        Value::Array(a) => 1 + a.iter().map(weight).sum::<usize>(),
        Value::Dict(d) => 1 + d.iter().map(|(k, x)| k.len() + weight(x)).sum::<usize>(),
        _ => 1,
    }
}

const XML_WORDS: &[&str] = &[
    "<plist version=\"1.0\">",
    "</plist>",
    "<dict>",
    "</dict>",
    "<dict/>",
    "<key>",
    "</key>",
    "<key/>",
    "<string>",
    "</string>",
    "<string/>",
    "<integer>",
    "</integer>",
    "<real>",
    "</real>",
    "<data>",
    "</data>",
    "<data/>",
    "<array>",
    "</array>",
    "<array/>",
    "<true/>",
    "<false/>",
    "<date>",
    "</date>",
    "<?xml version=\"1.0\"?>",
    "<!DOCTYPE plist>",
    "&amp;",
    "&lt;",
    "&gt;",
    "&quot;",
    "&apos;",
    "-9223372036854775808",
    "9223372036854775808",
    "NaN",
    "inf",
    "1e400",
    "AAAA",
    "====",
    "/",
    ">",
    "<",
];

#[test]
fn plist_binary() {
    let seeds = vec![
        plist::encode(&sample_plist()),
        plist::encode(&Value::Array(vec![Value::Int(1); 20])),
        plist::encode(&Value::str("x")),
        plist::encode(&Value::dict().with("a", Value::Data(vec![7; 300]))),
    ];
    Target::new("plist_binary")
        .seeds(seeds)
        .dict(&["bplist00"])
        .run(|b| {
            let Some(v) = plist::decode(b) else {
                return Ok(());
            };
            // Shared objects are legal (Apple's encoder uniques them), so a
            // value may be referenced more than once; a tree many times the
            // input is the reference blow-up of `bugs::plist_shared_references`.
            bounded("decoded plist", weight(&v), b.len(), 16, 64)?;
            // What decodes re-encodes to something that decodes the same.
            let again = plist::encode(&v);
            let back = plist::decode(&again).ok_or("a re-encoded plist does not decode")?;
            ensure(plist::encode(&back) == again, || {
                "a plist does not survive encode/decode".into()
            })
        });
}

#[test]
fn plist_xml() {
    let seeds = vec![
        plist::encode_xml(&sample_plist()),
        plist::encode_xml(&Value::dict().with("Request", Value::str("QueryType"))),
        b"<plist><dict><key>a</key><integer>-1</integer><key/><data>AAEC\n</data></dict></plist>"
            .to_vec(),
    ];
    Target::new("plist_xml")
        .seeds(seeds)
        .dict(XML_WORDS)
        .run(|b| {
            let Some(v) = plist::decode_xml(b) else {
                return Ok(());
            };
            bounded("decoded XML plist", weight(&v), b.len(), 2, 16)?;
            // One pass may normalise (white space, Null); after it the
            // value is stable.
            let once = plist::decode_xml(&plist::encode_xml(&v))
                .ok_or("an encoded XML plist does not decode")?;
            let text = plist::encode_xml(&once);
            let twice = plist::decode_xml(&text).ok_or("a second pass does not decode")?;
            ensure(plist::encode_xml(&twice) == text, || {
                "an XML plist changes on every pass".into()
            })?;
            // Either form, through the dispatcher lockdown uses.
            let _ = plist::decode_any(b);
            Ok(())
        });
}

#[test]
fn base64() {
    Target::new("base64")
        .seed(plist::base64_encode(b"any carnal pleasure."))
        .seed(plist::base64_encode(&[0xff; 7]))
        .dict(&["=", "==", "\n", " ", "+/"])
        .run(|b| {
            let text = String::from_utf8_lossy(b);
            if let Some(d) = plist::base64_decode(&text) {
                bounded("base64 output", d.len(), text.len(), 1, 0)?;
                let round = plist::base64_decode(&plist::base64_encode(&d));
                ensure(round.as_deref() == Some(&d[..]), || {
                    "base64 does not round trip".into()
                })?;
            }
            Ok(())
        });
}

// ── TLV8 and the pairing ──────────────────────────────────────────────────

#[test]
fn tlv8() {
    let long = vec![0x5a; 600];
    let seeds = vec![
        tlv8::encode(&[(6, &[1])]),
        tlv8::encode(&[(6, &[3]), (3, &[9; 384]), (4, &[1; 64])]),
        tlv8::encode(&[(6, &[5]), (5, &long), (5, &[1, 2])]),
        vec![1, 255],
    ];
    Target::new("tlv8").seeds(seeds).run(|b| {
        let items = tlv8::decode(b);
        let mut kept: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut total = 0;
        for k in 0..=255u8 {
            if let Some(v) = items.get(k) {
                total += v.len();
                kept.push((k, v.to_vec()));
            }
        }
        bounded("tlv8 values", total, b.len(), 1, 0)?;
        // Re-encoded, every item reads back the same.
        let pairs: Vec<(u8, &[u8])> = kept.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        let again = tlv8::decode(&tlv8::encode(&pairs));
        for (k, v) in &kept {
            ensure(again.get(*k) == Some(v.as_slice()), || {
                format!("tlv8 item {k} does not round trip")
            })?;
        }
        Ok(())
    });
}

/// A phone that never pairs: the setup and verify messages, malformed.
fn pairing_seeds() -> Vec<Vec<u8>> {
    let a = vec![0x42u8; 384];
    vec![
        tlv8::encode(&[(0, &[0]), (6, &[1])]),
        tlv8::encode(&[(6, &[3]), (3, &a), (4, &[7; 64])]),
        tlv8::encode(&[(6, &[3]), (3, &[0]), (4, &[7; 64])]),
        tlv8::encode(&[(6, &[5]), (5, &[9; 154])]),
        tlv8::encode(&[(6, &[1]), (3, &[9; 32])]),
        tlv8::encode(&[(6, &[3]), (5, &[1; 120])]),
    ]
}

#[test]
fn pair_setup() {
    let dir = TempDir::new("pair-setup");
    let store = Store::open(dir.path()).expect("an identity");
    let mut setup = PairSetup::default();
    // M1 once, so a mutated M3 reaches the SRP check.
    setup.handle(&tlv8::encode(&[(0, &[0]), (6, &[1])]), &store);
    Target::new("pair_setup")
        .seeds(pairing_seeds())
        .max_len(1024)
        .cost(8)
        .run(|b| {
            let out = setup.handle(b, &store);
            bounded("pair-setup answer", out.len(), b.len(), 1, 1024)?;
            ensure(!setup.complete, || {
                "pair-setup completed for a stranger".into()
            })
        });
}

#[test]
fn pair_verify() {
    let dir = TempDir::new("pair-verify");
    let store = Store::open(dir.path()).expect("an identity");
    let mut verify = PairVerify::default();
    verify.handle(&tlv8::encode(&[(6, &[1]), (3, &[9; 32])]), &store);
    Target::new("pair_verify")
        .seeds(pairing_seeds())
        .max_len(1024)
        .cost(4)
        .run(|b| {
            let out = verify.handle(b, &store);
            bounded("pair-verify answer", out.len(), b.len(), 1, 512)?;
            ensure(!verify.verified && verify.keys().is_none(), || {
                "pair-verify passed for a stranger".into()
            })
        });
}

/// A chip that signs anything, for the /auth-setup handshake.
struct FakeChip(u8);

impl MfiSigner for FakeChip {
    fn certificate(&mut self) -> Option<Vec<u8>> {
        Some(vec![0x30; 608])
    }
    fn sign(&mut self, digest: &[u8]) -> Option<Vec<u8>> {
        Some([digest, digest].concat())
    }
    fn protocol_major(&self) -> u8 {
        self.0
    }
}

#[test]
fn auth_setup() {
    let mut seed = vec![1u8];
    seed.extend_from_slice(&[9; 32]);
    Target::new("auth_setup")
        .seed(seed)
        .max_len(128)
        .cost(4)
        .run(|b| {
            let mut chip = FakeChip(if b.len() % 2 == 0 { 2 } else { 3 });
            if let Some(out) = pairing::auth_setup(b, &mut chip) {
                ensure(b.len() == 33 && b[0] == 1, || {
                    "/auth-setup took a bad request".into()
                })?;
                let sig = if chip.0 == 2 { 40 } else { 64 };
                ensure(out.len() == 32 + 4 + 608 + 4 + sig, || {
                    format!("/auth-setup answered {} bytes", out.len())
                })?;
            }
            Ok(())
        });
}

// ── RTSP and the sealed control frames ────────────────────────────────────

#[test]
fn rtsp_requests() {
    let mut setup = b"SETUP rtsp://fe80::1/123 RTSP/1.0\r\nContent-Type: application/x-apple-binary-plist\r\nCSeq: 4\r\nContent-Length: ".to_vec();
    let body = plist::encode(&sample_plist());
    setup.extend_from_slice(format!("{}\r\n\r\n", body.len()).as_bytes());
    setup.extend_from_slice(&body);
    let seeds = vec![
        b"GET /info RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec(),
        b"POST /pair-setup RTSP/1.0\r\nContent-Length: 3\r\nContent-Type: application/pairing+tlv8\r\nCSeq: 2\r\n\r\n\x06\x01\x01GET /info RTSP/1.0\r\n\r\n".to_vec(),
        b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 0\r\n\r\n".to_vec(),
        setup,
    ];
    Target::new("rtsp_requests")
        .seeds(seeds)
        .dict(&[
            "\r\n",
            "\r\n\r\n",
            "Content-Length: ",
            "content-length:",
            "CSeq: ",
            ": ",
            "RTSP/1.0",
            "4294967296",
            "18446744073709551616",
            "1048576",
            "1048577",
            "-1",
            "+7",
            " ",
        ])
        .run(|b| {
            let mut whole = b.to_vec();
            let all = rtsp::parse(&mut whole);
            if let Some(reqs) = &all {
                let bodies: usize = reqs.iter().map(|r| r.body.len()).sum();
                bounded("RTSP bodies", bodies, b.len(), 1, 0)?;
                ensure(reqs.iter().all(|r| r.body.len() <= rtsp::MAX_BODY), || {
                    "an RTSP body over the cap".into()
                })?;
                for r in reqs {
                    let res = rtsp::Response::ok().with_body("text/plain", r.body.clone());
                    let _ = rtsp::build(r, &res);
                    let _ = (r.path_only(), r.is_response());
                }
                // Fed in torn reads, the stream gives the same requests.
                let mut rng = Rng::new(fingerprint(b));
                let mut buf = Vec::new();
                let mut got = Vec::new();
                let mut at = 0;
                while at < b.len() {
                    let n = (1 + rng.below(40)).min(b.len() - at);
                    buf.extend_from_slice(&b[at..at + n]);
                    at += n;
                    got.extend(rtsp::parse(&mut buf).ok_or("a torn read is malformed")?);
                }
                ensure(&got == reqs && buf == whole, || {
                    "torn reads parse differently".into()
                })?;
            }
            Ok(())
        });
}

#[test]
fn control_frames() {
    let (a, b_key) = ([0x11u8; 32], [0x22u8; 32]);
    let mut phone = FrameCipher::new(&b_key, &a);
    let one = phone.encrypt(b"GET /info RTSP/1.0\r\nCSeq: 5\r\n\r\n");
    let two = [phone.encrypt(&[0x55; 300]), phone.encrypt(b"")].concat();
    Target::new("control_frames")
        .seed(one)
        .seed(two)
        .max_len(2048)
        .run(|b| {
            // Each input on a fresh channel: the counters start at zero.
            let mut ours = FrameCipher::new(&a, &b_key);
            let mut sealed = b.to_vec();
            let mut plain = Vec::new();
            if ours.decrypt(&mut sealed, &mut plain) {
                bounded("opened frames", plain.len() + sealed.len(), b.len(), 1, 0)?;
            }
            Ok(())
        });
}

// ── iAP2 ──────────────────────────────────────────────────────────────────

fn csm_seeds() -> Vec<Vec<u8>> {
    vec![
        msg::wired_identification("LEHU", "F1C200s", "LEHU"),
        msg::carplay_availability("aa:bb:cc:dd:ee:ff"),
        iap2::pack_csm(
            MSG_DEVICE_TIME_UPDATE,
            &[
                (0, &2_771_229_404i64.to_be_bytes()),
                (1, &60i16.to_be_bytes()),
                (2, &[0]),
            ],
        ),
        iap2::pack_csm(MSG_REQUEST_AUTH_CHALLENGE_RESPONSE, &[(0, &[0xab; 32])]),
        iap2::pack_csm(0x1d03, &[(5, &[]), (12, &[])]),
    ]
}

#[test]
fn iap2_packet() {
    let seeds = vec![
        iap2::pack_packet(CONTROL_SYN, 7, 0, 0, &LinkSync::proposal().pack()),
        iap2::pack_packet(CONTROL_ACK, 8, 3, CONTROL_SESSION, &csm_seeds()[2]),
        iap2::pack_packet(CONTROL_ACK, 1, 2, 0, &[]),
        iap2::pack_packet(CONTROL_EAK, 1, 2, 0, &[4]),
    ];
    Target::new("iap2_packet").seeds(seeds).run(|b| {
        if let Some((h, payload)) = iap2::parse_packet(b) {
            ensure(usize::from(h.length) == b.len(), || {
                "a packet of the wrong length".into()
            })?;
            let again = iap2::pack_packet(h.control, h.seq, h.ack, h.session, payload);
            ensure(again == b, || "a packet does not re-pack to itself".into())?;
            if let Some(sync) = LinkSync::parse(payload) {
                bounded(
                    "link sessions",
                    sync.sessions.len() * 3,
                    payload.len(),
                    1,
                    0,
                )?;
            }
        }
        Ok(())
    });
}

#[test]
fn iap2_csm() {
    Target::new("iap2_csm")
        .seeds(csm_seeds())
        // bugs::device_time_overflows.
        .run(|b| {
            if let Some((id, params)) = iap2::parse_csm(b) {
                let length = usize::from(u16::from_be_bytes([b[2], b[3]]));
                let total: usize = params.iter().map(|(_, v)| v.len()).sum();
                bounded("CSM parameters", total, length, 1, 0)?;
                ensure(iap2::pack_csm(id, &params) == b[..length], || {
                    "a CSM does not re-pack to itself".into()
                })?;
                for (_, v) in &params {
                    // A group parameter is a parameter list itself.
                    let _ = iap2::parse_params(v);
                }
            }
            if let Some(t) = msg::device_time_update(b) {
                let _ = t.unix_ms();
            }
            if let Some(c) = msg::auth_challenge(b) {
                bounded("auth challenge", c.len(), b.len(), 1, 0)?;
            }
            Ok(())
        });
}

/// A now playing update naming the art's transfer, then the transfer in
/// datagrams, each led by its length.
fn art_seed() -> Vec<u8> {
    let item = iap2::pack_params(&[(1, b"Song\0"), (12, b"Artist\0"), (26, &[3])]);
    let update = iap2::pack_csm(media::MSG_NOW_PLAYING_UPDATE, &[(0, &item)]);
    let mut s = vec![update.len() as u8];
    s.extend_from_slice(&update);
    for d in [
        &[3, 0x04, 0, 0, 0, 0, 0, 0, 0, 9][..],
        &[3, 0x80, 0xff, 0xd8, 0xff],
        &[3, 0x00, 1, 2, 3, 4],
        &[3, 0x40, 0xd9],
    ] {
        s.push(d.len() as u8);
        s.extend_from_slice(d);
    }
    s
}

#[test]
fn cp_album_art() {
    Target::new("cp_album_art")
        .seed(art_seed())
        .dict_bytes(&[&[0x04], &[0x80], &[0x40], &[0xc0], &[0x02], &[0, 26]])
        .run(|b| {
            // The first piece is a now playing update; the rest are the
            // file transfer's datagrams.
            let mut np = lehu_session::NowPlaying::default();
            let mut art = ArtTransfer::default();
            let mut taken = 0;
            for (k, piece) in pieces(b).enumerate() {
                if k == 0 {
                    if let Some(news) = media::now_playing_update(piece, &mut np, 0) {
                        art.expect(news.transfer);
                    }
                    continue;
                }
                match art.feed(piece) {
                    Step::Done(_) => {
                        let file = std::mem::take(art.file());
                        bounded("album art", file.len(), b.len(), 1, 0)?;
                        ensure(file.len() <= lehu_session::MAX_ARTWORK, || {
                            "art past the bound".into()
                        })?;
                        taken += file.len();
                    }
                    Step::Reply(_) | Step::Nothing => {}
                }
            }
            bounded("album art taken", taken, b.len(), 1, 0)
        });
}

/// Length-led pieces of `b`, the last cut short if it runs out.
fn pieces(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = b;
    std::iter::from_fn(move || {
        let (&n, tail) = rest.split_first()?;
        let n = usize::from(n).min(tail.len());
        let (piece, after) = tail.split_at(n);
        rest = after;
        Some(piece)
    })
}

/// The phone's side of a link: its SYN with an ACK, then data packets.
fn link_stream(csms: &[Vec<u8>]) -> Vec<u8> {
    let mut s = iap2::DETECT_MARKER.to_vec();
    s.extend(iap2::pack_packet(
        CONTROL_SYN | CONTROL_ACK,
        40,
        100,
        0,
        &LinkSync::proposal().pack(),
    ));
    for (k, c) in csms.iter().enumerate() {
        s.extend(iap2::pack_packet(
            CONTROL_ACK,
            41 + k as u8,
            100,
            CONTROL_SESSION,
            c,
        ));
    }
    s
}

#[test]
fn iap2_link() {
    let csms = csm_seeds();
    let seeds = vec![
        link_stream(&csms),
        link_stream(&csms[..1]),
        [
            link_stream(&[]),
            iap2::pack_packet(CONTROL_EAK, 9, 9, 0, &[1]),
            vec![0xff, 0x5a, 0xff, 0xff],
        ]
        .concat(),
        // The album art on the file transfer session.
        [
            link_stream(&csms[..1]),
            iap2::pack_packet(CONTROL_ACK, 42, 100, 12, &[3, 0x04]),
            iap2::pack_packet(CONTROL_ACK, 43, 100, 12, &[3, 0xc0, 0xff, 0xd8]),
        ]
        .concat(),
    ];
    Target::new("iap2_link")
        .seeds(seeds)
        .max_len(8192)
        .cost(2)
        .run(|b| {
            let pipe = Pipe::new(b.to_vec(), fingerprint(b));
            let mut link = iap2::Link::new(pipe);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut delivered = 0;
            if link.negotiate(deadline).is_ok() {
                let mut out = Vec::new();
                let mut art = ArtTransfer::default();
                loop {
                    match link.recv_control(&mut out) {
                        Ok(iap2::Received::Control) => {
                            let _ = iap2::parse_csm(&out);
                        }
                        Ok(iap2::Received::File) => {
                            let _ = art.feed(&out);
                        }
                        _ => break,
                    }
                    delivered += out.len();
                }
            }
            bounded("delivered CSMs", delivered, b.len(), 1, 0)?;
            bounded(
                "written by the link",
                link.stream_mut().written.len(),
                b.len(),
                1,
                128,
            )
        });
}

// ── Video framing ─────────────────────────────────────────────────────────

fn avcc_record() -> Vec<u8> {
    let mut rec = vec![1, 0x42, 0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0x00, 0x1e];
    rec.extend_from_slice(&[1, 0, 4, 0x68, 0xce, 0x38, 0x80]);
    rec
}

fn hvcc_record() -> Vec<u8> {
    let mut rec = vec![0u8; 22];
    rec.push(3);
    rec.extend_from_slice(&[0xa0, 0, 1, 0, 2, 0x40, 0x01]);
    rec.extend_from_slice(&[0xa1, 0, 1, 0, 3, 0x42, 0x01, 0x01]);
    rec.extend_from_slice(&[0xa2, 0, 2, 0, 1, 0x44, 0, 1, 0x44]);
    rec
}

#[test]
fn nalu_config() {
    let mut wrapped_avc = vec![0u8; 8];
    wrapped_avc.extend_from_slice(b"avc1");
    wrapped_avc.extend_from_slice(&[0; 78]);
    wrapped_avc.extend_from_slice(b"avcC");
    wrapped_avc.extend(avcc_record());
    let mut wrapped_hevc = vec![0u8; 8];
    wrapped_hevc.extend_from_slice(b"hvcC");
    wrapped_hevc.extend(hvcc_record());
    Target::new("nalu_config")
        .seeds([avcc_record(), hvcc_record(), wrapped_avc, wrapped_hevc])
        .dict(&["avcC", "hvcC"])
        .run(|b| {
            let (out, _) = nalu::config_to_annexb(b);
            // Each parameter set grows by two bytes: a 2-byte length
            // becomes a 4-byte start code.
            bounded("parameter sets", out.len(), b.len(), 2, 0)
        });
}

#[test]
fn nalu_frame() {
    let frame = [
        0, 0, 0, 3, 0x65, 0x88, 0x84, 0, 0, 0, 1, 0x41, 0, 0, 0, 2, 0x06, 0x05,
    ]
    .to_vec();
    Target::new("nalu_frame")
        .seed(frame)
        .seed(vec![0, 0, 0, 1, 0x09])
        .run(|b| {
            let mut f = b.to_vec();
            if !nalu::frame_to_annexb_in_place(&mut f) {
                return ensure(f == b, || "a refused frame was changed".into());
            }
            // Accepted: every length prefix is a start code now, the rest
            // untouched.
            let mut off = 0;
            while off + 4 <= b.len() {
                let n = u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as usize;
                ensure(f[off..off + 4] == [0, 0, 0, 1], || {
                    "a prefix not rewritten".into()
                })?;
                ensure(f[off + 4..off + 4 + n] == b[off + 4..off + 4 + n], || {
                    "a NAL changed".into()
                })?;
                off += 4 + n;
            }
            ensure(off == b.len(), || "an accepted chain with a tail".into())
        });
}

// ── mDNS, the audio formats, the pair records ─────────────────────────────

fn service() -> Service {
    Service {
        instance: "LEHU".into(),
        service_type: "_airplay._tcp".into(),
        hostname: "lehu.local".into(),
        port: 7000,
        ipv4: Ipv4Addr::new(192, 168, 53, 1),
        txt: vec![
            "deviceid=AA:BB:CC:DD:EE:FF".into(),
            "features=0x5A7FFFF7,0x1E".into(),
        ],
    }
}

fn query(names: &[&str]) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0, 0, 0, names.len() as u8, 0, 0, 0, 0, 0, 0];
    for (k, name) in names.iter().enumerate() {
        if k > 0 && name.is_empty() {
            // A compression pointer to the first question's name.
            q.extend_from_slice(&[0xc0, 12]);
        } else {
            for label in name.split('.') {
                q.push(label.len() as u8);
                q.extend_from_slice(label.as_bytes());
            }
            q.push(0);
        }
        q.extend_from_slice(&[0, 12, 0, 1]);
    }
    q
}

#[test]
fn mdns_query() {
    let s = service();
    let seeds = vec![
        query(&["_airplay._tcp.local"]),
        query(&["_raop._tcp.local", ""]),
        query(&["something.else", "LEHU._airplay._tcp.local", "lehu.local"]),
        mdns::build_answer(0, &s),
    ];
    Target::new("mdns_query")
        .seeds(seeds)
        // `read_name` underflows on a pointer backwards: bugs::mdns_pointer_back.
        .max_len(1500)
        .dict_bytes(&[
            b"\xc0\x0c",
            b"\x05local",
            b"\x08_airplay",
            b"\x04_tcp",
            b"\x04LEHU",
        ])
        .run(|b| {
            if let Some(id) = mdns::asks_for(b, &s) {
                ensure(id == u16::from_be_bytes([b[0], b[1]]), || {
                    "the wrong query id".into()
                })?;
            }
            Ok(())
        });
}

#[test]
fn audio_format_word() {
    let seeds = [
        0x40_0000i64,
        0x80_0000,
        0x7000_0000,
        0x4,
        0x800,
        1 << 40,
        -1,
    ]
    .map(|v| v.to_be_bytes().to_vec());
    Target::new("audio_format_word")
        .seeds(seeds)
        .max_len(16)
        .run(|b| {
            let mut word = [0u8; 8];
            for (d, s) in word.iter_mut().zip(b) {
                *d = *s;
            }
            let fmt = av::audio_format_of(i64::from_be_bytes(word));
            ensure(matches!(fmt.channels, 1 | 2) && fmt.rate > 0, || {
                format!("an unplayable stream format {fmt:?}")
            })?;
            let _ = av::audio_kind_of(i64::from_be_bytes(word), &String::from_utf8_lossy(b));
            Ok(())
        });
}

#[test]
fn lockdown_records() {
    let dir = TempDir::new("lockdown");
    let store = RecordStore::new(dir.path());
    let records = dir.path().join("lockdown");
    std::fs::create_dir_all(&records).expect("the record directory");
    let record = Value::dict()
        .with("HostID", Value::str("5AC7A8E4-1D35-4B0C-B6E2-0123456789AB"))
        .with(
            "RootCertificate",
            Value::Data(b"-----BEGIN CERTIFICATE-----\n".to_vec()),
        )
        .with(
            "RootPrivateKey",
            Value::Data(b"-----BEGIN PRIVATE KEY-----\n".to_vec()),
        )
        .with(
            "SystemBUID",
            Value::str("00000000-0000-4000-8000-000000000000"),
        );
    Target::new("lockdown_records")
        .seed(plist::encode_xml(&record))
        .seed(plist::encode(&record))
        .dict(XML_WORDS)
        .cost(2)
        .run(|b| {
            let serial = "00008110001A2B3C4D5E6F70";
            std::fs::write(
                records.join(format!("{}.plist", lockdown::dashed(serial))),
                b,
            )
            .map_err(|e| e.to_string())?;
            if let Some(r) = store.read(serial) {
                let _ = lockdown::record_usable(&r);
                bounded("pair record", weight(&r), b.len(), 16, 64)?;
            }
            Ok(())
        });
}

#[test]
fn usb_serials() {
    Target::new("usb_serials")
        .seed("00008110001A2B3C4D5E6F70")
        .seed("0123456789abcdef0123456789abcdef01234567")
        .dict(&["-", "é", "€", "𝄞"])
        .max_len(64)
        // bugs::dashed_splits_a_character.
        .run(|b| {
            let s = String::from_utf8_lossy(b);
            let d = lockdown::dashed(&s);
            ensure(d.len() <= s.len() + 1, || "a dashed serial grew".into())
        });
}
