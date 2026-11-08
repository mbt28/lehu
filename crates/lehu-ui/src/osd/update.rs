//! Settings > System > Update (screen 14) and "Get from phone" (screen 15).
//!
//! The program has the update (see [`crate::host::Update`]); this shows it.
//! "Get from phone" asks the program to serve the page and shows two QR
//! codes side by side -- the access point to join, the page to open --
//! drawn here, white with a quiet zone, each module a whole number of
//! pixels, at the size they show. They are made when the view opens or
//! what they say changes, and dropped when it closes. Once the update is
//! checked the view gives way to the Update page, where Install asks first
//! (the head unit restarts) and then shows the install's progress. Until
//! then Cancel update stops and removes it; an install cannot be stopped, so
//! from there on the screen stays on its progress. After a restart, how the
//! last update went is said once.

use qrcodegen::{QrCode, QrCodeEcc};
use slint::{Image, Rgb8Pixel, SharedPixelBuffer, SharedString};

use super::{Ask, Bridge, Host, State, UPDATE, UPDATE_PHONE, clean};
use crate::host::{UpdateOutcome, UpdateStage};

/// The white border a QR code needs, in modules.
const QUIET: u32 = 4;
/// A QR code's largest side on the screen, in pixels.
pub(super) const QR_MAX: u32 = 216;

pub(super) const HINT_CANCEL: &str = "What arrived is removed from the head unit. It can be \
                                      sent again from the phone.";
pub(super) const HINT_INSTALL: &str = "The head unit restarts when it is done, in about a \
                                       minute. A connected phone is disconnected first.";
pub(super) const HINT_INSTALL_BOOT: &str = "The head unit restarts and installs it, which \
                                            takes about 4 minutes. Keep the power on.";

/// The Wi-Fi as a phone's camera joins it: `WIFI:T:WPA;S:<ssid>;P:<pass>;;`
/// with `\ ; , " :` escaped by a backslash.
pub(super) fn wifi_text(ssid: &str, passphrase: &str) -> String {
    let mut out = String::with_capacity(ssid.len() + passphrase.len() + 24);
    if passphrase.is_empty() {
        out.push_str("WIFI:T:nopass;S:");
        escape(&mut out, ssid);
    } else {
        out.push_str("WIFI:T:WPA;S:");
        escape(&mut out, ssid);
        out.push_str(";P:");
        escape(&mut out, passphrase);
    }
    out.push_str(";;");
    out
}

fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        if matches!(c, '\\' | ';' | ',' | '"' | ':') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// `text` as a QR code at most `max` pixels square: black modules on white,
/// each `max / modules` pixels; `None` for nothing, or too long a text.
pub(super) fn qr_pixels(text: &str, max: u32) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    if text.is_empty() {
        return None;
    }
    let code = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let size = u32::try_from(code.size()).ok()?;
    let modules = size + 2 * QUIET;
    let scale = (max / modules).max(1);
    let side = modules * scale;
    let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(side, side);
    let width = side as usize;
    let buf = pixels.make_mut_slice();
    buf.fill(Rgb8Pixel::new(255, 255, 255));
    let black = Rgb8Pixel::new(0, 0, 0);
    for my in 0..size {
        for mx in 0..size {
            if !code.get_module(mx as i32, my as i32) {
                continue;
            }
            let (x0, y0) = ((mx + QUIET) * scale, (my + QUIET) * scale);
            for y in y0..y0 + scale {
                let row = y as usize * width;
                buf[row + x0 as usize..row + (x0 + scale) as usize].fill(black);
            }
        }
    }
    Some(pixels)
}

fn qr_image(text: &str) -> Image {
    qr_pixels(text, QR_MAX).map_or_else(Image::default, Image::from_rgb8)
}

/// Bytes as the screens show them: KB under a megabyte, MB with one
/// decimal under 10.
pub(super) fn mb(bytes: u64) -> String {
    let tenths = bytes * 10 / (1 << 20);
    if bytes < 1 << 20 {
        format!("{} KB", bytes.div_ceil(1024))
    } else if tenths < 100 {
        format!("{}.{} MB", tenths / 10, tenths % 10)
    } else {
        format!("{} MB", bytes.div_ceil(1 << 20))
    }
}

/// As `Bridge.update-stage` numbers them.
fn stage_id(stage: UpdateStage) -> i32 {
    match stage {
        UpdateStage::Idle => 0,
        UpdateStage::Receiving => 1,
        UpdateStage::Checking => 2,
        UpdateStage::Ready => 3,
        UpdateStage::Failed => 4,
        UpdateStage::Installing => 5,
        UpdateStage::Restarting => 6,
    }
}

