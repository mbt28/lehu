// SPDX-License-Identifier: GPL-3.0-or-later
//! The wireless half composed: one thread that takes the Bluetooth
//! connections to the Android Auto service and gives each its handoff,
//! takes the phone's TCP connection and starts the GAL session over it,
//! and watches the access point's carrier under that session.
//!
//! One phone at a time: a second TCP connection while a session runs is
//! refused, and the session ends only through its own link (the TCP
//! connection breaking or going silent) or the access point going down.
//! The thread does not start a second session after the first ended: the
//! loop restarts the wireless session, as it does every session. The
//! Bluetooth link of a handoff stays open as long as the phone keeps it,
//! so the handoffs run on threads of their own.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lehu_bt::RfcommHandle;
use lehu_session::{Event, Fault, Phase, Ports, Sinks};

use crate::aaw::Network;
use crate::handoff;
use crate::machine::{Config, phase_from_u8};
use crate::session::{Gal, lock};
use crate::tcp::Listener;
use crate::transport::StreamTransport;

/// Handoffs served at once: a phone's reconnection may overlap its old
/// connection; a third connection is closed unread.
const MAX_HANDOFFS: usize = 2;
/// How long the thread waits for a Bluetooth connection before it looks
/// at the TCP listener and the access point.
const WAIT: Duration = Duration::from_millis(200);

/// What the thread starts from.
pub(crate) struct Start {
    pub rfcomm: RfcommHandle,
    pub listener: Listener,
    pub network: Network,
    /// The access point's interface, whose carrier is watched.
    pub iface: String,
    pub config: Config,
    pub sinks: Box<dyn Sinks>,
    pub ports: Ports,
    /// Where the GAL session goes once a phone connected; the session's
    /// `command` and `stop` reach it there.
    pub gal: Arc<Mutex<Option<Gal>>>,
    pub phase: Arc<AtomicU8>,
    pub name: Arc<OnceLock<String>>,
    pub identity: Arc<OnceLock<String>>,
    /// Set to the phone a handoff sent the credentials to.
    pub bt_peer: Arc<Mutex<String>>,
}

/// The thread, as the session holds it. Dropping it ends the thread.
pub(crate) struct Waiter {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    port: u16,
}

impl Waiter {
    /// The TCP port the phones are told.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Ask the thread to end, without waiting for it.
    pub(crate) fn end(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// End the thread and wait for it: the profile is unregistered, the
    /// listener closed, the handoffs joined.
    pub(crate) fn stop(&mut self) {
        self.end();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn spawn(start: Start) -> io::Result<Waiter> {
    let running = Arc::new(AtomicBool::new(true));
    let port = start.listener.port();
    let thread = {
        let running = Arc::clone(&running);
        thread::Builder::new()
            .name("aa-wireless".into())
            .spawn(move || run(start, &running))?
    };
    Ok(Waiter {
        running,
        thread: Some(thread),
        port,
    })
}

fn run(s: Start, running: &Arc<AtomicBool>) {
    let Start {
        rfcomm,
        listener,
        network,
        iface,
        config,
        sinks,
        ports,
        gal,
        phase,
        name,
        identity,
        bt_peer,
    } = s;
    let network = Arc::new(network);
    let mut handoffs: Vec<JoinHandle<()>> = Vec::new();
    let mut sinks = Some(sinks);
    // The carrier is watched only once it was seen up with a phone in the
    // session: the AP coming up late is not a loss.
    let mut carrier_seen = false;
    let mut started = false;
    let phase_now = || phase_from_u8(phase.load(Ordering::Acquire));
    while running.load(Ordering::Acquire) {
        handoffs.retain(|t| !t.is_finished());
        let began = Instant::now();
        if let Some(conn) = rfcomm.accept(WAIT) {
            let peer = conn.peer;
            if handoffs.len() >= MAX_HANDOFFS {
                log::warn!("aa-bt: a third Bluetooth connection, from {peer}, closed");
            } else {
                log::info!("aa-bt: Bluetooth connection from {peer}");
                let network = Arc::clone(&network);
                let running = Arc::clone(running);
                let bt_peer = Arc::clone(&bt_peer);
                let spawned = thread::Builder::new().name("aa-bt".into()).spawn(move || {
                    let stream = conn.into_stream(Some(handoff::READ_WAIT));
                    let outcome = handoff::run(stream, &network, &running);
                    if outcome.handed_off {
                        *bt_peer.lock().unwrap_or_else(|e| e.into_inner()) = peer.to_string();
                    }
                    log::info!(
                        "aa-bt: the handoff for {peer} is over (credentials {}, phone {})",
                        if outcome.handed_off {
                            "sent"
                        } else {
                            "not sent"
                        },
                        if outcome.identity.id.is_empty() {
                            "unnamed"
                        } else {
                            &outcome.identity.id
                        }
                    );
                });
                match spawned {
                    Ok(t) => handoffs.push(t),
                    Err(e) => log::error!("aa-bt: cannot serve the Bluetooth connection: {e}"),
                }
            }
        }
        // The accept waited the whole turn when no phone came; the manager
        // gone closes its queue and makes it return at once, and the
        // listener's poll then keeps the pace.
        let left = WAIT.saturating_sub(began.elapsed());
        if listener.poll(left.as_millis() as i32)
            && let Some((stream, peer)) = listener.accept()
        {
            if started {
                // One phone at a time: the running session is left alone.
                log::warn!("aa-tcp: a connection from {peer} while a session runs, refused");
                drop(stream);
            } else if let Some(sinks) = sinks.take() {
                log::info!("aa-tcp: phone connected from {peer}");
                let transport = Arc::new(StreamTransport::tcp(stream));
                match Gal::start(
                    transport,
                    config.clone(),
                    sinks,
                    ports.clone(),
                    Arc::clone(&phase),
                    Arc::clone(&name),
                    Arc::clone(&identity),
                ) {
                    Ok(g) => {
                        *lock(&gal) = Some(g);
                        started = true;
                    }
                    Err(e) => {
                        // Nothing to run the phone's session with: over.
                        log::error!("aa: cannot start the session over TCP: {e}");
                        finish(&ports, &phase, Some(Fault::LinkLost));
                        break;
                    }
                }
            }
        }
        // The access point going down under the session ends it; a phone
        // that left Wi-Fi is seen by the session's own link.
        let in_session = started && phase_now() != Phase::Closed;
        match lehu_wifi::carrier(&iface) {
            Some(true) => carrier_seen = true,
            Some(false) | None if carrier_seen && in_session => {
                log::warn!("aa: the access point went down; the wireless session ends");
                if let Some(g) = &*lock(&gal) {
                    g.abort();
                }
                break;
            }
            _ => {}
        }
        if started && phase_now() == Phase::Closed {
            break;
        }
    }
    // Asked to close before any phone connected: nothing else will say so.
    if !started {
        finish(&ports, &phase, None);
    }
    running.store(false, Ordering::Release);
    drop(rfcomm);
    drop(listener);
    for t in handoffs {
        let _ = t.join();
    }
    log::info!("aa: the wireless session's thread ends");
}

/// The session is over without a GAL session having run: the fault, if
/// any, then Closed, once.
fn finish(ports: &Ports, phase: &AtomicU8, fault: Option<Fault>) {
    if phase.swap(Phase::Closed as u8, Ordering::AcqRel) == Phase::Closed as u8 {
        return;
    }
    if let Some(f) = fault {
        ports.report(Event::Fault(f));
    }
    ports.report(Event::Phase(Phase::Closed));
}
