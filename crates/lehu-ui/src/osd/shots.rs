// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Every screen drawn into a PPM file, to look at rather than to assert:
//! `LEHU_SHOTS=<dir> cargo test -p lehu-ui shots -- --ignored`.

use super::*;
use crate::host::{
    AudioOutput, BtDevice, KnownDevice, Media, NavArrow, NavCard, Prompt, PromptKind,
};
use crate::keys::OsdKey;
use crate::test_host::TestHost;

const W: u32 = 800;
const H: u32 = 480;

struct Frame(Vec<u16>);

impl Surface for Frame {
    fn write_rows(&mut self, y0: u32, y1: u32, pixels: &[Rgb565Pixel]) {
        let s = (y0 * W) as usize;
        for (i, p) in pixels.iter().enumerate() {
            self.0[s + i] = p.0;
        }
        let _ = y1;
    }
}

struct Shot {
    osd: Osd<TestHost>,
    frame: Frame,
    dir: std::path::PathBuf,
}

impl Shot {
    fn new(host: TestHost) -> Self {
        let mut s = Self {
            osd: Osd::new(host, W, H).unwrap(),
            frame: Frame(vec![0; (W * H) as usize]),
            dir: std::env::var("LEHU_SHOTS").unwrap().into(),
        };
        s.tick();
        s
    }
    fn tick(&mut self) {
        for _ in 0..3 {
            self.osd.tick(false, &mut self.frame);
        }
    }
    fn bridge(&self) -> Bridge<'_> {
        self.osd.ui.global::<Bridge<'_>>()
    }
    fn goto(&mut self, screen: i32) {
        self.bridge().set_screen(screen);
        self.tick();
    }
    fn key(&mut self, k: OsdKey) {
        self.osd.key(k, None, 1);
        self.tick();
    }
    fn keys(&mut self, k: OsdKey, n: usize) {
        for _ in 0..n {
            self.key(k);
        }
    }
    /// As after a touch: no ring left from earlier keys on the next switch.
    fn forget_keys(&mut self) {
        self.osd.st.skew += KEY_RECENT_MS + 1;
        self.tick();
    }
    fn save(&mut self, name: &str) {
        self.tick();
        let mut out = format!("P6\n{W} {H}\n255\n").into_bytes();
        for &p in &self.frame.0 {
            let r = ((p >> 11) & 0x1f) as u8;
            let g = ((p >> 5) & 0x3f) as u8;
            let b = (p & 0x1f) as u8;
            out.extend([r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2]);
        }
        std::fs::write(self.dir.join(format!("{name}.ppm")), out).unwrap();
    }
}

/// No phone, the clock at 14:32.
fn base_host() -> TestHost {
    let mut h = TestHost::new();
    h.local_time = Some((14 * 60 + 32) * 60_000);
    h
}

