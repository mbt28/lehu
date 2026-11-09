// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What the system tells the head unit: USB device names from the kernel,
//! Bluetooth addresses and names from BlueZ, hostapd's config file.

use lehu_bt::device::{self, Known};
use lehu_bt::{Address, Prompt};
use lehu_usb::{Device, Id};
use lehu_wifi as wifi;

use crate::{Target, bounded, ensure};

#[test]
fn usb_device_names() {
    Target::new("usb_device_names")
        .seed("1-1")
        .seed("1-1.4.3")
        .seed("usb1")
        .dict(&["é", "€", "-", "."])
        .max_len(64)
        .run(|b| {
            let name = String::from_utf8_lossy(b);
            let d = Device::new(
                b.first().copied().unwrap_or(1),
                2,
                Id {
                    vendor: 0x05ac,
                    product: 0x12a8,
                },
                0,
                &name,
            );
            // Kept whole when it fits, cut (never torn) when it does not.
            ensure(name.starts_with(d.name()), || {
                "a device name that is not a prefix".into()
            })?;
            let _ = (d.role(), d.node(), d.to_string(), d.same_slot(&d));
            Ok(())
        });
}

#[test]
fn bt_addresses() {
    Target::new("bt_addresses")
        .seed("AA:BB:CC:DD:EE:FF")
        .seed("/org/bluez/hci0/dev_00_1A_7D_DA_71_13")
        .seed("aa_bb_cc_dd_ee_ff")
        .dict(&[":", "_", "/", "dev_", "+f", "-1", "é"])
        .max_len(64)
        .run(|b| {
            let s = String::from_utf8_lossy(b);
            if let Some(a) = Address::parse(&s) {
                // What parses prints as the canonical form, which parses
                // to the same address.
                ensure(Address::parse(&a.to_string()) == Some(a), || {
                    "an address does not round trip".into()
                })?;
                let _ = a.is_zero();
            }
            Ok(())
        });
}

#[test]
fn bt_names() {
    Target::new("bt_names")
        .seed("Alex's Keyboard")
        .seed("  K380 \t Multi-Device  ")
        .seed("Ü€𝄞 pad")
        .max_len(256)
        .run(|b| {
            let s = String::from_utf8_lossy(b);
            let p = device::printable(&s, device::NAME_CHARS);
            ensure(
                p.len() <= device::NAME_CHARS && p.bytes().all(|c| (0x20..=0x7e).contains(&c)),
                || format!("an unprintable or long name {p:?}"),
            )?;
            let mut k = Known {
                address: "AA:BB:CC:DD:EE:FF".into(),
                alias: s.to_string(),
                icon: if b.len() % 2 == 0 {
                    "input-keyboard".into()
                } else {
                    s.to_string()
                },
                class: b
                    .get(..4)
                    .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])),
                appearance: b.get(..2).map(|w| u16::from_le_bytes([w[0], w[1]])),
                ..Known::default()
            };
            let shown = k.display_name();
            ensure(
                shown.len() <= device::NAME_CHARS && shown.is_ascii(),
                || format!("a display name {shown:?}"),
            )?;
            let _ = (k.is_hid(), k.kind().text());
            k.alias.clear();
            let _ = k.display_name();
            let prompt = Prompt::code(&p, &device::six_digits(u32::from(b.len() as u16)), 3);
            bounded(
                "prompt text",
                prompt.title.len() + prompt.code.len(),
                p.len(),
                1,
                64,
            )
        });
}

const HOSTAPD: &str = "\
# hostapd for the head unit
interface=wlan0
driver=nl80211
ssid=LEHU
hw_mode=a
channel=36
wpa=2
wpa_passphrase=lehu1234
wpa_key_mgmt=WPA-PSK
rsn_pairwise=CCMP
";

#[test]
fn hostapd_config() {
    Target::new("hostapd_config")
        .seed(HOSTAPD)
        .seed("ssid=a\nssid=b\n#ssid=c\n  wpa_passphrase = x  \nchannel=99999999999\n")
        .dict(&[
            "ssid=",
            "wpa_passphrase=",
            "channel=",
            "interface=",
            "#",
            "\n",
            "\r\n",
            "=",
            " ",
            "\t",
        ])
        .max_len(4096)
        .run(|b| {
            let text = String::from_utf8_lossy(b);
            let mut p = wifi::Params::default();
            wifi::parse_into(&text, &mut p);
            bounded(
                "parsed values",
                p.ssid.len() + p.passphrase.len() + p.iface.len(),
                text.len(),
                1,
                0,
            )?;
            // A rewrite with what WPA2 accepts reads back as written, and
            // keeps every other line as it was.
            let (ssid, pass) = ("Car Net", "pass phrase 1");
            wifi::check(ssid, pass).map_err(|e| e.to_string())?;
            let out = wifi::rewrite(&text, ssid, pass);
            let mut q = wifi::Params::default();
            wifi::parse_into(&out, &mut q);
            ensure(q.ssid == ssid && q.passphrase == pass, || {
                "a rewrite reads back differently".into()
            })?;
            ensure(q.iface == p.iface && q.channel == p.channel, || {
                "a rewrite changed another directive".into()
            })?;
            let others = |t: &str| {
                t.lines()
                    .filter(|l| !matches!(wifi::setting(l), Some(("ssid" | "wpa_passphrase", _))))
                    // Lines are written back with a bare newline: a CR
                    // before it is not kept, which hostapd does not mind.
                    .map(|l| l.trim_end_matches('\r').to_string())
                    .collect::<Vec<_>>()
            };
            ensure(others(&out) == others(&text), || {
                "a rewrite lost or changed a line".into()
            })?;
            // The checks refuse a newline in either value.
            let s = String::from_utf8_lossy(&b[..b.len().min(40)]);
            if wifi::check(&s, pass).is_ok() {
                ensure(!s.contains(['\n', '\r']), || {
                    "a newline passed the check".into()
                })?;
            }
            Ok(())
        });
}

#[test]
fn hostapd_radio() {
    Target::new("hostapd_radio")
        .seed(HOSTAPD)
        .seed("hw_mode=g\nchannel=6\nht_capab=[HT40+][SHORT-GI-20]\nieee80211ac=1\nvht_oper_chwidth=1\n")
        .dict(&[
            "hw_mode=",
            "channel=",
            "ht_capab=",
            "[HT40+]",
            "[HT40-]",
            "ieee80211n=",
            "ieee80211ac=",
            "ieee80211ax=",
            "vht_oper_chwidth=",
            "he_oper_chwidth=",
            "vht_oper_centr_freq_seg0_idx=",
            "country_code=",
            "\n",
            "=",
            "1",
        ])
        .max_len(4096)
        .run(|b| {
            let text = String::from_utf8_lossy(b);
            let _ = wifi::parse_radio(&text);
            // Whatever the file held, a valid radio written into it reads
            // back as written.
            for r in [
                wifi::Radio {
                    band: wifi::Band::G5,
                    channel: 44,
                    width: 80,
                    country: "DE".into(),
                },
                wifi::Radio {
                    band: wifi::Band::G2,
                    channel: 11,
                    width: 40,
                    country: "US".into(),
                },
                wifi::Radio {
                    band: wifi::Band::G2,
                    channel: 6,
                    width: 20,
                    country: "GB".into(),
                },
            ] {
                wifi::check_radio(&r).map_err(|e| e.to_string())?;
                let out = wifi::rewrite_radio(&text, &r);
                let back = wifi::parse_radio(&out);
                ensure(back == r, || format!("{r:?} reads back as {back:?}"))?;
            }
            Ok(())
        });
}