impl<H: Host> State<H> {
    /// The update -> the Settings row and the two pages, on change; the
    /// program told whether the pages show; the outcome of the last update
    /// asked once.
    ///
    /// Before the pump looks at the screen: a switch made here is seen in
    /// the same tick.
    pub(super) fn pump_update(&mut self, bridge: &Bridge<'_>) {
        if std::mem::take(&mut self.update_goto) {
            bridge.set_screen(UPDATE);
        }
        // An install cannot be stopped: its progress stays on the screen,
        // whichever way anything tried to leave it. Seen in this same tick,
        // so nothing else draws.
        let stage = self.host.update().stage;
        if matches!(stage, UpdateStage::Installing | UpdateStage::Restarting)
            && bridge.get_screen() != UPDATE
        {
            bridge.set_screen(UPDATE);
        }
        let generation = self.host.update_generation();
        let changed = !self.update_seen || generation != self.p_update_gen;
        if changed {
            self.update_seen = true;
            self.p_update_gen = generation;
            self.show_update(bridge);
        }
        let screen = bridge.get_screen();
        let tree = !self.over_phone && (screen == UPDATE || screen == UPDATE_PHONE);
        if self.p_update_tree != Some(tree) {
            self.p_update_tree = Some(tree);
            self.host.update_screens(tree);
        }
        // The QR codes: made as the view opens or what they say changes,
        // dropped when it closes.
        if changed || screen != self.p_screen {
            let u = self.host.update();
            let (join, page) = if screen == UPDATE_PHONE && u.serving {
                (wifi_text(&u.ssid, &u.passphrase), u.url.clone())
            } else {
                (String::new(), String::new())
            };
            if self.p_qr_join.update(&join) {
                bridge.set_update_qr_join(qr_image(&join));
            }
            if self.p_qr_page.update(&page) {
                bridge.set_update_qr_page(qr_image(&page));
            }
        }

        // How the last update went: once, when no other question is up.
        if !self.update_asked
            && self.system_ask == Ask::None
            && self.host.update().outcome != UpdateOutcome::None
        {
            self.update_asked = true;
            self.system_ask = Ask::UpdateOutcome;
            self.dialog_dirty = true;
        }
    }

    fn show_update(&mut self, bridge: &Bridge<'_>) {
        let u = self.host.update();
        let stage = u.stage;
        bridge.set_update_shown(u.shown);
        bridge.set_update_available(u.available);
        bridge.set_update_can_cancel(u.can_cancel);
        let can_cancel = u.can_cancel;
        let label = if !u.available {
            "Not available on this system"
        } else {
            match stage {
                UpdateStage::Receiving => "Receiving",
                UpdateStage::Checking => "Checking",
                UpdateStage::Ready => "Update ready",
                UpdateStage::Installing => "Installing",
                UpdateStage::Restarting => "Restarting",
                UpdateStage::Idle | UpdateStage::Failed => u.version.as_str(),
            }
        };
        bridge.set_update_label(clean(&mut self.scratch, label));
        // The System row says so only when there is something going on.
        let attention = if u.available && !matches!(stage, UpdateStage::Idle | UpdateStage::Failed)
        {
            label
        } else {
            ""
        };
        bridge.set_update_attention(clean(&mut self.scratch, attention));
        let version = if u.version.is_empty() {
            "Unknown"
        } else {
            u.version.as_str()
        };
        bridge.set_update_version(clean(&mut self.scratch, version));
        bridge.set_update_stage(stage_id(stage));
        let partial = stage == UpdateStage::Receiving || stage == UpdateStage::Idle;
        let transfer = if u.size > 0 && partial && (u.received > 0 || stage != UpdateStage::Idle) {
            format!("{} of {}", mb(u.received), mb(u.size))
        } else {
            String::new()
        };
        bridge.set_update_transfer(SharedString::from(transfer.as_str()));
        let progress = match stage {
            UpdateStage::Receiving if u.size > 0 => u.received as f32 / u.size as f32,
            UpdateStage::Installing => u.percent.min(100) as f32 / 100.0,
            _ => 0.0,
        };
        bridge.set_update_progress(progress);
        bridge.set_update_new_version(clean(&mut self.scratch, &u.new_version));
        bridge.set_update_notes(clean(&mut self.scratch, &u.notes));
        bridge.set_update_size(SharedString::from(mb(u.size_kb * 1024).as_str()));
        bridge.set_update_mode(SharedString::from(if u.restart {
            "Installs with a restart, about 4 minutes"
        } else {
            "Installs in place, about a minute"
        }));
        bridge.set_update_message(clean(&mut self.scratch, &u.message));
        bridge.set_update_ssid(clean(&mut self.scratch, &u.ssid));
        bridge.set_update_pass(clean(&mut self.scratch, &u.passphrase));
        bridge.set_update_url(clean(&mut self.scratch, &u.url));
        bridge.set_update_serve_error(clean(&mut self.scratch, &u.serve_error));

        // Rows that went with the stage: the ring leaves them.
        let ring = bridge.get_ring_row();
        let installing = matches!(stage, UpdateStage::Installing | UpdateStage::Restarting);
        if (ring.as_str() == "install" && stage != UpdateStage::Ready)
            || (matches!(ring.as_str(), "get" | "power") && installing)
            || (ring.as_str() == "cancel" && !can_cancel)
        {
            self.ring_gone(bridge);
        }
        // Checked while the phone's view shows: on to Install.
        if stage == UpdateStage::Ready
            && self.p_update_stage != UpdateStage::Ready
            && bridge.get_screen() == UPDATE_PHONE
        {
            if let Some(slot) = self.ring_memory.get_mut(UPDATE as usize) {
                *slot = "install".to_string();
            }
            bridge.set_screen(UPDATE);
        }
        self.p_update_stage = stage;
    }

