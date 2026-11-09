// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The access point's radio in hostapd's config: the band, the channel, the
//! width and the country, read from the file and written back into it.
//!
//! Only channels every country in the list allows without radar detection
//! are offered: a DFS channel makes hostapd listen for a minute before it
//! beacons, and a channel the regulatory domain refuses leaves the board
//! with no access point at all. Under no country the kernel's world domain
//! forbids starting an AP on 5 GHz, so 5 GHz needs one.

use crate::{Error, setting};

/// The band the AP runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Band {
    #[default]
    G2,
    G5,
}

/// The radio part of hostapd's config.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Radio {
    pub band: Band,
    pub channel: u32,
    /// 20, 40 or 80 MHz.
    pub width: u32,
    /// The ISO 3166 code (`DE`), empty when the config has none.
    pub country: String,
}

/// The countries offered, by their code. One not here that the config has
/// is kept, with the channels every country allows.
pub const COUNTRIES: &[&str] = &[
    "AT", "AU", "BE", "BG", "BR", "CA", "CH", "CN", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FR",
    "GB", "GR", "HK", "HR", "HU", "IE", "IN", "IS", "IT", "JP", "KR", "LI", "LT", "LU", "LV", "MT",
    "NL", "NO", "NZ", "PL", "PT", "RO", "SE", "SG", "SI", "SK", "TR", "TW", "US", "ZA",
];

/// The widths each band can be given, in MHz.
pub fn widths(band: Band) -> &'static [u32] {
    match band {
        Band::G2 => &[20, 40],
        Band::G5 => &[20, 40, 80],
    }
}

/// The last 2.4 GHz channel: 11 under the FCC's plan and wherever the
/// country is not known, 13 in the rest of the list.
fn last_g2(country: &str) -> u32 {
    if COUNTRIES.contains(&country) && !matches!(country, "US" | "CA" | "TW") {
        13
    } else {
        11
    }
}

/// The 5 GHz channels without radar detection: 36 to 48 everywhere, 149 to
/// 165 where the upper band is open to an outdoor device.
fn channels_g5(country: &str) -> &'static [u32] {
    match country {
        "" => &[],
        "US" | "CA" | "AU" | "NZ" | "CN" | "IN" | "SG" | "HK" | "TW" | "BR" => {
            &[36, 40, 44, 48, 149, 153, 157, 161, 165]
        }
        "KR" => &[36, 40, 44, 48, 149, 153, 157, 161],
        _ => &[36, 40, 44, 48],
    }
}

/// The 80 MHz block a 5 GHz channel is in: its centre channel.
fn block_80(channel: u32) -> Option<u32> {
    match channel {
        36..=48 => Some(42),
        149..=161 => Some(155),
        _ => None,
    }
}

/// Where a 40 MHz channel's second half is: +1 above, -1 below.
fn second_40(band: Band, channel: u32, country: &str) -> Option<i32> {
    match band {
        // Pairs 36/40, 44/48, 149/153, 157/161; 165 has no partner.
        Band::G5 => match channel {
            36 | 44 | 149 | 157 => Some(1),
            40 | 48 | 153 | 161 => Some(-1),
            _ => None,
        },
        // Four channels up when they are there, else four down.
        Band::G2 => {
            if channel + 4 <= last_g2(country) {
                Some(1)
            } else if channel > 4 {
                Some(-1)
            } else {
                None
            }
        }
    }
}

/// The channels the band, the width and the country allow, in order.
pub fn channels(band: Band, width: u32, country: &str) -> Vec<u32> {
    let all: Vec<u32> = match band {
        Band::G2 => (1..=last_g2(country)).collect(),
        Band::G5 => channels_g5(country).to_vec(),
    };
    all.into_iter()
        .filter(|&c| match width {
            20 => true,
            40 => second_40(band, c, country).is_some(),
            80 => band == Band::G5 && block_80(c).is_some(),
            _ => false,
        })
        .collect()
}

/// What hostapd would start with: a country it knows the form of, a width
/// the band has, and a channel the three allow.
pub fn check_radio(r: &Radio) -> Result<(), Error> {
    if !r.country.is_empty()
        && (r.country.len() != 2 || !r.country.bytes().all(|b| b.is_ascii_uppercase()))
    {
        return Err(Error::Country);
    }
    if r.band == Band::G5 && r.country.is_empty() {
        return Err(Error::NoCountry);
    }
    if !widths(r.band).contains(&r.width) {
        return Err(Error::Width(r.width));
    }
    if !channels(r.band, r.width, &r.country).contains(&r.channel) {
        return Err(Error::Channel(r.channel));
    }
    Ok(())
}

