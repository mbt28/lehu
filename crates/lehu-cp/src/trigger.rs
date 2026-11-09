// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What the wired and the wireless triggers share: the head unit's ids as
//! the phone is told them, the answers to the control-session messages
//! both paths see (the identification's outcome, the MFi authentication,
//! the clock, now playing and route guidance), and the album art over the
//! link's file transfer session.

use std::io::{self, Read, Write};
use std::sync::Arc;

use lehu_session::Fault;

use crate::carplay_msg::{self as msg, Declared};
use crate::file::{ArtTransfer, Step};
use crate::iap2;
use crate::media::{self, Route};
use crate::pairing::Store;
use crate::session::Shared;

/// A MAC-shaped id derived from the pairing id, locally administered: the
/// device id the phone stores us under, the same for every transport.
pub fn device_id(store: &Store) -> String {
    let h = crate::crypto::sha256(store.pairing_id.as_bytes());
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        h[0] | 0x02,
        h[1],
        h[2],
        h[3],
        h[4],
        h[5]
    )
}

/// Our Ed25519 public key as the phone wants it, hex.
pub fn public_key_hex(store: &Store) -> String {
    store.public.iter().map(|b| format!("{b:02x}")).collect()
}

/// What one iAP2 link has been through: the identification it was sent
/// and whether the phone took it and the authentication, so the now
/// playing and route guidance subscriptions go once both are done, the
/// route the guidance described so far and the album art coming.
pub struct LinkState {
    wired: bool,
    /// What the identification declared; the lean set after a rejection.
    pub declared: Declared,
    identified: bool,
    authenticated: bool,
    subscribed: bool,
    route: Route,
    art: ArtTransfer,
}

impl LinkState {
    pub fn new(wired: bool) -> Self {
        Self {
            wired,
            declared: Declared::FULL,
            identified: false,
            authenticated: false,
            subscribed: false,
            route: Route::default(),
            art: ArtTransfer::default(),
        }
    }

    /// The phone rejected the identification: true when it is to be sent
    /// again with the set declared before now playing and guidance were
    /// added, which phones accept; false when that was already the one.
    pub fn retry_lean(&mut self, csm: &[u8]) -> bool {
        // The parameters name the fields the phone objected to.
        let fields: Vec<u16> = iap2::parse_csm(csm)
            .map(|(_, p)| p.iter().map(|(k, _)| *k).collect())
            .unwrap_or_default();
        let lean = Declared::lean(self.wired);
        if self.declared == lean {
            return false;
        }
        log::warn!(
            "carplay: identification rejected (fields {fields:?}); again without route guidance"
        );
        self.declared = lean;
        true
    }

    /// Both steps done: subscribe to what the identification declared.
    fn subscribe<S: Read + Write>(&mut self, link: &mut iap2::Link<S>) -> io::Result<()> {
        if !self.identified || !self.authenticated || self.subscribed {
            return Ok(());
        }
        self.subscribed = true;
        // The art comes over the link's file transfer session, if the
        // phone took one in the link synchronisation.
        let artwork = self.declared.artwork && link.file_session().is_some();
        if self.declared.now_playing {
            link.send_control(&media::start_now_playing(artwork))?;
        }
        if self.declared.guidance {
            link.send_control(&media::start_route_guidance())?;
        }
        let said = |on: bool, what: &'static str| if on { what } else { "" };
        log::info!(
            "carplay: subscribed to{}{}{}",
            said(self.declared.now_playing, " now playing"),
            said(self.declared.now_playing && artwork, " with art"),
            said(self.declared.guidance, " route guidance"),
        );
        Ok(())
    }
}

