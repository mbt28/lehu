// SPDX-License-Identifier: GPL-3.0-or-later
//! Program state -> Bridge properties, on change only. Every property the
//! screens show from outside is compared against what was last pushed; an
//! unchanged tick sets nothing and the renderer stays idle.

use slint::{ComponentHandle, SharedString};

use lehu_core::settings::{BUFFER_MAX_MS, NightMode};

use super::{
    ACCESS_POINT, AUDIO, BLUETOOTH, BT_DEVICE, Bridge, CAMERA, CLOCK_UNSET_MS, CONNECTION, DEVICES,
    HOME, Host, INFO, LINK, PICK_MS, Pick, SETTINGS, State, UPDATE_PHONE, VIDEO, VOLUME_BAR_MS,
    clean, compose,
};
use crate::AppWindow;
use crate::keys::key_name;

impl<H: Host> State<H> {
    pub(super) fn pump(&mut self, ui: &AppWindow) {
        let bridge = ui.global::<Bridge<'_>>();
        self.switched = false;
        // An access point change whose "Restarting..." went out last tick.
        self.ap_write_pending();
        let now = self.now();

        // The home tiles: the session ("CarPlay: Alex's iPhone") and,
        // while one runs, its link -- a cable wins over Wi-Fi.
        if self.p_status.update(self.host.status()) {
            self.home_dirty = true;
        }
        let link = if self.host.link_usb() {
            "wired"
        } else if self.host.link_wifi() {
            "wireless"
        } else {
            ""
        };
        if link != self.p_link_label {
            self.p_link_label = link;
            self.home_dirty = true;
        }
        let waiting = self.host.session_backgrounded();
        if waiting != self.p_waiting {
            self.p_waiting = waiting;
            self.home_dirty = true;
            // The phone's tile and the one to connect one take turns.
            let ringed = if waiting { "connect" } else { "resume" };
            if bridge.get_ring_row().as_str() == ringed {
                self.ring_gone(&bridge);
            }
            bridge.set_session_waiting(waiting);
        }
        if self.home_dirty {
            self.home_dirty = false;
            self.pump_home(&bridge);
        }
        self.pump_clock(&bridge, now);
        let night = self.host.settings().display.night_mode;
        if Some(night) != self.p_night {
            self.p_night = Some(night);
            bridge.set_night_label(SharedString::from(super::pick::night_label(night)));
            bridge.set_night_choice(super::pick::night_index(night));
            bridge.set_night(night_palette(night));
        }
        let fps = self.host.settings().android_auto.fps;
        if Some(fps) != self.p_fps {
            self.p_fps = Some(fps);
            let label = compose(&mut self.scratch, format_args!("{fps} fps"));
            bridge.set_fps_label(SharedString::from(label));
            bridge.set_fps_choice(i32::from(fps >= 60));
        }

        // Volume keys change it from another thread: one load per tick.
        // Every change after the first look brings the volume bar up.
        let volume_gen = self.host.volume_generation();
        if !self.volume_seen || volume_gen != self.p_volume_gen {
            let change = self.volume_seen;
            self.volume_seen = true;
            self.p_volume_gen = volume_gen;
            // 0% is silent too, and shown as muted: there is no Mute row,
            // turning the level down to 0 is muting.
            let level = self.host.volume_level().min(100);
            let muted = self.host.volume_muted() || level == 0;
            let label = if muted {
                "Muted"
            } else {
                compose(&mut self.scratch, format_args!("{level}%"))
            };
            bridge.set_volume_label(SharedString::from(label));
            bridge.set_volume_level(level as i32);
            bridge.set_volume_muted(muted);
            if change {
                self.volume_bar_until = now + VOLUME_BAR_MS;
                if !self.volume_bar {
                    self.volume_bar = true;
                    bridge.set_volume_bar(true);
                }
            }
        }
        if self.volume_bar && now >= self.volume_bar_until {
            self.volume_bar = false;
            bridge.set_volume_bar(false);
        }

        // How the phone is connected: the Home screen's icons.
        let link_usb = self.host.link_usb();
        if link_usb != self.p_link_usb {
            self.p_link_usb = link_usb;
            bridge.set_link_usb(link_usb);
        }
        let link_wifi = self.host.link_wifi();
        if link_wifi != self.p_link_wifi {
            self.p_link_wifi = link_wifi;
            bridge.set_link_wifi(link_wifi);
        }
        if self.p_ssid.update(&self.host.settings().wifi.ssid) {
            bridge.set_wifi_ssid(clean(&mut self.scratch, self.p_ssid.as_str()));
        }
        if self.p_pass.update(&self.host.settings().wifi.passphrase) {
            bridge.set_wifi_pass(clean(&mut self.scratch, self.p_pass.as_str()));
        }
        if self.p_bt_name.update(&self.host.settings().bluetooth.name) {
            bridge.set_bt_name(clean(&mut self.scratch, self.p_bt_name.as_str()));
        }

        // A camera that could not start (or stopped): back where it was
        // opened from, saying why on the Video screen.
        if self.host.take_camera_failure(&mut self.why) {
            bridge.set_camera_error(clean(&mut self.scratch, &self.why));
            if bridge.get_screen() == CAMERA {
                bridge.set_screen(self.cam_return);
            }
        }
        // A dialog over the camera view would be under the picture: leave it.
        if bridge.get_screen() == CAMERA && bridge.get_prompt_kind() != 0 {
            bridge.set_screen(self.cam_return);
        }
        // The camera key, just learned (the question closes) or changed.
        if let Some(code) = self.host.take_learned_key() {
            self.persist("camera.key", toml::Value::Integer(i64::from(code)));
            if self.system_ask == super::Ask::CameraKey {
                self.system_ask = super::Ask::None;
                self.dialog_dirty = true;
            }
        }
        let camera_key = self.host.settings().camera.key;
        if Some(camera_key) != self.p_camera_key {
            self.p_camera_key = Some(camera_key);
            key_name(&mut self.scratch, camera_key);
            bridge.set_camera_key_label(SharedString::from(self.scratch.as_str()));
        }
        let fast_switch = self.host.settings().camera.fast_switch;
        if Some(fast_switch) != self.p_fast_switch {
            self.p_fast_switch = Some(fast_switch);
            bridge.set_camera_fastswitch(fast_switch);
        }

        let buffer_ms = self.host.settings().audio.buffer_ms;
        if Some(buffer_ms) != self.p_buffer_ms {
            self.p_buffer_ms = Some(buffer_ms);
            let label = compose(&mut self.scratch, format_args!("{buffer_ms} ms"));
            bridge.set_buffer_label(SharedString::from(label));
            let share = buffer_ms.min(BUFFER_MAX_MS) * 100 / BUFFER_MAX_MS.max(1);
            bridge.set_buffer_level(share as i32);
        }
        if self.buffer_save_at != 0 && now >= self.buffer_save_at {
            self.save_buffer();
        }
        self.pump_audio(&bridge);
        self.pump_output(&bridge);

        // A row picking in place stops off its screen, under a dialog, or
        // left alone.
        if self.pick != Pick::None
            && (bridge.get_screen() != self.pick_screen()
                || bridge.get_prompt_kind() != 0
                || now.saturating_sub(self.pick_last_ms) >= PICK_MS)
        {
            self.pick = Pick::None;
            self.show_pick(&bridge);
        }

        self.pump_update(&bridge);
        let screen = bridge.get_screen();
        if screen != self.p_screen {
            // The position is shown afresh, at the new screen's pace.
            self.p_media_second = None;
            // Back from the phones goes where they were opened from.
            if screen == DEVICES && (self.p_screen == HOME || self.p_screen == CONNECTION) {
                self.devices_parent = self.p_screen;
            }
            // The loop runs the camera while its view is up.
            self.host.set_camera_running(screen == CAMERA);
            if screen == CAMERA {
                // The column comes up on the menu, the ring on Back -- unless
                // a request (the camera key) switched the camera on: then
                // the picture alone, until a touch or a key.
                bridge.set_camera_error(SharedString::default());
                self.held = true;
                self.held_since = now;
                self.cam_return = if self.p_screen >= 0 && self.p_screen != CAMERA {
                    self.p_screen
                } else {
                    VIDEO
                };
                self.cam_page = 0;
                self.cam_row = 0;
                self.cam_editing = false;
                self.cam_rows_dirty = true;
                if self.cam_quiet {
                    self.hide_panel();
                } else {
                    self.show_panel();
                }
                self.cam_quiet = false;
            } else if screen == VIDEO && self.p_screen != CAMERA {
                // Opens it for a moment.
                bridge.set_camera_available(self.host.camera_present());
            } else if screen == AUDIO || (screen == SETTINGS && self.output_now.is_none()) {
                // A card plugged in since shows up; the Audio row names the
                // card, not the setting.
                self.refresh_outputs(&bridge);
            } else if screen == ACCESS_POINT {
                self.open_access_point();
            } else if screen == LINK {
                self.open_link();
            } else if screen == INFO {
                self.open_info(&bridge);
            } else if screen == UPDATE_PHONE {
                // The page is served while the phone may come for it.
                self.host.update_serve();
            }
        }
        self.pump_camera(&bridge, screen);
        self.pump_bluetooth(&bridge, screen);
        self.pump_devices(&bridge);
        self.pump_media(&bridge, screen, now);
        self.pump_access_point(&bridge);
        self.pump_link(&bridge, screen, now);
        self.pump_info(&bridge, screen, now);
        self.pump_dialog(&bridge);
        // After the dialog: one that just opened or closed is a switch now,
        // seen while the screen under it still has its ring; and it takes
        // the card away at once.
        let modal = bridge.get_prompt_kind() != 0 || self.editing >= 0;
        if screen != self.p_screen || modal != self.p_modal {
            self.focus_seed = true;
            self.switched = true;
            self.ring_lost = false; // the switch re-seeds the ring anyway
            // Where the ring was on the screen left (or covered): back on
            // it -- by Back, Home, or a dialog closing -- the ring goes
            // there again rather than to the first row.
            let ring = bridge.get_ring_row();
            if !self.p_modal
                && !ring.is_empty()
                && let Some(slot) = usize::try_from(self.p_screen)
                    .ok()
                    .and_then(|i| self.ring_memory.get_mut(i))
            {
                slot.clear();
                slot.push_str(ring.as_str());
            }
            let returning = screen == self.p_screen || self.goes_back(self.p_screen, screen);
            self.ring_target = if modal || !returning {
                None
            } else {
                usize::try_from(screen)
                    .ok()
                    .and_then(|i| self.ring_memory.get(i))
                    .filter(|key| !key.is_empty())
                    .cloned()
            };
            // A screen that went away does not report losing focus.
            bridge.set_ring_row(SharedString::default());
            let bt_now = screen == BLUETOOTH || screen == BT_DEVICE;
            let bt_before = self.p_screen == BLUETOOTH || self.p_screen == BT_DEVICE;
            if bt_now != bt_before {
                self.host.bt_screen_visible(bt_now);
            }
            self.p_screen = screen;
            self.p_modal = modal;
        }
        self.pump_nav(&bridge, screen, modal, now);
    }
}

