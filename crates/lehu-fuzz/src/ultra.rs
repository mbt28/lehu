// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! CarPlay Ultra: the MFi 4 messages and the phone's compressed
//! certificates, the MFi 4 session's first steps, OPACK, the vehicle state
//! and UI sync messages, the data streams' packages and the theme file.

use lehu_cp::mfi4::{self, cert, wire};
use lehu_cp::opack::{self, Value};
use lehu_cp::pairing::MfiSigner;
use lehu_cp::plist;
use lehu_cp::ui_sync::{Layout, UiSync};
use lehu_cp::ultra::{self, Theme};
use lehu_cp::vehicle::VehicleState;

use crate::{Target, TempDir, bounded, ensure};

/// The phone's setup and start, the shapes the session sees first.
fn mfi4_seeds() -> Vec<Vec<u8>> {
    let setup = wire::Message::new(wire::CLEAR, 0x5100)
        .with(0, [2])
        .with(1, [3])
        .with(2, [1])
        .with(3, [1])
        .with(3, [2])
        .with(4, [7]);
    let start = wire::Message::new(wire::CLEAR, 0x5111)
        .with(0, 0x1357u16.to_be_bytes())
        .with(1, [3; 33])
        .with(2, vec![0x42; 112]);
    let tunnel = wire::Message::new(wire::TUNNELED, 0x5500)
        .with(0, [1])
        .with(1, b"\x06\x01\x01".to_vec());
    vec![
        setup.encode(),
        start.encode(),
        tunnel.encode(),
        wire::Message::new(0x1234, 0x5113).with(0, [6]).encode(),
        wire::Message::new(wire::CLEAR, 0x51FE).encode(),
    ]
}

#[test]
fn mfi4_wire() {
    Target::new("mfi4_wire")
        .seeds(mfi4_seeds())
        .max_len(1024)
        .run(|b| {
            let Ok(m) = wire::decode(b) else {
                return Ok(());
            };
            let total: usize = m.params.iter().map(|(_, v)| v.len()).sum();
            bounded("mfi4 parameters", total, b.len(), 1, 0)?;
            // What decodes encodes back to the same bytes.
            ensure(m.encode() == b, || {
                "an MFi 4 message does not round trip".into()
            })
        });
}

/// A compressed leaf for P-256's generator, and an intermediate.
fn cert_seeds() -> Vec<Vec<u8>> {
    let g = crate::unhex("036b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296");
    let mut leaf = vec![0xA2u8];
    leaf.extend(b"ABCDEFGH");
    leaf.extend(b"20250101000000");
    leaf.extend(b"20510101000000");
    leaf.extend(&g);
    leaf.extend([0x5a; 31]);
    leaf.push(3);
    leaf.extend([0x77; 20]);
    leaf.extend([0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01]);
    leaf.extend([0x30, 0x03, 0x04, 0x01, 0x00]);
    let mut mid = vec![0xA5u8];
    mid.extend(b"ROOTNAME");
    mid.extend(b"20200101000000");
    mid.extend(b"20400101000000");
    mid.extend(b"SUBJECTN");
    mid.extend(&g);
    mid.extend([0x44; 20]);
    mid.extend([0x88; 36]);
    mid.extend([0x30, 0x06, 0x02, 0x01, 0x02, 0x02, 0x01, 0x03]);
    vec![leaf, mid]
}

#[test]
fn mfi4_certificate() {
    Target::new("mfi4_certificate")
        .seeds(cert_seeds())
        .max_len(512)
        .cost(4)
        .run(|b| {
            if let Ok(u) = cert::unpack(b) {
                // The DER is the fields rebuilt: a fixed frame around what
                // came in, the key twice as long uncompressed.
                bounded("certificate DER", u.der.len(), b.len(), 3, 1024)?;
            }
            Ok(())
        });
}

/// A chip that signs anything with 64 bytes.
struct FakeChip;

impl MfiSigner for FakeChip {
    fn certificate(&mut self) -> Option<Vec<u8>> {
        Some(vec![0x30; 608])
    }
    fn sign(&mut self, digest: &[u8]) -> Option<Vec<u8>> {
        Some([digest, digest].concat())
    }
    fn protocol_major(&self) -> u8 {
        3
    }
}

#[test]
fn mfi4_session() {
    let dir = TempDir::new("mfi4");
    let setup = mfi4_seeds().remove(0);
    Target::new("mfi4_session")
        .seeds(mfi4_seeds())
        .max_len(1024)
        .cost(16)
        .run(|b| {
            // A fresh session, and one past the setup, so a start reaches
            // the key derivation and the seal.
            let mut tunnel = |d: &[u8]| -> Result<Vec<u8>, String> { Ok(d.to_vec()) };
            for warm in [false, true] {
                let mut s = mfi4::Session::new(dir.path());
                s.binds_pairing = true;
                if warm {
                    s.handle(&setup, &mut FakeChip, &mut tunnel)
                        .map_err(|e| format!("the setup seed: {e}"))?;
                }
                if let Ok(out) = s.handle(b, &mut FakeChip, &mut tunnel) {
                    bounded("mfi4 answer", out.len(), 0, 0, 1024)?;
                }
                ensure(!s.proven(), || "a stranger proved itself".into())?;
            }
            Ok(())
        });
}