/// A CarPlay session waiting behind the screens, music playing, three
/// phones known, two remotes paired, a camera and two outputs.
fn phone_host() -> TestHost {
    let mut h = base_host();
    h.status = "CarPlay: Alex's iPhone".into();
    h.link_wifi = true;
    h.backgrounded = true;
    h.media = Media {
        available: true,
        title: "Night Drive".into(),
        artist: "The Long Roads".into(),
        album: "Open Roads".into(),
        duration_ms: 354_000,
        position_ms: 134_000,
        position_at: lehu_core::clock::now_ms(),
        playing: true,
    };
    h.media_gen += 1;
    let mut rgb = Vec::new();
    for y in 0..160u32 {
        for x in 0..160u32 {
            rgb.extend([(x * 255 / 160) as u8, 60, (y * 255 / 160) as u8]);
        }
    }
    h.art = Some(crate::host::Art::from_rgb(160, 160, &rgb));
    let phone = |id: &str, name: &str, detail: &str, active, nearby| KnownDevice {
        id: id.into(),
        name: name.into(),
        detail: detail.into(),
        active,
        nearby,
    };
    h.devices = vec![
        phone("a", "Alex's iPhone", "Connected - CarPlay", true, false),
        phone("b", "Pixel 8", "Nearby - Android Auto", false, true),
        phone("c", "Work iPhone", "Not here - CarPlay", false, false),
    ];
    h.devices_gen += 1;
    let bt = |id: &str, name: &str, detail: &str, connected| BtDevice {
        id: id.into(),
        name: name.into(),
        detail: detail.into(),
        connected,
    };
    h.bt_paired = vec![
        bt("k", "Rotary remote", "Connected", true),
        bt("j", "Keyboard", "Paired", false),
    ];
    h.bt_found = vec![bt("f", "Media remote", "Remote control", false)];
    h.bt_summary = "2 paired".into();
    h.bt_gen += 1;
    h.camera_present = true;
    h.settings.camera.key = 67;
    h.outputs = vec![
        AudioOutput {
            device: "hw:0,0".into(),
            label: "Built-in speaker".into(),
            active: true,
        },
        AudioOutput {
            device: "hw:CARD=Device,DEV=0".into(),
            label: "USB Audio Device".into(),
            active: false,
        },
    ];
    h.link = lehu_wifi::Link {
        up: true,
        channel: Some(lehu_wifi::Channel {
            mhz: 5180,
            width_mhz: 80,
        }),
        station: None,
        stations: 0,
        tx_bytes: 0,
        rx_bytes: 0,
    };
    h
}

fn night_host() -> TestHost {
    let mut h = phone_host();
    h.settings.display.night_mode = lehu_core::settings::NightMode::Night;
    h
}

fn guide(h: &mut TestHost, arrow: NavArrow, maneuver: &str) {
    h.nav = NavCard {
        active: true,
        arrow,
        maneuver: maneuver.into(),
        distance: "350 m".into(),
        road: "Hauptstrasse".into(),
        eta: "14:52".into(),
        eta_note: "arrival".into(),
    };
    h.nav_gen += 1;
}