    /// The dialog for the last update's outcome: (kind, title, accept),
    /// its hint in `update_hint`. Interrupted offers the Update page, where
    /// it can be installed again.
    pub(super) fn outcome_dialog(&mut self) -> (i32, &'static str, &'static str) {
        let hint = &mut self.update_hint;
        hint.clear();
        match &self.host.update().outcome {
            UpdateOutcome::Updated(version) => {
                hint.push_str("The head unit runs version ");
                hint.push_str(version);
                hint.push_str(" now.");
                (3, "Updated", "")
            }
            UpdateOutcome::Failed(why) => {
                hint.push_str(why);
                (3, "The update failed", "")
            }
            UpdateOutcome::Interrupted => {
                hint.push_str(
                    "It was not installed. It can be installed again from Settings > System > Update.",
                );
                (2, "The update was interrupted", "Update")
            }
            UpdateOutcome::None => (0, "", ""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wifi_qr_text_escapes_its_specials() {
        assert_eq!(
            wifi_text("LEHU", "lehu1234"),
            "WIFI:T:WPA;S:LEHU;P:lehu1234;;"
        );
        assert_eq!(
            wifi_text(r#"My;Car,"1":\"#, "p;a:s,s\\"),
            r#"WIFI:T:WPA;S:My\;Car\,\"1\"\:\\;P:p\;a\:s\,s\\;;"#
        );
        assert_eq!(wifi_text("Open", ""), "WIFI:T:nopass;S:Open;;");
        assert_eq!(
            wifi_text("Zoë", "pässword"),
            "WIFI:T:WPA;S:Zoë;P:pässword;;"
        );
    }

    #[test]
    fn a_qr_code_is_whole_modules_on_white() {
        let p = qr_pixels("http://192.168.9.1:8080/", QR_MAX).unwrap();
        let side = p.width();
        assert_eq!(side, p.height());
        assert!((QR_MAX - 40..=QR_MAX).contains(&side), "{side}");
        // Version 2 (25 modules) and its quiet zone, at a whole scale.
        let modules = 25 + 2 * QUIET;
        assert_eq!(side % modules, 0);
        let scale = side / modules;
        let at = |x: u32, y: u32| p.as_slice()[(y * side + x) as usize];
        let white = Rgb8Pixel::new(255, 255, 255);
        let black = Rgb8Pixel::new(0, 0, 0);
        // The quiet zone is white, the finder pattern's corner black, and
        // its first ring white.
        assert_eq!(at(0, 0), white);
        assert_eq!(at(QUIET * scale - 1, QUIET * scale - 1), white);
        assert_eq!(at(QUIET * scale, QUIET * scale), black);
        assert_eq!(at((QUIET + 1) * scale, (QUIET + 1) * scale), white);
        assert_eq!(at(side - 1, side - 1), white);
        // The longest Wi-Fi (32-byte name, 63-byte password, escaped) fits.
        let long = wifi_text(&";".repeat(32), &",".repeat(63));
        let p = qr_pixels(&long, QR_MAX).unwrap();
        assert!(p.width() <= QR_MAX && p.width() >= 2 * 60, "{}", p.width());
        assert!(qr_pixels("", QR_MAX).is_none());
    }

    #[test]
    fn sizes_in_mb() {
        assert_eq!(mb(0), "0 KB");
        assert_eq!(mb(40 * 1024), "40 KB");
        assert_eq!(mb(40 * 1024 + 1), "41 KB");
        assert_eq!(mb(1 << 20), "1.0 MB");
        assert_eq!(mb(37 << 20), "37 MB");
        assert_eq!(mb((37 << 20) + 1), "38 MB");
        assert_eq!(mb((9 << 20) + (1 << 19)), "9.5 MB");
    }
}
