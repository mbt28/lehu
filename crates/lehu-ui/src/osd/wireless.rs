//! The access point (screen 12) and the Link speed page (screen 13), both
//! under Wireless.
//!
//! The access point's radio is read from hostapd's config when its page
//! opens, never per tick. Band and Width are picked from their options,
//! Channel and Country stepped through the ones the other three allow
//! (`lehu_wifi::channels`); a change that leaves another field invalid
//! moves that one too (`settle`), so what is written always starts. Each
//! change rewrites the config and restarts the AP; the header says
//! "Restarting..." for the frame before the restart runs (it blocks the
//! loop for as long as the image's command takes), then how it went:
//! refused, saved for the next boot, or not allowed (LEHU unprivileged).
//!
//! The Link speed page samples the host once a second while it is up: the
//! AP's channel as the radio runs it, the phone's signal and rates, and the
//! interface's byte counters, whose difference is the traffic.

use lehu_wifi::{Applied, Band, COUNTRIES, Link, Radio, channel_number, channels, settle, widths};
use slint::SharedString;

use super::{Bridge, Host, LINK, Pick, State, clean, compose};

/// The Link speed page's samples are this far apart.
const LINK_EVERY_MS: u64 = 1000;

/// A change for the access point, waiting for one frame.
pub(super) enum ApWrite {
    Radio(Radio),
    Network(String, String),
}

fn band_label(band: Band) -> &'static str {
    match band {
        Band::G2 => "2.4 GHz",
        Band::G5 => "5 GHz",
    }
}

/// How a change went, for the header.
fn applied_label(applied: Applied) -> &'static str {
    match applied {
        Applied::Restarted => "Restarted",
        Applied::AtBoot => "Saved, applies at the next boot",
    }
}

/// kbit/s as Mbit/s: one decimal under 10.
fn mbit(out: &mut String, kbps: u64) {
    use std::fmt::Write as _;
    if kbps >= 10_000 {
        let _ = write!(out, "{}", kbps / 1000);
    } else {
        let _ = write!(out, "{}.{}", kbps / 1000, kbps % 1000 / 100);
    }
}

/// A signal in dBm, in words a glance can take.
fn signal_word(dbm: i32) -> &'static str {
    match dbm {
        -55.. => "strong",
        -67..=-56 => "good",
        -75..=-68 => "fair",
        _ => "weak",
    }
}

impl<H: Host> State<H> {
    /// The page opened: the config read again, the last status dropped.
    pub(super) fn open_access_point(&mut self) {
        self.ap_status.clear();
        self.read_access_point();
    }

    fn read_access_point(&mut self) {
        match self.host.access_point() {
            Ok(radio) => self.ap = Some(radio),
            Err(e) => {
                self.ap = None;
                self.ap_status = e.short();
            }
        }
    }