fn opack_seeds() -> Vec<Vec<u8>> {
    let request = opack::dict([
        ("command", Value::str("registerRequest")),
        ("transactionID", Value::Int(77)),
        (
            "values",
            Value::Array(vec![Value::Int(264), Value::Int(-6727), Value::Real(1.5)]),
        ),
        ("key", Value::Data(vec![1; 40])),
        ("id", Value::Uuid([9; 16])),
    ]);
    vec![
        opack::encode(&request),
        vec![0xdf, 0x41, b'k', 0xa0, 0xa0, 0xc1, 0x00, 0x03],
        vec![0xef, 0x41, b'a', 0x08, 0x03],
    ]
}

#[test]
fn opack_decode() {
    Target::new("opack")
        .seeds(opack_seeds())
        .dict_bytes(&[&[0xa0], &[0xc1, 0x00], &[0xdf], &[0xef], &[0x03], &[0x64]])
        .max_len(2048)
        .run(|b| {
            if let Some(v) = opack::decode(b) {
                bounded("opack values", v.weight(), b.len(), 6, 4096)?;
                // Compared as bytes: a NaN decodes, and is never equal
                // to itself.
                let bytes = opack::encode(&v);
                let again = opack::decode(&bytes).map(|w| opack::encode(&w));
                ensure(again.as_deref() == Some(&bytes[..]), || {
                    "an OPACK value does not round trip".into()
                })?;
            }
            Ok(())
        });
}

fn vehicle_request(command: &str, values: Value) -> Vec<u8> {
    let inner = opack::dict([
        ("command", Value::str(command)),
        ("transactionID", Value::Int(5)),
        ("values", values),
    ]);
    opack::encode(&opack::dict([
        ("pluginID", Value::Int(0)),
        ("pluginData", Value::Data(opack::encode(&inner))),
    ]))
}

#[test]
fn vehicle_state() {
    let mut state = VehicleState::new("LEHU", "1.0");
    let seeds = vec![
        vehicle_request("configRequest", Value::Null),
        vehicle_request("registerRequest", Value::Array(vec![Value::Int(264)])),
        vehicle_request(
            "writeRequest",
            Value::Dict(vec![(Value::Int(262), Value::Bool(true))]),
        ),
        vehicle_request(
            "controlRequest",
            Value::Dict(vec![(Value::Int(9), Value::Null)]),
        ),
    ];
    Target::new("vehicle_state")
        .seeds(seeds)
        .max_len(2048)
        .run(|b| {
            let out = state.handle(b);
            let total: usize = out.iter().map(Vec::len).sum();
            // The description goes out whatever came in; the rest echoes.
            bounded("vehicle state answers", total, b.len(), 4, 8192)
        });
}

#[test]
fn ui_sync() {
    let frame = |header: plist::Value| {
        opack::encode(&opack::dict([
            ("displayUUID", Value::str("panel")),
            ("data", Value::Data(plist::encode(&header))),
        ]))
    };
    let h = |kind: i64| {
        plist::Value::dict()
            .with("type", plist::Value::Int(kind))
            .with("ssn", plist::Value::Int(1))
            .with("psn", plist::Value::Int(1))
    };
    let mut sync = UiSync::new(Some(Layout {
        id: "L".into(),
        palette: "P".into(),
    }));
    Target::new("ui_sync")
        .seeds(vec![frame(h(0)), frame(h(1)), frame(h(2)), frame(h(3))])
        .max_len(1024)
        .run(|b| {
            let out = sync.handle(b);
            ensure(out.len() <= 4, || {
                format!("{} answers to one frame", out.len())
            })?;
            let total: usize = out.iter().map(Vec::len).sum();
            bounded("ui sync answers", total, b.len(), 8, 4096)
        });
}

#[test]
fn ultra_packages() {
    let seed = [
        ultra::package(0x7379_6e63, 0x636d_6e64, 9, b"one"),
        ultra::package(0x6173_796e, 0, 0, b""),
    ]
    .concat();
    Target::new("ultra_packages")
        .seed(seed)
        .max_len(512)
        .run(|b| {
            let mut plain = b.to_vec();
            let mut total = 0;
            while let Ok(Some(p)) = ultra::next_package(&mut plain) {
                total += p.body.len();
            }
            bounded("package bodies", total, b.len(), 1, 0)
        });
}

#[test]
fn ultra_theme() {
    let seed = b"# a theme\nasset=44C9FA31EC\nversion=985\nsdk=19.0\ncenter=Center_Display\nlayout=L1\npalette=blue\n";
    Target::new("ultra_theme")
        .seed(seed.to_vec())
        .dict(&["=", "\n", "#", "version", "layout", "é"])
        .max_len(512)
        .run(|b| {
            if let Ok(t) = Theme::parse(&String::from_utf8_lossy(b)) {
                ensure(
                    !t.asset.is_empty() && t.asset.len() <= 64 && !t.center.is_empty(),
                    || "a theme with an empty or long id".into(),
                )?;
            }
            Ok(())
        });
}
