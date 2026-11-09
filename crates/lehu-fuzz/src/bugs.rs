// SPDX-License-Identifier: GPL-3.0-or-later
//! What the targets found, one test each with the input that triggers it.
//! They state the behaviour wanted and fail today, so they are ignored;
//! `cargo test -p lehu-fuzz -- --ignored` runs them. A fix turns one
//! green: then drop its `#[ignore]` and the guard its target keeps.

use std::panic;
use std::time::Instant;

use lehu_audio::{Format, Segment, decode_pcm_be};
use lehu_core::devices::{MAX_KNOWN, Registry};
use lehu_cp::carplay_msg::{MSG_DEVICE_TIME_UPDATE, device_time_update};
use lehu_cp::iap2::pack_csm;
use lehu_cp::lockdown;
use lehu_cp::mdns::{self, Service};
use lehu_cp::pairing::Store;
use lehu_cp::plist;

use crate::cp::weight;
use crate::{BUDGET, TempDir, hex};

/// A binary plist whose objects each hold two references to the next: a
/// few bytes per level, twice the values per level. `MAX_DEPTH` (32) stops
/// a loop, not this: 33 levels are 140 bytes and 2^33 values.
fn shared_references(levels: usize) -> Vec<u8> {
    let mut b = b"bplist00".to_vec();
    let mut offsets = Vec::new();
    for i in 0..levels - 1 {
        offsets.push(b.len() as u8);
        b.extend_from_slice(&[0xa2, (i + 1) as u8, (i + 1) as u8]);
    }
    offsets.push(b.len() as u8);
    b.extend_from_slice(&[0x10, 0x01]);
    let table = b.len() as u64;
    b.extend_from_slice(&offsets);
    b.extend_from_slice(&[0; 6]);
    b.extend_from_slice(&[1, 1]);
    b.extend_from_slice(&(levels as u64).to_be_bytes());
    b.extend_from_slice(&0u64.to_be_bytes());
    b.extend_from_slice(&table.to_be_bytes());
    b
}

/// lehu-cp, `plist::decode` (plist.rs, `Decoder::parse`, the array and
/// dictionary arms at lines 382-407): a reference is followed every time
/// it appears, so shared objects are decoded once per path to them. 18
/// levels, 111 bytes, decode to 262 143 values; 33 levels would be 2^33,
/// a hang and an out-of-memory from one SETUP body. Every binary plist the
/// phone sends (the AV session's SETUP, the commands) goes through it.
#[test]
fn plist_shared_references() {
    // Fixed by a node budget: such a plist is refused, quickly; a small
    // sharing is still decoded.
    for levels in [3, 18, 33] {
        let b = shared_references(levels);
        let start = Instant::now();
        let v = plist::decode(&b);
        let took = start.elapsed();
        let w = v.as_ref().map_or(0, weight);
        assert!(
            w <= 16 * b.len() + 64 && took <= BUDGET,
            "{} bytes decode to {w} values in {took:?}: {}",
            b.len(),
            hex(&b)
        );
    }
    assert!(plist::decode(&shared_references(3)).is_some());
}

/// lehu-cp, `DeviceTime::unix_ms` (carplay_msg.rs:112): `unix_secs * 1000`
/// overflows for any time past year 292 471 208; the phone's eight bytes
/// are taken as they come. A debug build panics on the iAP2 thread; the
/// release build wraps and the loop is handed a nonsense clock, which it
/// sets the system's to.
#[test]
fn device_time_overflows() {
    // Fixed by refusing a time outside 2000..2100; a real one still
    // parses.
    let csm = pack_csm(MSG_DEVICE_TIME_UPDATE, &[(0, &i64::MAX.to_be_bytes())]);
    let ms = panic::catch_unwind(|| device_time_update(&csm).map(|t| t.unix_ms()));
    assert!(matches!(ms, Ok(None)), "not refused: {}", hex(&csm));
    let now: i64 = 1_792_920_166;
    let csm = pack_csm(MSG_DEVICE_TIME_UPDATE, &[(0, &now.to_be_bytes())]);
    assert_eq!(
        device_time_update(&csm).map(|t| t.unix_ms()),
        Some(now * 1000)
    );
}

/// lehu-cp, `lockdown::dashed` (lockdown.rs:137): a 24-byte serial is cut
/// at byte 8 without checking that it is a character boundary. The serial
/// is the phone's USB descriptor string, as sysfs gives it.
#[test]
fn dashed_splits_a_character() {
    let serial = "0000000\u{e9}000000000000000";
    assert_eq!(serial.len(), 24);
    let r = panic::catch_unwind(|| lockdown::dashed(serial));
    assert!(r.is_ok(), "dashed panicked on {}", hex(serial.as_bytes()));
}

/// lehu-cp, `unhex` in pairing.rs (line 55, `&s[i..i + 2]`): hex is sliced
/// by bytes, so a non-ASCII character in identity.txt panics
/// `Store::open`, which the CarPlay session calls at every start.
#[test]
fn pairing_store_hex_splits_a_character() {
    let dir = TempDir::new("bug-identity");
    let text = "priv=0\u{e9}0\npub=00\npi=x\n";
    std::fs::write(dir.path().join("identity.txt"), text).expect("written");
    let path = dir.path().to_path_buf();
    let r = panic::catch_unwind(move || Store::open(&path).map(|_| ()));
    assert!(
        r.is_ok(),
        "Store::open panicked on identity.txt {}",
        hex(text.as_bytes())
    );
}