#[test]
#[ignore]
fn shots() {
    // ── home ──
    let mut s = Shot::new(base_host());
    s.save("01-home-idle");
    s.key(OsdKey::Next);
    s.save("02-home-idle-ring");

    let mut s = Shot::new(phone_host());
    s.save("03-home-session");
    s.keys(OsdKey::Next, 4);
    s.save("04-home-ring");
    s.osd.host_mut().media.available = false;
    s.osd.host_mut().media_gen += 1;
    s.forget_keys();
    s.goto(SETTINGS);
    s.goto(HOME);
    s.save("05-home-session-nothing-playing");

    let mut s = Shot::new(phone_host());
    guide(s.osd.host_mut(), NavArrow::Left, "Turn left");
    s.tick();
    s.save("06-home-nav");

    // ── settings ──
    let mut s = Shot::new(phone_host());
    s.goto(SETTINGS);
    s.save("07-settings");
    s.goto(CONNECTION);
    s.save("64-connection");
    s.goto(SYSTEM);
    s.save("65-system");
    // Back, Night: the knob starts choosing and turns.
    s.goto(VIDEO);
    s.keys(OsdKey::Next, 2);
    s.key(OsdKey::Ok);
    s.key(OsdKey::Prev);
    s.save("08-video-night-choosing");
    s.key(OsdKey::Back);
    s.goto(AUDIO);
    s.keys(OsdKey::Next, 1);
    s.key(OsdKey::Ok);
    s.key(OsdKey::Next);
    s.save("09-audio-volume-changing");
    s.key(OsdKey::Ok);
    s.goto(SYSTEM);
    s.keys(OsdKey::Next, 3);
    s.key(OsdKey::Ok);
    s.key(OsdKey::Left);
    s.osd.st.skew += VOLUME_BAR_MS;
    s.save("10-system-power-choosing");
    s.key(OsdKey::Back);
    s.forget_keys();

    // ── info ──
    let mut h = phone_host();
    h.link_usb = true;
    h.load = lehu_core::sysinfo::Load {
        mem_total_kb: 54 * 1024,
        mem_available_kb: 31 * 1024,
        own_rss_kb: 10 * 1024,
        cpu_total: 1000,
        cpu_idle: 800,
        own_ticks: 10,
    };
    let mut s = Shot::new(h);
    s.goto(SYSTEM);
    s.goto(INFO);
    s.tick();
    {
        let h = s.osd.host_mut();
        h.load.cpu_total = 1100;
        h.load.cpu_idle = 860;
        h.load.own_ticks = 13;
        h.video = 625_000;
    }
    s.osd.st.skew += 1000;
    s.tick();
    s.save("66-info");

    // ── the pages under Settings ──
    let mut s = Shot::new(phone_host());
    s.goto(SETTINGS);
    s.goto(AUDIO);
    s.save("12-audio");
    // Back, the volume, the four levels, the buffer, Output.
    s.keys(OsdKey::Next, 8);
    s.key(OsdKey::Ok);
    s.key(OsdKey::Next);
    s.save("13-audio-output-choosing");
    s.goto(VIDEO);
    s.save("14-video");
    s.goto(WIRELESS);
    s.save("15-wireless");
    s.goto(ACCESS_POINT);
    s.save("16-access-point");
    s.goto(LINK);
    s.save("17-link-speed");
    s.goto(BLUETOOTH);
    s.save("18-bluetooth");
    s.bridge().invoke_bt_open("k".into());
    s.goto(BT_DEVICE);
    s.save("19-bluetooth-device");

    let mut s = Shot::new(phone_host());
    s.goto(SETTINGS);
    s.goto(DEVICES);
    s.save("20-phones");
    s.keys(OsdKey::Next, 3);
    s.key(OsdKey::Ok);
    s.save("21-phones-choosing");
    s.key(OsdKey::Right);
    s.key(OsdKey::Ok);
    s.save("22-dialog-forget");

    // ── update ──
    use crate::host::{UpdateOutcome, UpdateStage};
    let mut s = Shot::new(phone_host());
    s.goto(SYSTEM);
    s.keys(OsdKey::Next, 1);
    s.save("50-system-update-row");
    s.forget_keys();
    s.goto(UPDATE);
    s.save("51-update");
    s.goto(UPDATE_PHONE);
    let serve = |u: &mut crate::host::Update| {
        u.serving = true;
        u.ssid = "LEHU".into();
        u.passphrase = "lehu1234".into();
        u.url = "http://192.168.9.1:8080/".into();
    };
    serve(&mut s.osd.host_mut().update);
    s.osd.host_mut().update_gen += 1;
    s.tick();
    s.save("52-update-phone");
    {
        let u = &mut s.osd.host_mut().update;
        u.stage = UpdateStage::Receiving;
        u.received = 12 << 20;
        u.size = 37 << 20;
        u.can_cancel = true;
    }
    s.osd.host_mut().update_gen += 1;
    s.tick();
    s.save("53-update-phone-receiving");
    {
        let u = &mut s.osd.host_mut().update;
        u.stage = UpdateStage::Ready;
        u.new_version = "22.0.1-20261108095815".into();
        u.notes = "Faster start, a fix for wireless Android Auto".into();
        u.size_kb = 37 * 1024;
        u.restart = true;
    }
    s.osd.host_mut().update_gen += 1;
    s.tick();
    s.save("54-update-ready");
    s.bridge().invoke_update_cancel();
    s.tick();
    s.save("63-update-cancel-dialog");
    s.bridge().invoke_prompt_cancel();
    s.tick();
    s.bridge().invoke_update_install();
    s.tick();
    s.save("55-update-install-dialog");
    s.bridge().invoke_prompt_accept();
    {
        let u = &mut s.osd.host_mut().update;
        u.stage = UpdateStage::Installing;
        u.can_cancel = false;
        u.restart = false;
        u.percent = 40;
        u.message = "Receiving the update: 11 of 27 parts".into();
    }
    s.osd.host_mut().update_gen += 1;
    s.tick();
    s.save("56-update-installing");
    {
        let u = &mut s.osd.host_mut().update;
        u.stage = UpdateStage::Failed;
        u.message = "The update is not signed with LEHU OS's key".into();
    }
    s.osd.host_mut().update_gen += 1;
    s.tick();
    s.save("57-update-failed");
    let mut h = phone_host();
    h.update.available = false;
    let mut s = Shot::new(h);
    s.goto(SYSTEM);
    s.keys(OsdKey::Next, 1);
    s.save("58-system-update-not-available");
    let mut h = base_host();
    h.update.outcome = UpdateOutcome::Updated("22.0.1-20261108095815".into());
    let mut s = Shot::new(h);
    s.save("59-update-outcome");
    let mut h = night_host();
    h.update.outcome = UpdateOutcome::Interrupted;
    let mut s = Shot::new(h);
    s.save("60-update-interrupted-night");
    s.bridge().invoke_prompt_cancel();
    s.goto(SETTINGS);
    s.goto(UPDATE);
    s.goto(UPDATE_PHONE);
    serve(&mut s.osd.host_mut().update);
    s.osd.host_mut().update_gen += 1;
    s.tick();
    s.save("61-update-phone-night");

    // ── now playing ──
    let mut s = Shot::new(phone_host());
    s.goto(MEDIA);
    s.keys(OsdKey::Next, 2);
    s.save("23-media");
    let mut s = Shot::new(phone_host());
    guide(s.osd.host_mut(), NavArrow::Roundabout, "Roundabout, exit 2");
    s.goto(MEDIA);
    s.save("24-media-nav");

    // ── the editor, the dialogs, the volume bar ──
    let mut s = Shot::new(phone_host());
    s.goto(WIRELESS);
    s.bridge().invoke_edit_field(2);
    s.tick();
    s.save("25-editor");

    let mut s = Shot::new(phone_host());
    s.osd.host_mut().bt_prompt = Prompt {
        kind: PromptKind::Code,
        title: "Pair Rotary remote".into(),
        code: "482 913".into(),
        hint: "Type this code on the keyboard, then press Enter.".into(),
        accept: String::new(),
    };
    s.osd.host_mut().bt_gen += 1;
    s.tick();
    s.save("26-dialog-code");
    s.osd.host_mut().bt_prompt = Prompt {
        kind: PromptKind::Question,
        title: "Pair Keyboard?".into(),
        code: String::new(),
        hint: "Confirm that the keyboard shows 482913.".into(),
        accept: "Pair".into(),
    };
    s.osd.host_mut().bt_gen += 1;
    s.tick();
    s.save("27-dialog-question");

    let mut s = Shot::new(phone_host());
    s.goto(SETTINGS);
    s.osd.host_mut().volume_gen += 1;
    s.tick();
    s.save("28-volume-bar");

    // ── night ──
    let mut s = Shot::new(night_host());
    s.save("29-home-night");
    s.goto(VIDEO);
    s.keys(OsdKey::Next, 2);
    s.save("30-video-night");
    let mut s = Shot::new(night_host());
    guide(s.osd.host_mut(), NavArrow::SlightRight, "Bear right");
    s.goto(MEDIA);
    s.save("31-media-nav-night");
    s.goto(SETTINGS);
    s.goto(DEVICES);
    s.keys(OsdKey::Next, 3);
    s.key(OsdKey::Ok);
    s.key(OsdKey::Right);
    s.key(OsdKey::Ok);
    s.save("32-dialog-forget-night");

    // ── every turn arrow ──
    for (i, (arrow, words)) in [
        (NavArrow::Straight, "Straight on"),
        (NavArrow::SlightLeft, "Bear left"),
        (NavArrow::SharpLeft, "Sharp left"),
        (NavArrow::Right, "Turn right"),
        (NavArrow::SharpRight, "Sharp right"),
        (NavArrow::UTurn, "U-turn"),
        (NavArrow::Arrive, "Arrive"),
        (NavArrow::None, "Ferry"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut s = Shot::new(base_host());
        guide(s.osd.host_mut(), arrow, words);
        s.goto(SETTINGS);
        s.save(&format!("40-nav-arrow-{i}"));
    }
}