/// After one of its fields changed, the radio made valid again by the
/// others giving way: the width falls back to 20 MHz, the channel to 6 or
/// 36, else to the first one allowed.
pub fn settle(r: &mut Radio) {
    if !widths(r.band).contains(&r.width) {
        r.width = 20;
    }
    let allowed = channels(r.band, r.width, &r.country);
    if allowed.contains(&r.channel) {
        return;
    }
    let usual = match r.band {
        Band::G2 => 6,
        Band::G5 => 36,
    };
    if allowed.contains(&usual) {
        r.channel = usual;
    } else if let Some(&first) = allowed.first() {
        r.channel = first;
    } else if r.width != 20 {
        r.width = 20;
        settle(r);
    }
}

fn flag(value: &str) -> bool {
    value.trim() == "1"
}

/// The radio as the config text has it; the last of a repeated key counts.
pub fn parse_radio(text: &str) -> Radio {
    let (mut mode, mut channel, mut country) = ("", 0, "");
    let (mut n, mut ht40) = (true, false);
    let (mut ac, mut vht80, mut ax, mut he80) = (false, false, false, false);
    for line in text.lines() {
        match setting(line) {
            Some(("hw_mode", v)) => mode = v,
            Some(("channel", v)) => channel = v.parse().unwrap_or(0),
            Some(("country_code", v)) => country = v,
            Some(("ieee80211n", v)) => n = flag(v),
            Some(("ht_capab", v)) => ht40 = v.contains("[HT40+]") || v.contains("[HT40-]"),
            Some(("ieee80211ac", v)) => ac = flag(v),
            Some(("vht_oper_chwidth", v)) => vht80 = flag(v),
            Some(("ieee80211ax", v)) => ax = flag(v),
            Some(("he_oper_chwidth", v)) => he80 = flag(v),
            _ => {}
        }
    }
    let band = if mode == "a" || (mode.is_empty() && channel >= 36) {
        Band::G5
    } else {
        Band::G2
    };
    let width = if band == Band::G5 && ((ac && vht80) || (ax && he80)) {
        80
    } else if n && ht40 {
        40
    } else {
        20
    };
    Radio {
        band,
        channel,
        width,
        country: country.to_string(),
    }
}

/// `ht_capab` with its HT40 flag for this radio, every other flag kept.
fn ht_capab(old: &str, r: &Radio) -> String {
    // Until none is left: taking one out can join the halves of another.
    let mut out = old.to_string();
    while out.contains("[HT40+]") || out.contains("[HT40-]") {
        out = out.replace("[HT40+]", "").replace("[HT40-]", "");
    }
    if r.width >= 40 {
        let up = second_40(r.band, r.channel, &r.country).unwrap_or(1) > 0;
        out.insert_str(0, if up { "[HT40+]" } else { "[HT40-]" });
    }
    out
}

/// The config text with the radio's directives set, every other line kept
/// as it is. A directive is replaced where it stands, appended when
/// missing, or dropped when the radio has no use for it. The 802.11ac and
/// 802.11ax switches are left as the image set them, except that 80 MHz
/// turns 802.11ac on, and 2.4 GHz turns it off where the file has it.
pub fn rewrite_radio(text: &str, r: &Radio) -> String {
    let mut old_ht = None;
    let has = |key: &str| {
        text.lines()
            .any(|l| setting(l).is_some_and(|(k, _)| k == key))
    };
    let (has_ac, has_vht_width, has_he_width) = (
        has("ieee80211ac"),
        has("vht_oper_chwidth"),
        has("he_oper_chwidth"),
    );
    for line in text.lines() {
        if let Some(("ht_capab", v)) = setting(line) {
            old_ht = Some(v);
        }
    }
    let wide = r.band == Band::G5 && r.width == 80;
    let centre = block_80(r.channel).unwrap_or(0).to_string();
    let ht = ht_capab(old_ht.unwrap_or(""), r);
    let mut edits: Vec<(&str, Option<String>)> = vec![
        (
            "hw_mode",
            Some(if r.band == Band::G5 { "a" } else { "g" }.into()),
        ),
        ("channel", Some(r.channel.to_string())),
        ("ht_capab", (!ht.is_empty()).then_some(ht)),
    ];
    if !r.country.is_empty() {
        edits.push(("country_code", Some(r.country.clone())));
        edits.push(("ieee80211d", Some("1".into())));
    }
    if r.width >= 40 {
        edits.push(("ieee80211n", Some("1".into())));
    }
    if wide {
        edits.push(("ieee80211ac", Some("1".into())));
        edits.push(("vht_oper_chwidth", Some("1".into())));
        edits.push(("vht_oper_centr_freq_seg0_idx", Some(centre.clone())));
        if has_he_width {
            edits.push(("he_oper_chwidth", Some("1".into())));
            edits.push(("he_oper_centr_freq_seg0_idx", Some(centre)));
        }
    } else {
        if r.band == Band::G2 && has_ac {
            edits.push(("ieee80211ac", Some("0".into())));
        }
        if has_vht_width {
            edits.push(("vht_oper_chwidth", Some("0".into())));
        }
        edits.push(("vht_oper_centr_freq_seg0_idx", None));
        if has_he_width {
            edits.push(("he_oper_chwidth", Some("0".into())));
        }
        edits.push(("he_oper_centr_freq_seg0_idx", None));
    }
    set_directives(text, &edits)
}

