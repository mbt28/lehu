// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The wired trigger, on its own thread: the chain from a plugged iPhone
//! to a phone that connects to our control port.
//!
//! 1. The phone is switched to configuration 6 and usbmux opened on it.
//! 2. The kernel's NCM link comes up with a link-local address.
//! 3. lockdown pairs (the first time) and starts the carkit service, a
//!    TLS stream.
//! 4. iAP2 is negotiated over it; the phone asks for the identification
//!    and the MFi authentication, then declares CarPlay available.
//! 5. CarPlayStartSession hands it our address and port. If it does not
//!    connect, the message is sent again on a schedule, the phone's USB
//!    reset once at 8 s, and the session given up after ten tries.
//!
//! A chain that breaks before any control connection (the phone locked,
//! a service that did not open) is tried again every two seconds while
//! the phone stays on the bus. The iAP2 link stays up for the session
//! (the phone keeps using it); once a control connection exists its loss
//! does not end the session.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use lehu_core::clock;
use lehu_session::{Fault, Phase};

use crate::carplay_msg as msg;
use crate::iap2::{self, Received};
use crate::lockdown::{self, RecordStore};
use crate::ncm;
use crate::session::Shared;
use crate::state::{Action, StartSchedule};
use crate::trigger;
use crate::usbmux::{self, MuxHost};

/// The phones whose USB was reset this plug: once per plug, process-wide,
/// since the reset re-enumerates the phone and the session that follows
/// is a new one.
static RESET_DONE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn reset_done(serial: &str) -> bool {
    RESET_DONE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|s| s.contains(serial))
}

fn note_reset(serial: &str, done: bool) {
    let mut g = RESET_DONE.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(HashSet::new);
    if done {
        set.insert(serial.to_string());
    } else {
        set.remove(serial);
    }
}

enum Outcome {
    /// The chain ran (or broke) and may be tried again.
    Again,
    /// The session is over.
    Done,
}