/// Answer a control-session message both triggers handle alike: the
/// identification's outcome, the MFi authentication with the chip, the
/// phone's clock, now playing and route guidance into the side channel.
/// `None` when `id` is not one of them. A rejected identification the
/// caller sends again first (`LinkState::retry_lean`).
pub fn answer_common<S: Read + Write>(
    shared: &Arc<Shared>,
    link: &mut iap2::Link<S>,
    state: &mut LinkState,
    id: u16,
    csm: &[u8],
) -> Option<io::Result<()>> {
    let result = match id {
        msg::MSG_IDENTIFICATION_ACCEPTED => {
            log::info!("carplay: identification accepted");
            state.identified = true;
            state.subscribe(link)
        }
        msg::MSG_IDENTIFICATION_REJECTED => {
            let fields: Vec<u16> = iap2::parse_csm(csm)
                .map(|(_, p)| p.iter().map(|(k, _)| *k).collect())
                .unwrap_or_default();
            log::warn!("carplay: identification rejected (fields {fields:?})");
            shared.report(lehu_session::Event::Fault(Fault::Rejected));
            Ok(())
        }
        msg::MSG_REQUEST_AUTH_CERTIFICATE => {
            let mut chip = shared.chip.lock().unwrap_or_else(|e| e.into_inner());
            match chip.as_mut().and_then(|c| c.certificate()) {
                Some(cert) => link.send_control(&msg::auth_certificate(&cert)),
                None => {
                    log::warn!("carplay: the phone wants the MFi certificate and there is no chip");
                    Ok(())
                }
            }
        }
        msg::MSG_REQUEST_AUTH_CHALLENGE_RESPONSE => {
            let Some(challenge) = msg::auth_challenge(csm) else {
                return Some(Ok(()));
            };
            let mut chip = shared.chip.lock().unwrap_or_else(|e| e.into_inner());
            match chip.as_mut().and_then(|c| c.sign(&challenge)) {
                Some(sig) => link.send_control(&msg::auth_response(&sig)),
                None => {
                    log::warn!("carplay: the MFi chip did not sign the challenge");
                    Ok(())
                }
            }
        }
        msg::MSG_AUTH_SUCCEEDED => {
            log::info!("carplay: MFi authentication succeeded");
            state.authenticated = true;
            state.subscribe(link)
        }
        msg::MSG_AUTH_FAILED => {
            log::warn!("carplay: MFi authentication failed");
            shared.report(lehu_session::Event::Fault(Fault::Rejected));
            Ok(())
        }
        msg::MSG_DEVICE_TIME_UPDATE => {
            if let Some(t) = msg::device_time_update(csm) {
                shared.report(lehu_session::Event::Clock {
                    unix_ms: t.unix_ms(),
                    zone_offset_min: t.zone_offset_min,
                    dst_offset_min: t.dst_offset_min,
                });
            }
            Ok(())
        }
        media::MSG_NOW_PLAYING_UPDATE => {
            let now = lehu_core::clock::now_ms();
            let mut art = None;
            shared.ports.tell_now_playing(|np| {
                art = media::now_playing_update(csm, np, now);
                art.is_some()
            });
            match art {
                Some(media::ItemArt {
                    transfer: Some(id), ..
                }) => state.art.expect(Some(id)),
                Some(media::ItemArt { new_item: true, .. }) => {
                    // Another item, no art named for it (yet): the last
                    // one's goes.
                    state.art.expect(None);
                    shared.ports.tell_artwork(|a| a.clear());
                }
                _ => {}
            }
            Ok(())
        }
        media::MSG_ROUTE_GUIDANCE_UPDATE | media::MSG_ROUTE_GUIDANCE_MANEUVER => {
            let route = &mut state.route;
            shared
                .ports
                .tell_guidance(|g| media::route_guidance(csm, route, g));
            Ok(())
        }
        _ => return None,
    };
    Some(result)
}

/// A file transfer datagram: the album art coming, answered as the
/// transfer asks; once whole, it is the art of what plays.
pub fn file_datagram<S: Read + Write>(
    shared: &Arc<Shared>,
    link: &mut iap2::Link<S>,
    state: &mut LinkState,
    datagram: &[u8],
) -> io::Result<()> {
    match state.art.feed(datagram) {
        Step::Nothing => Ok(()),
        Step::Reply(reply) => link.send_file(&reply),
        Step::Done(reply) => {
            let file = state.art.file();
            log::debug!("carplay: album art, {} bytes", file.len());
            shared.ports.tell_artwork(|a| a.replace(file));
            // The old art's buffer, or nothing.
            *file = Vec::new();
            link.send_file(&reply)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_id_is_a_local_mac_from_the_pairing_id() {
        let dir = std::env::temp_dir().join(format!("lehu-cp-trigger-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let id = device_id(&store);
        assert_eq!(id.len(), 17);
        let first = u8::from_str_radix(&id[..2], 16).unwrap();
        assert_eq!(first & 0x02, 0x02, "locally administered");
        assert_eq!(id, device_id(&store), "stable");
        assert_eq!(public_key_hex(&store).len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