/// Each `key=value` of `edits` written over every line of that key (`None`
/// drops them), and appended once at the end when the text had none.
pub fn set_directives(text: &str, edits: &[(&str, Option<String>)]) -> String {
    let mut out = String::with_capacity(text.len() + 128);
    let mut written = vec![false; edits.len()];
    for line in text.lines() {
        let edit = setting(line).and_then(|(k, _)| edits.iter().position(|(key, _)| *key == k));
        match edit {
            Some(i) => {
                if let (false, Some(value)) = (written[i], &edits[i].1) {
                    out.push_str(edits[i].0);
                    out.push('=');
                    out.push_str(value);
                    out.push('\n');
                }
                // A repeated key is written once, where it first stood.
                written[i] = true;
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (i, (key, value)) in edits.iter().enumerate() {
        if let (false, Some(value)) = (written[i], value) {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board's file, in the shape the image ships it.
    const BOARD: &str = "interface=wlan0\n\
driver=nl80211\n\
ssid=f1c200s-ap\n\
hw_mode=g\n\
channel=6\n\
ieee80211n=1\n\
ht_capab=[SHORT-GI-20]\n\
wmm_enabled=1\n\
dtim_period=1\n\
wpa=2\n\
wpa_passphrase=secret123\n\
wpa_key_mgmt=WPA-PSK\n\
rsn_pairwise=CCMP\n";

    fn radio(band: Band, channel: u32, width: u32, country: &str) -> Radio {
        Radio {
            band,
            channel,
            width,
            country: country.into(),
        }
    }

    #[test]
    fn the_board_file_reads_as_2g_channel_6() {
        assert_eq!(parse_radio(BOARD), radio(Band::G2, 6, 20, ""));
        // A 5 GHz file with 80 MHz through 802.11ac.
        let wide = "hw_mode=a\nchannel=36\nieee80211n=1\nht_capab=[HT40+]\n\
                    ieee80211ac=1\nvht_oper_chwidth=1\nvht_oper_centr_freq_seg0_idx=42\n\
                    country_code=DE\n";
        assert_eq!(parse_radio(wide), radio(Band::G5, 36, 80, "DE"));
        // HT40 with 802.11n off is 20 MHz; a channel alone says the band.
        assert_eq!(
            parse_radio("channel=44\nieee80211n=0\nht_capab=[HT40+]\n"),
            radio(Band::G5, 44, 20, "")
        );
        assert_eq!(parse_radio(""), radio(Band::G2, 0, 20, ""));
    }

    #[test]
    fn channels_follow_band_width_and_country() {
        assert_eq!(channels(Band::G2, 20, "DE"), (1..=13).collect::<Vec<_>>());
        assert_eq!(channels(Band::G2, 20, "US"), (1..=11).collect::<Vec<_>>());
        // Unknown or none: what every country allows.
        assert_eq!(channels(Band::G2, 20, "").len(), 11);
        assert_eq!(channels(Band::G2, 20, "XX").len(), 11);
        assert_eq!(channels(Band::G2, 40, "DE"), (1..=13).collect::<Vec<_>>());
        assert_eq!(channels(Band::G2, 80, "DE"), Vec::<u32>::new());
        assert_eq!(channels(Band::G5, 20, "DE"), vec![36, 40, 44, 48]);
        assert_eq!(channels(Band::G5, 20, ""), Vec::<u32>::new());
        assert_eq!(
            channels(Band::G5, 40, "US"),
            vec![36, 40, 44, 48, 149, 153, 157, 161]
        );
        assert_eq!(
            channels(Band::G5, 80, "US"),
            vec![36, 40, 44, 48, 149, 153, 157, 161]
        );
        assert!(channels(Band::G5, 20, "US").contains(&165));
        assert!(!channels(Band::G5, 20, "KR").contains(&165));
    }

    #[test]
    fn invalid_radios_are_refused() {
        assert!(check_radio(&radio(Band::G2, 6, 20, "")).is_ok());
        assert!(check_radio(&radio(Band::G5, 36, 80, "DE")).is_ok());
        assert!(matches!(
            check_radio(&radio(Band::G5, 36, 20, "")),
            Err(Error::NoCountry)
        ));
        assert!(matches!(
            check_radio(&radio(Band::G2, 13, 20, "US")),
            Err(Error::Channel(13))
        ));
        assert!(matches!(
            check_radio(&radio(Band::G5, 149, 20, "DE")),
            Err(Error::Channel(149))
        ));
        assert!(matches!(
            check_radio(&radio(Band::G5, 165, 40, "US")),
            Err(Error::Channel(165))
        ));
        assert!(matches!(
            check_radio(&radio(Band::G2, 6, 80, "DE")),
            Err(Error::Width(80))
        ));
        assert!(matches!(
            check_radio(&radio(Band::G2, 6, 20, "de")),
            Err(Error::Country)
        ));
        assert!(matches!(
            check_radio(&radio(Band::G2, 6, 20, "D\n")),
            Err(Error::Country)
        ));
    }

    #[test]
    fn a_change_settles_the_other_fields() {
        let mut r = radio(Band::G2, 13, 40, "DE");
        r.band = Band::G5;
        settle(&mut r);
        assert_eq!(r, radio(Band::G5, 36, 40, "DE"));
        let mut r = radio(Band::G5, 157, 80, "US");
        r.country = "DE".into();
        settle(&mut r);
        assert_eq!(r, radio(Band::G5, 36, 80, "DE"));
        let mut r = radio(Band::G5, 44, 80, "DE");
        r.band = Band::G2;
        settle(&mut r);
        assert_eq!(r, radio(Band::G2, 6, 20, "DE"));
        let mut r = radio(Band::G2, 13, 20, "DE");
        r.country = "US".into();
        settle(&mut r);
        assert_eq!(r.channel, 6);
    }

    #[test]
    fn a_rewrite_reads_back_and_keeps_the_rest() {
        for r in [
            radio(Band::G5, 36, 80, "DE"),
            radio(Band::G5, 48, 40, "DE"),
            radio(Band::G5, 165, 20, "US"),
            radio(Band::G2, 11, 40, "US"),
            radio(Band::G2, 1, 40, "DE"),
            radio(Band::G2, 6, 20, "GB"),
        ] {
            check_radio(&r).unwrap();
            let out = rewrite_radio(BOARD, &r);
            assert_eq!(parse_radio(&out), r, "{out}");
            for keep in [
                "ssid=f1c200s-ap",
                "wpa_passphrase=secret123",
                "dtim_period=1",
                "driver=nl80211",
            ] {
                assert!(out.lines().any(|l| l == keep), "{keep} lost: {out}");
            }
            // Written once each, other ht_capab flags kept.
            assert_eq!(out.matches("channel=").count(), 1);
            assert!(out.contains("[SHORT-GI-20]"));
            // Twice is the same as once.
            assert_eq!(rewrite_radio(&out, &r), out);
        }
        let out = rewrite_radio(BOARD, &radio(Band::G5, 40, 80, "DE"));
        assert!(out.contains("ht_capab=[HT40-][SHORT-GI-20]\n"));
        assert!(out.contains("vht_oper_centr_freq_seg0_idx=42\n"));
        assert!(out.contains("country_code=DE\nieee80211d=1\n"));
        assert!(out.contains("hw_mode=a\n"));
        // Back to 2.4 GHz: 802.11ac off, the VHT centre gone.
        let back = rewrite_radio(&out, &radio(Band::G2, 6, 20, "DE"));
        assert!(back.contains("ieee80211ac=0\n"));
        assert!(back.contains("vht_oper_chwidth=0\n"));
        assert!(!back.contains("seg0"));
        assert!(back.contains("ht_capab=[SHORT-GI-20]\n"));
        // A flag hidden inside another is taken out too.
        let nested = rewrite_radio("ht_capab=[HT[HT40-]40+]\n", &radio(Band::G2, 6, 20, ""));
        assert_eq!(parse_radio(&nested).width, 20, "{nested}");
        // An ht_capab left with nothing goes.
        let bare = rewrite_radio("ht_capab=[HT40+]\n", &radio(Band::G2, 6, 20, ""));
        assert_eq!(bare, "hw_mode=g\nchannel=6\n");
        // 802.11ax's width follows when the file has it.
        let he = rewrite_radio(
            "ieee80211ax=1\nhe_oper_chwidth=0\n",
            &radio(Band::G5, 36, 80, "DE"),
        );
        assert!(he.contains("he_oper_chwidth=1\n"));
        assert!(he.contains("he_oper_centr_freq_seg0_idx=42\n"));
    }

    #[test]
    fn repeated_keys_are_written_once_where_the_first_stood() {
        let out = set_directives(
            "a=1\nchannel=1\nb=2\nchannel=3\n# channel=9\n",
            &[("channel", Some("11".into())), ("gone", None)],
        );
        assert_eq!(out, "a=1\nchannel=11\nb=2\n# channel=9\n");
        let out = set_directives("a=1\nx=2\n", &[("x", None), ("y", Some("3".into()))]);
        assert_eq!(out, "a=1\ny=3\n");
    }
}
