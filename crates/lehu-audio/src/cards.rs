//! The playback devices there are, from the kernel's own lists in
//! `/proc/asound` (`cards` for each card's id and name, `pcm` for its
//! devices): no ALSA call, no device opened, so a PCM the mixer holds is
//! listed too. Each is named as `aplay -L` names it, by the card's id
//! rather than its number, which a USB card plugged in later can change.

use std::fs;

/// One ALSA playback device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The PCM to open: `hw:CARD=<id>,DEV=<n>`.
    pub device: String,
    /// What the card calls itself, with the device's name when the card
    /// has more than one.
    pub label: String,
    pub card: u32,
    pub card_id: String,
    pub dev: u32,
}

/// The playback devices the kernel lists now, in card order.
pub fn outputs() -> Vec<Output> {
    let cards = fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let pcm = fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
    parse(&cards, &pcm)
}

/// `cards`: " 0 [Codec          ]: sun4i-codec - sun4i-codec", then the
/// long name on a line of its own. Each card's (number, id, name).
fn parse_cards(text: &str) -> Vec<(u32, String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((number, rest)) = line.split_once(' ') else {
            continue;
        };
        let Ok(number) = number.parse::<u32>() else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((id, rest)) = rest.split_once(']') else {
            continue;
        };
        let name = rest
            .trim_start_matches(':')
            .split_once(" - ")
            .map_or(rest, |(_, name)| name)
            .trim()
            .to_string();
        out.push((number, id.trim().to_string(), name));
    }
    out
}

/// `pcm`: "00-00: CDC PCM Codec-0 :  : playback 1 : capture 1". Each
/// playback device's (card, device, name).
fn parse_pcm(text: &str) -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((at, rest)) = line.split_once(": ") else {
            continue;
        };
        let Some((card, dev)) = at.split_once('-') else {
            continue;
        };
        let (Ok(card), Ok(dev)) = (card.trim().parse(), dev.trim().parse()) else {
            continue;
        };
        let fields: Vec<&str> = rest.split(" : ").map(str::trim).collect();
        if !fields.iter().any(|f| f.starts_with("playback")) {
            continue;
        }
        let name = fields.first().copied().unwrap_or("").trim_end_matches(" :");
        out.push((card, dev, name.trim().to_string()));
    }
    out
}

pub fn parse(cards: &str, pcm: &str) -> Vec<Output> {
    let cards = parse_cards(cards);
    let pcm = parse_pcm(pcm);
    let mut out = Vec::new();
    for (card, dev, pcm_name) in &pcm {
        let Some((_, id, name)) = cards.iter().find(|(n, _, _)| n == card) else {
            continue;
        };
        let several = pcm.iter().filter(|(c, _, _)| c == card).count() > 1;
        let label = if several && !pcm_name.is_empty() {
            format!("{name}: {pcm_name}")
        } else if name.is_empty() {
            id.clone()
        } else {
            name.clone()
        };
        out.push(Output {
            device: format!("hw:CARD={id},DEV={dev}"),
            label,
            card: *card,
            card_id: id.clone(),
            dev: *dev,
        });
    }
    out
}

/// The device a PCM name opens, as (card -- a number or an id -- and
/// device), for the plain forms: `hw:0,0`, `plughw:Codec`, `hw:CARD=x,DEV=1`.
/// `None` for anything else (`default`, a plugin of the image's).
fn target(pcm: &str) -> Option<(String, u32)> {
    let pcm: String = pcm.chars().filter(|&c| c != '"').collect();
    let (kind, args) = pcm.split_once(':')?;
    if !matches!(kind, "hw" | "plughw") {
        return None;
    }
    let (mut card, mut dev) = (None, 0);
    for (i, part) in args.split(',').enumerate() {
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (if i == 0 { "CARD" } else { "DEV" }, part.trim()),
        };
        match key {
            "CARD" => card = Some(value.to_string()),
            "DEV" => dev = value.parse().ok()?,
            _ => {}
        }
    }
    card.filter(|c| !c.is_empty()).map(|c| (c, dev))
}

/// The setting `pcm` opens this output.
pub fn opens(pcm: &str, out: &Output) -> bool {
    target(pcm).is_some_and(|(card, dev)| {
        dev == out.dev && (card == out.card_id || card.parse() == Ok(out.card))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [Codec          ]: sun4i-codec - sun4i-codec
                      sun4i-codec
 1 [Device         ]: USB-Audio - USB Audio Device
                      Generic USB Audio Device at usb-1c13000.usb-1, full speed
 2 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0
                      vc4-hdmi-0
";
    const PCM: &str = "00-00: CDC PCM Codec-0 :  : playback 1 : capture 1
01-00: USB Audio : USB Audio : playback 1 : capture 1
01-01: USB Audio #1 : USB Audio #1 : playback 1
02-00: MAI PCM i2s-hifi-0 : MAI PCM i2s-hifi-0 : playback 1
03-00: Orphan : : capture 1
";

    #[test]
    fn the_playback_devices_are_listed_by_card_id() {
        let all = parse(CARDS, PCM);
        let devices: Vec<_> = all.iter().map(|o| o.device.as_str()).collect();
        assert_eq!(
            devices,
            [
                "hw:CARD=Codec,DEV=0",
                "hw:CARD=Device,DEV=0",
                "hw:CARD=Device,DEV=1",
                "hw:CARD=vc4hdmi0,DEV=0",
            ]
        );
        let labels: Vec<_> = all.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "sun4i-codec",
                "USB Audio Device: USB Audio",
                "USB Audio Device: USB Audio #1",
                "vc4-hdmi-0",
            ]
        );
        assert!(parse("", PCM).is_empty());
        assert!(parse(CARDS, "").is_empty());
        assert!(parse("garbage [x\n 9 [", "zz-00: x\n0-0 : y\n").is_empty());
    }

    #[test]
    fn a_setting_is_matched_by_number_or_id() {
        let all = parse(CARDS, PCM);
        let codec = &all[0];
        assert!(opens("hw:0,0", codec));
        assert!(opens("hw:0", codec));
        assert!(opens("plughw:Codec,0", codec));
        assert!(opens("hw:CARD=Codec,DEV=0", codec));
        assert!(opens("hw:\"Codec\",0", codec));
        assert!(!opens("hw:0,1", codec));
        assert!(!opens("hw:1,0", codec));
        assert!(!opens("default", codec));
        assert!(!opens("dmix:0", codec));
        assert!(opens("hw:CARD=Device,DEV=1", &all[2]));
        assert!(opens("hw:1,1", &all[2]));
        assert!(!opens("hw:CARD=,DEV=0", codec));
    }
}
