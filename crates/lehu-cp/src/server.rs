// SPDX-License-Identifier: GPL-3.0-or-later
//! The control server: accepts the phone's connections to the control
//! port and gives each its thread. Two at once at most (the phone's
//! reconnection overlapping its old connection); more are refused.

use std::io;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::control::{self, ConnHandle};
use crate::session::Shared;

const MAX_CONNECTIONS: usize = 2;

pub fn run(listener: TcpListener, shared: Arc<Shared>) {
    let mut threads: Vec<(Arc<ConnHandle>, JoinHandle<()>)> = Vec::new();
    while shared.is_running() {
        let mut p = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd.
        let ready = unsafe { libc::poll(&mut p, 1, 500) } > 0;
        // Threads that ended are joined here, so the handles do not pile up.
        threads.retain(|(_, t)| !t.is_finished());
        let mut live = shared.connections.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|h| threads.iter().any(|(t, _)| Arc::ptr_eq(t, h)));
        drop(live);
        if !ready {
            continue;
        }
        let (stream, peer) = match listener.accept() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                log::error!("carplay: accept: {e}");
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if threads.len() >= MAX_CONNECTIONS {
            log::warn!("carplay: a third control connection from {peer}, refused");
            continue;
        }
        let handle = ConnHandle::new();
        shared
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&handle));
        let s = Arc::clone(&shared);
        let h = Arc::clone(&handle);
        match thread::Builder::new()
            .name("cp-ctl".into())
            .spawn(move || control::serve(stream, peer, s, h))
        {
            Ok(t) => threads.push((handle, t)),
            Err(e) => log::error!("carplay: cannot serve the connection: {e}"),
        }
    }
    for (h, _) in &threads {
        h.stop.store(true, Ordering::Release);
    }
    for (_, t) in threads {
        let _ = t.join();
    }
}
