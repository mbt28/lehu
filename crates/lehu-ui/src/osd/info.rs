// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Settings > System > Info (screen 18): what the system is, and once a
//! second while it shows, what it does. Nothing is read off this page.

use lehu_core::sysinfo::Load;
use slint::SharedString;

use super::wireless::mbit;
use super::{Bridge, Host, INFO, State};

const INFO_EVERY_MS: u64 = 1000;

/// The counters a rate is measured against.
pub(super) struct Sample {
    at: u64,
    load: Load,
    tx: u64,
    rx: u64,
    video: u32,
}

/// `"12%, LEHU 3%"` between two samples; `None` when no time has passed.
pub(super) fn cpu_text(before: &Load, now: &Load) -> Option<String> {
    let total = now.cpu_total.checked_sub(before.cpu_total)?;
    if total == 0 {
        return None;
    }
    let idle = now.cpu_idle.saturating_sub(before.cpu_idle).min(total);
    let own = now.own_ticks.saturating_sub(before.own_ticks).min(total);
    Some(format!(
        "{}%, LEHU {}%",
        (total - idle) * 100 / total,
        own * 100 / total
    ))
}

/// `"23 of 54 MB, LEHU 10 MB"`.
pub(super) fn memory_text(load: &Load) -> String {
    let used = load.mem_total_kb.saturating_sub(load.mem_available_kb);
    format!(
        "{} of {} MB, LEHU {} MB",
        used / 1024,
        load.mem_total_kb / 1024,
        load.own_rss_kb / 1024
    )
}

impl<H: Host> State<H> {
    pub(super) fn open_info(&mut self, bridge: &Bridge<'_>) {
        self.info_due = 0;
        self.info_prev = None;
        let names = self.host.system_names();
        bridge.set_info_kernel(SharedString::from(names.kernel.as_str()));
        bridge.set_info_lehu(SharedString::from(names.lehu.as_str()));
    }

    /// Once a second while the page is up.
    pub(super) fn pump_info(&mut self, bridge: &Bridge<'_>, screen: i32, now: u64) {
        if screen != INFO || self.over_phone || now < self.info_due {
            return;
        }
        self.info_due = now + INFO_EVERY_MS;

        // LEHU OS names its version; another image, itself.
        let version = self.host.update().version.clone();
        let (label, os) = if version.is_empty() {
            ("System", self.host.system_names().os)
        } else {
            ("LEHU OS", version)
        };
        if self.p_info[0].update(label) {
            bridge.set_info_os_label(SharedString::from(label));
        }
        if self.p_info[1].update(&os) {
            bridge.set_info_os(SharedString::from(os.as_str()));
        }

        let load = self.host.system_load();
        let text = memory_text(&load);
        if self.p_info[2].update(&text) {
            bridge.set_info_memory(SharedString::from(text.as_str()));
        }
        let text = self
            .info_prev
            .as_ref()
            .and_then(|p| cpu_text(&p.load, &load))
            .unwrap_or_else(|| "Measuring...".to_string());
        if self.p_info[3].update(&text) {
            bridge.set_info_cpu(SharedString::from(text.as_str()));
        }

        let (usb, wifi) = (self.host.link_usb(), self.host.link_wifi());
        // A cable wins: over it the video is what can be counted; over
        // Wi-Fi the interface counts everything, both ways.
        let (tx, rx) = if wifi && !usb {
            let l = self.host.link();
            (l.tx_bytes, l.rx_bytes)
        } else {
            (0, 0)
        };
        let video = self.host.video_bytes();
        let mut text = String::from(if usb {
            "Cable, "
        } else if wifi {
            "Wi-Fi, "
        } else {
            "No phone"
        });
        if usb || wifi {
            match self.info_prev.as_ref().filter(|p| now > p.at) {
                Some(p) if usb && video >= p.video => {
                    mbit(&mut text, u64::from(video - p.video) * 8 / (now - p.at));
                    text.push_str(" Mbit/s video");
                }
                Some(p) if !usb && tx >= p.tx && rx >= p.rx => {
                    let ms = now - p.at;
                    mbit(&mut text, (rx - p.rx) * 8 / ms);
                    text.push_str(" Mbit/s in, ");
                    mbit(&mut text, (tx - p.tx) * 8 / ms);
                    text.push_str(" out");
                }
                _ => text.push_str("measuring..."),
            }
        }
        if self.p_info[4].update(&text) {
            bridge.set_info_link(SharedString::from(text.as_str()));
        }
        self.info_prev = Some(Sample {
            at: now,
            load,
            tx,
            rx,
            video,
        });
    }

    /// When the Info page wants its next sample.
    pub(super) fn info_due(&self) -> Option<u64> {
        (self.p_screen == INFO && !self.over_phone).then_some(self.info_due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(total: u64, idle: u64, own: u64) -> Load {
        Load {
            cpu_total: total,
            cpu_idle: idle,
            own_ticks: own,
            ..Load::default()
        }
    }

    #[test]
    fn cpu_is_the_share_between_two_samples() {
        let a = load(1000, 800, 10);
        assert_eq!(
            cpu_text(&a, &load(1100, 860, 13)).as_deref(),
            Some("40%, LEHU 3%")
        );
        assert_eq!(cpu_text(&a, &a), None, "no time passed");
        assert_eq!(cpu_text(&a, &load(900, 700, 10)), None, "went back");
    }

    #[test]
    fn memory_reads_at_a_glance() {
        let l = Load {
            mem_total_kb: 54 * 1024,
            mem_available_kb: 31 * 1024,
            own_rss_kb: 10 * 1024,
            ..Load::default()
        };
        assert_eq!(memory_text(&l), "23 of 54 MB, LEHU 10 MB");
    }
}