/// The same `unhex`, through `Store::pairing` and pairings.txt, which
/// pair-verify reads for the phone's long-term key.
#[test]
fn pairing_file_hex_splits_a_character() {
    let dir = TempDir::new("bug-pairings");
    let store = Store::open(dir.path()).expect("an identity");
    let text = "PHONE-1 0\u{e9}0\n";
    std::fs::write(dir.path().join("pairings.txt"), text).expect("written");
    let r = panic::catch_unwind(panic::AssertUnwindSafe(|| store.pairing("PHONE-1")));
    assert!(
        r.is_ok(),
        "Store::pairing panicked on pairings.txt {}",
        hex(text.as_bytes())
    );
}

/// lehu-cp, `read_name` in mdns.rs (line 141): the fallback in
/// `consumed.unwrap_or(p + 1 - off)` is computed eagerly, and after a
/// compression pointer back to an earlier name `p < off`: the subtraction
/// overflows. A debug
/// build panics on any query that names its second question by a pointer
/// to its first, which is how phones compress them; a release build wraps
/// and throws the value away, so the board is not affected today.
#[test]
fn mdns_pointer_back() {
    let service = Service {
        instance: "LEHU".into(),
        service_type: "_airplay._tcp".into(),
        hostname: "lehu.local".into(),
        port: 7000,
        ipv4: std::net::Ipv4Addr::new(192, 168, 53, 1),
        txt: Vec::new(),
    };
    let q = crate::unhex(
        "1234 0000 0002 0000 0000 0000 055f72616f70 045f746370 056c6f63616c 00 000c 0001 c00c 000c 0001",
    );
    let r = panic::catch_unwind(|| mdns::asks_for(&q, &service));
    assert!(r.is_ok(), "asks_for panicked on {}", hex(&q));
}

/// lehu-audio, `decode_pcm_be` (decoder.rs:122): the frame count divides
/// by the format's channel count, 0 included. `Decoder::open` refuses 0
/// channels; this public function does not.
#[test]
fn decode_pcm_be_zero_channels() {
    let r = panic::catch_unwind(|| {
        let mut seg = Segment::with_capacity(64);
        decode_pcm_be(&[1, 2, 3, 4], Format::new(48000, 0), false, &mut seg)
    });
    assert!(
        r.is_ok(),
        "decode_pcm_be panicked on 01020304 with 0 channels"
    );
}

/// lehu-cp, `nalu::frame_to_annexb_in_place` (nalu.rs:18): `off + 4 + n`
/// with `n` from the frame's u32 length overflows a 32-bit usize. On the
/// board (ARMv5, release, no overflow checks) the body ff ff ff fc wraps
/// the sum to 0, the check passes, `off` stays at 0 and the screen thread
/// loops for ever; in a debug build it panics. A 64-bit host cannot reach
/// it, so this test exists only on 32-bit targets.
#[cfg(target_pointer_width = "32")]
#[test]
fn nalu_length_wraps_on_32_bits() {
    let r = panic::catch_unwind(|| {
        let mut f = vec![0xff, 0xff, 0xff, 0xfc];
        lehu_cp::nalu::frame_to_annexb_in_place(&mut f)
    });
    assert!(
        matches!(r, Ok(false)),
        "the frame ff ff ff fc was not refused"
    );
}

/// lehu-wifi, `rewrite` and `parse_into` (lib.rs, `setting` trims the
/// value): `check` accepts a name with a leading or trailing space and
/// `rewrite` writes it so, but `AccessPoint::read` trims it off. The phone
/// is then handed a name the access point does not broadcast.
#[test]
fn hostapd_name_with_spaces() {
    // Fixed by refusing the spaces up front; a name inside them still
    // goes through whole.
    assert!(lehu_wifi::check(" Car Net ", "lehu12345").is_err());
    assert!(lehu_wifi::check("Car Net", " lehu12345").is_err());
    let ssid = "Car Net";
    lehu_wifi::check(ssid, "lehu12345").expect("accepted");
    let text = lehu_wifi::rewrite("interface=wlan0\nssid=LEHU\n", ssid, "lehu12345");
    let mut p = lehu_wifi::Params::default();
    lehu_wifi::parse_into(&text, &mut p);
    assert_eq!(p.ssid, ssid, "written as {:?}", text);
}

/// lehu-core, `Registry::open` (devices.rs:98): the file's records are
/// taken whatever their number; `MAX_KNOWN` is kept only when `seen` adds
/// one, and then only one record goes.
#[test]
fn registry_file_over_max_known() {
    let dir = TempDir::new("bug-registry");
    let mut text = String::new();
    for i in 0..3 * MAX_KNOWN {
        text.push_str(&format!(
            "[[devices]]\nid = \"p{i}\"\nkind = \"car-play\"\nname = \"\"\nlast-seen = {i}\n"
        ));
    }
    std::fs::write(dir.path().join("devices.toml"), &text).expect("written");
    let r = Registry::open(dir.path());
    assert!(r.all().len() <= MAX_KNOWN, "{} records kept", r.all().len());
}