    pub(super) fn ap_widths(&self) -> &'static [u32] {
        widths(self.ap.as_ref().map_or(Band::G2, |r| r.band))
    }

    /// Where a row of the radio starts, its list made for Channel and
    /// Country; `None` when there is no config to change, or no channel
    /// the band and country allow.
    pub(super) fn ap_start(&mut self, row: Pick) -> Option<i32> {
        let Some(r) = self.ap.clone() else {
            // Said in the header already; read again in case it is back.
            self.read_access_point();
            return None;
        };
        self.pick_list.clear();
        let index =
            |list: &[String], want: &str| list.iter().position(|v| v == want).unwrap_or(0) as i32;
        match row {
            Pick::Band => Some(i32::from(r.band == Band::G5)),
            Pick::Width => Some(
                widths(r.band)
                    .iter()
                    .position(|&w| w == r.width)
                    .unwrap_or(0) as i32,
            ),
            Pick::Channel => {
                let list = channels(r.band, r.width, &r.country);
                if list.is_empty() {
                    self.ap_status = lehu_wifi::Error::NoCountry.short();
                    return None;
                }
                self.pick_list.extend(list.iter().map(u32::to_string));
                Some(index(&self.pick_list, &r.channel.to_string()))
            }
            Pick::Country => {
                self.pick_list
                    .extend(COUNTRIES.iter().map(|c| (*c).to_string()));
                if !r.country.is_empty() && !self.pick_list.contains(&r.country) {
                    let at = self
                        .pick_list
                        .iter()
                        .position(|c| *c > r.country)
                        .unwrap_or(self.pick_list.len());
                    self.pick_list.insert(at, r.country.clone());
                }
                Some(index(&self.pick_list, &r.country))
            }
            _ => None,
        }
    }

    /// Enter on a row of the radio: the new radio, the others settled
    /// around the change, written on the next frame.
    pub(super) fn ap_take(&mut self, row: Pick, taken: i32) {
        let Some(old) = self.ap.clone() else {
            return;
        };
        let pick = |list: &[String]| {
            usize::try_from(taken)
                .ok()
                .and_then(|i| list.get(i).cloned())
        };
        let mut r = old.clone();
        match row {
            Pick::Band => r.band = if taken == 1 { Band::G5 } else { Band::G2 },
            Pick::Width => match usize::try_from(taken)
                .ok()
                .and_then(|i| widths(r.band).get(i))
            {
                Some(&w) => r.width = w,
                None => return,
            },
            Pick::Channel => match pick(&self.pick_list).and_then(|c| c.parse().ok()) {
                Some(c) => r.channel = c,
                None => return,
            },
            Pick::Country => match pick(&self.pick_list) {
                Some(c) => r.country = c,
                None => return,
            },
            _ => return,
        }
        // 5 GHz without a country cannot start: said, nothing written.
        if r.band == Band::G5 && r.country.is_empty() {
            self.ap_status = lehu_wifi::Error::NoCountry.short();
            return;
        }
        settle(&mut r);
        if r != old {
            self.ap_request(ApWrite::Radio(r));
        }
    }

    /// A change for the access point: written once "Restarting..." is on
    /// screen.
    pub(super) fn ap_request(&mut self, write: ApWrite) {
        self.ap_status.clear();
        self.ap_status.push_str("Restarting...");
        self.ap_pending = Some(write);
        self.ap_armed = false;
    }

    /// At the start of a pump: a change whose "Restarting..." frame went out
    /// the tick before is carried out now.
    pub(super) fn ap_write_pending(&mut self) {
        if !self.ap_armed {
            return;
        }
        let Some(write) = self.ap_pending.take() else {
            return;
        };
        self.ap_armed = false;
        let result = match &write {
            ApWrite::Radio(r) => self.host.configure_radio(r),
            ApWrite::Network(ssid, pass) => self.host.configure_access_point(ssid, pass),
        };
        self.ap_status = match result {
            Ok(applied) => applied_label(applied).to_string(),
            Err(e) => {
                log::warn!("access point: {e}");
                e.short()
            }
        };
        let status = std::mem::take(&mut self.ap_status);
        self.read_access_point();
        // A read that failed says so; else how the write went.
        if self.ap.is_some() {
            self.ap_status = status;
        }
    }

    /// The page's labels and header, on change; the pending change armed
    /// once its frame is drawn.
    pub(super) fn pump_access_point(&mut self, bridge: &Bridge<'_>) {
        if self.ap_pending.is_some() {
            self.ap_armed = true;
        }
        if self.p_ap_status.update(&self.ap_status) {
            bridge.set_ap_status(clean(&mut self.scratch, self.p_ap_status.as_str()));
        }
        if self.ap == self.p_ap {
            return;
        }
        self.p_ap = self.ap.clone();
        let Some(r) = &self.ap else {
            bridge.set_ap_band_choice(-1);
            bridge.set_ap_width_choice(-1);
            bridge.set_ap_channel_label(SharedString::default());
            bridge.set_ap_country_label(SharedString::default());
            return;
        };
        bridge.set_ap_five(r.band == Band::G5);
        bridge.set_ap_band_choice(i32::from(r.band == Band::G5));
        let width = widths(r.band).iter().position(|&w| w == r.width);
        bridge.set_ap_width_choice(width.map_or(-1, |i| i as i32));
        let channel = compose(&mut self.scratch, format_args!("{}", r.channel));
        bridge.set_ap_channel_label(SharedString::from(channel));
        let country = if r.country.is_empty() {
            "Not set"
        } else {
            r.country.as_str()
        };
        bridge.set_ap_country_label(clean(&mut self.scratch, country));
    }

    /// The Link speed page opened: sampled now, the traffic measured from
    /// the next sample on.
    pub(super) fn open_link(&mut self) {
        self.link_due = 0;
        self.link_prev = None;
        // What the config says, for a driver that does not report its
        // channel.
        self.ap = self.host.access_point().ok();
    }

    /// Once a second while the page is up.
    pub(super) fn pump_link(&mut self, bridge: &Bridge<'_>, screen: i32, now: u64) {
        if screen != LINK || self.over_phone || now < self.link_due {
            return;
        }
        self.link_due = now + LINK_EVERY_MS;
        let link = self.host.link();
        let mut text = String::new();
        self.link_radio(&link, &mut text);
        if self.p_link[0].update(&text) {
            bridge.set_link_radio(SharedString::from(text.as_str()));
        }
        text.clear();
        // The phone: the session's when it runs over Wi-Fi; a station the
        // driver lists otherwise (this board's lists none).
        if self.host.link_wifi() {
            text.push_str(self.host.status());
        } else if link.station.is_some() {
            text.push_str("Connected, no session");
        } else {
            text.push_str("None");
        }
        if self.p_link[1].update(&text) {
            bridge.set_link_phone(SharedString::from(text.as_str()));
        }
        text.clear();
        if let Some(s) = link.station.as_ref().filter(|s| s.signal_dbm != 0) {
            let _ = std::fmt::Write::write_fmt(
                &mut text,
                format_args!("{} dBm, {}", s.signal_dbm, signal_word(s.signal_dbm)),
            );
        }
        if self.p_link[2].update(&text) {
            bridge.set_link_signal(SharedString::from(text.as_str()));
        }
        text.clear();
        // The phone's video comes this way: the rate it sends at is the one
        // that counts.
        if let Some(s) = link.station.as_ref().filter(|s| s.rx_kbps > 0) {
            mbit(&mut text, u64::from(s.rx_kbps));
            text.push_str(" Mbit/s");
        }
        if self.p_link[3].update(&text) {
            bridge.set_link_rate(SharedString::from(text.as_str()));
        }
        let mut sending = String::new();
        text.clear();
        let sample = (link.tx_bytes, link.rx_bytes, now);
        match self.link_prev {
            // Counters that went back: the interface came up again.
            Some((tx, rx, at)) if now > at && link.tx_bytes >= tx && link.rx_bytes >= rx => {
                let ms = now - at;
                mbit(&mut text, (link.rx_bytes - rx) * 8 / ms);
                text.push_str(" Mbit/s");
                mbit(&mut sending, (link.tx_bytes - tx) * 8 / ms);
                sending.push_str(" Mbit/s");
            }
            _ => {
                text.push_str("Measuring...");
                sending.push_str("Measuring...");
            }
        }
        self.link_prev = Some(sample);
        if self.p_link[4].update(&text) {
            bridge.set_link_receiving(SharedString::from(text.as_str()));
        }
        if self.p_link[5].update(&sending) {
            bridge.set_link_sending(SharedString::from(sending.as_str()));
        }
    }

    /// "5 GHz, channel 36, 80 MHz": from the radio when it says, else from
    /// the config; "Not running" without carrier.
    fn link_radio(&self, link: &Link, out: &mut String) {
        use std::fmt::Write as _;
        if !link.up {
            out.push_str("Not running");
            return;
        }
        let (band, channel, width) = match (link.channel, &self.ap) {
            (Some(c), ap) => {
                let band = if c.mhz >= 5000 { Band::G5 } else { Band::G2 };
                let width = if c.width_mhz > 0 {
                    c.width_mhz
                } else {
                    ap.as_ref().map_or(0, |r| r.width)
                };
                (band, channel_number(c.mhz), width)
            }
            (None, Some(r)) => (r.band, r.channel, r.width),
            (None, None) => {
                out.push_str("Running");
                return;
            }
        };
        let _ = write!(out, "{}, channel {channel}", band_label(band));
        if width > 0 {
            let _ = write!(out, ", {width} MHz");
        }
    }

    /// When the Link speed page wants its next sample.
    pub(super) fn link_due(&self) -> Option<u64> {
        (self.p_screen == LINK && !self.over_phone).then_some(self.link_due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_signals_read_at_a_glance() {
        let mut s = String::new();
        mbit(&mut s, 433_300);
        s.push(' ');
        mbit(&mut s, 6_500);
        s.push(' ');
        mbit(&mut s, 0);
        assert_eq!(s, "433 6.5 0.0");
        assert_eq!(signal_word(-40), "strong");
        assert_eq!(signal_word(-55), "strong");
        assert_eq!(signal_word(-60), "good");
        assert_eq!(signal_word(-70), "fair");
        assert_eq!(signal_word(-80), "weak");
    }
}