fn sleep_while_running(shared: &Shared, d: Duration) {
    let end = Instant::now() + d;
    while shared.is_running() && Instant::now() < end {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Reset the phone's port, wherever the phone is on the bus now (the
/// reveal moved it since `device` was seen).
fn reset_phone(serial: &str, device: &lehu_usb::Device) {
    let target = match usbmux::find_by_serial(serial) {
        Some(p) => lehu_usb::Device::new(p.bus, p.address, device.id, device.class, &p.sysname),
        None => *device,
    };
    if let Err(e) = lehu_usb::reset(&target) {
        log::warn!("carplay: the USB reset failed: {e}");
    }
}

pub fn run(shared: Arc<Shared>, device: lehu_usb::Device, port: u16) {
    let mut tries = 0u32;
    while shared.is_running() && !shared.closing.load(Ordering::Acquire) {
        if usbmux::find_by_serial(&shared.serial).is_none() {
            log::info!("carplay: the phone left the bus");
            shared.fail(Fault::LinkLost);
            return;
        }
        tries += 1;
        match attempt(&shared, &device, port, tries) {
            Ok(Outcome::Done) => return,
            Ok(Outcome::Again) => {}
            Err(why) => log::warn!("carplay: wired start failed: {why}"),
        }
        if shared.control_seen.load(Ordering::Acquire) {
            // The phone is in the session; the trigger has done its work.
            return;
        }
        sleep_while_running(&shared, Duration::from_secs(2));
    }
    // Asked to close before any phone connected: nothing else will say so.
    if shared.closing.load(Ordering::Acquire) && !shared.control_seen.load(Ordering::Acquire) {
        shared.set_phase(Phase::Closed);
    }
}

fn attempt(
    shared: &Arc<Shared>,
    device: &lehu_usb::Device,
    port: u16,
    try_no: u32,
) -> Result<Outcome, String> {
    let serial = shared.serial.clone();
    let phone = usbmux::configure_carplay(&serial, &shared.closing)?;
    let host = match MuxHost::open(&phone, shared.config.realtime) {
        Ok(h) => h,
        // A mux that stays silent is a phone whose USB side is stuck (left
        // in configuration 6 by an earlier run, or stalled): a port reset
        // brings it back, once per plug; the attempt after it goes through.
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut && !reset_done(&serial) => {
            log::warn!("carplay: the mux answers nothing; resetting the phone's USB");
            note_reset(&serial, true);
            reset_phone(&serial, device);
            return Err(format!("usbmux: {e}; the phone was reset"));
        }
        Err(e) => return Err(format!("usbmux: {e}")),
    };
    log::info!("carplay: usbmux up on {} (configuration 6)", phone.node());

    // The network link, with one rebind of the kernel drivers if the
    // claim knocked them off.
    let iface = ncm::interface_of(&phone.sysname);
    // Without CAP_NET_ADMIN this fails, which is fine where the image brings
    // the link up itself (a hotplug hook, systemd-networkd): it matters only
    // if no address ever comes.
    let brought_up = ncm::bring_up(&iface);
    let mut address = None;
    for i in 0..40 {
        if !shared.is_running() {
            host.close();
            return Ok(Outcome::Done);
        }
        address = ncm::link_local(&iface);
        if address.is_some() {
            break;
        }
        if i == 6 {
            ncm::rebind(&phone.sysname);
        }
        thread::sleep(Duration::from_millis(250));
    }
    let Some(address) = address else {
        host.close();
        if !brought_up && try_no == 1 {
            log::warn!(
                "carplay: could not bring {iface} up (needs CAP_NET_ADMIN, or the image's hotplug hook)"
            );
        }
        return Err(format!(
            "{iface} has no link-local address, the NCM link is down"
        ));
    };

    // The carkit channel.
    let store = RecordStore::new(&shared.config.state_dir);
    let mut carkit = match lockdown::open_carkit(&host, &serial, &store) {
        Ok(s) => s,
        Err(e) => {
            host.close();
            return Err(format!("carkit: {e}"));
        }
    };
    log::info!("carplay: carkit channel up, {iface} is {address}");
    carkit.set_read_timeout(Duration::from_millis(200));
    let mut link = iap2::Link::new(carkit);
    if let Err(e) = link.negotiate(Instant::now() + Duration::from_secs(10)) {
        host.close();
        return Err(format!("iAP2: {e}"));
    }

    // The control session: answer what the phone asks, then start.
    let pk_hex = trigger::public_key_hex(&shared.store);
    let device_id = trigger::device_id(&shared.store);
    let start = msg::wired_start_session(&msg::WiredSession {
        ip: &address,
        port: u32::from(port),
        device_id: &device_id,
        public_key: &pk_hex,
        source_version: crate::info::SOURCE_VERSION,
        ultra: shared.config.ultra_start(),
    });
    let mut schedule: Option<StartSchedule> = None;
    let mut state = trigger::LinkState::new(true);
    let mut csm = Vec::with_capacity(2048);
    let outcome = loop {
        if !shared.is_running() || shared.closing.load(Ordering::Acquire) {
            break Outcome::Done;
        }
        if shared.control_seen.load(Ordering::Acquire)
            && let Some(s) = schedule.as_mut()
        {
            s.arrived();
            note_reset(&serial, false);
        }
        match link.recv_control(&mut csm) {
            Ok(Received::Control) => {
                if let Err(e) = answer(
                    shared,
                    &mut link,
                    &mut state,
                    &csm,
                    &start,
                    &mut schedule,
                    (&address, port),
                ) {
                    log::warn!("carplay: iAP2: {e}");
                    break Outcome::Again;
                }
            }
            Ok(Received::File) => {
                if let Err(e) = trigger::file_datagram(shared, &mut link, &mut state, &csm) {
                    log::warn!("carplay: iAP2: {e}");
                    break Outcome::Again;
                }
            }
            Ok(Received::Nothing) => {}
            Err(e) => {
                if shared.control_seen.load(Ordering::Acquire) {
                    log::info!("carplay: the iAP2 link ended ({e}); the session goes on");
                    break Outcome::Done;
                }
                log::warn!("carplay: the iAP2 link ended before the phone connected ({e})");
                break Outcome::Again;
            }
        }
        if let Some(s) = schedule.as_mut() {
            match s.tick(clock::now_ms()) {
                Action::Wait(_) => {}
                Action::Resend => {
                    log::info!("carplay: the phone has not connected, CarPlayStartSession again");
                    if let Err(e) = link.send_control(&start) {
                        log::warn!("carplay: iAP2: {e}");
                        break Outcome::Again;
                    }
                }
                Action::Reset => {
                    log::warn!(
                        "carplay: no control connection after 8 s, resetting the phone's USB"
                    );
                    note_reset(&serial, true);
                    reset_phone(&serial, device);
                    shared.fail(Fault::SetupTimeout);
                    break Outcome::Done;
                }
                Action::GiveUp => {
                    log::warn!("carplay: the phone never connected after the start");
                    shared.fail(Fault::SetupTimeout);
                    break Outcome::Done;
                }
            }
        }
    };
    // The iAP2 link and the mux go with the attempt; a session in
    // progress runs over the network link, which the kernel keeps.
    drop(link);
    host.close();
    if matches!(outcome, Outcome::Done)
        && shared.phase() != Phase::Closed
        && !shared.control_seen.load(Ordering::Acquire)
    {
        shared.set_phase(Phase::Closed);
    }
    Ok(outcome)
}

/// One control-session message from the phone and its answer.
fn answer(
    shared: &Arc<Shared>,
    link: &mut iap2::Link<lockdown::Stream>,
    state: &mut trigger::LinkState,
    csm: &[u8],
    start: &[u8],
    schedule: &mut Option<StartSchedule>,
    (address, port): (&str, u16),
) -> Result<(), String> {
    let Some((id, _)) = iap2::parse_csm(csm) else {
        return Ok(());
    };
    let cfg = &shared.config;
    let identification = |state: &trigger::LinkState| {
        msg::wired_identification_themed(
            &cfg.name,
            &cfg.model,
            &cfg.manufacturer,
            state.declared,
            cfg.theme_asset(),
        )
    };
    if id == msg::MSG_IDENTIFICATION_REJECTED && state.retry_lean(csm) {
        return link
            .send_control(&identification(state))
            .map_err(|e| e.to_string());
    }
    if let Some(r) = trigger::answer_common(shared, link, state, id, csm) {
        return r.map_err(|e| e.to_string());
    }
    match id {
        msg::MSG_START_IDENTIFICATION => {
            log::info!("carplay: identification requested");
            link.send_control(&identification(state))
        }
        msg::MSG_CARPLAY_AVAILABILITY => {
            if schedule.is_none() {
                log::info!("carplay: CarPlayStartSession to [{address}]:{port}");
                if let Err(e) = link.send_control(start) {
                    return Err(e.to_string());
                }
                *schedule = Some(StartSchedule::new(
                    clock::now_ms(),
                    !reset_done(&shared.serial),
                ));
            }
            Ok(())
        }
        other => {
            log::debug!("carplay: iAP2 message {other:#06x} ignored");
            Ok(())
        }
    }
    .map_err(|e| e.to_string())
}