/// The night palette: with Night, and never with Day. Auto follows what the
/// phone is told, and it is told day: CarPlay is left to darken by itself,
/// Android Auto's night sensor says day.
pub(super) fn night_palette(mode: NightMode) -> bool {
    mode == NightMode::Night
}

impl<H: Host> State<H> {
    /// The home tiles' words. While a session waits behind the screens,
    /// its tile: the protocol (the status before its colon) and the phone
    /// with its link. Otherwise what a phone is doing, if anything, under
    /// "Connect a phone".
    fn pump_home(&mut self, bridge: &Bridge<'_>) {
        let status = self.p_status.as_str();
        let idle = status.is_empty() || status == "No phone";
        let (title, phone) = if self.p_waiting {
            status.split_once(": ").unwrap_or((status, ""))
        } else {
            ("", "")
        };
        if self.p_phone_title.update(title) {
            bridge.set_phone_title(clean(&mut self.scratch, title));
        }
        let detail = match (phone, self.p_link_label) {
            (_, "") => compose(&mut self.scratch, format_args!("{phone}")),
            ("", link) => compose(&mut self.scratch, format_args!("{link}")),
            (phone, link) => compose(&mut self.scratch, format_args!("{phone} - {link}")),
        };
        if self.p_phone_detail.update(detail) {
            let detail = self.p_phone_detail.as_str();
            bridge.set_phone_detail(clean(&mut self.scratch, detail));
        }
        let note = if self.p_waiting || idle { "" } else { status };
        if self.p_status_note.update(note) {
            bridge.set_status_note(clean(&mut self.scratch, note));
        }
    }

    /// The bar's clock: the time looked at once a minute, on the minute,
    /// and written only when the minute it shows changed.
    fn pump_clock(&mut self, bridge: &Bridge<'_>, now: u64) {
        if now < self.clock_due {
            return;
        }
        let local = self.host.local_time_ms();
        let minute = local.map(|ms| (ms / 60_000 % 1440) as u32);
        self.clock_due = match local {
            Some(ms) => now + 60_000 - ms % 60_000,
            None => now + CLOCK_UNSET_MS,
        };
        if self.p_clock == Some(minute) {
            return;
        }
        self.p_clock = Some(minute);
        let label = match minute {
            Some(m) => compose(
                &mut self.scratch,
                format_args!("{:02}:{:02}", m / 60, m % 60),
            ),
            None => "",
        };
        bridge.set_clock(SharedString::from(label));
    }
}
