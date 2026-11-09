// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! The album art over iAP2 file transfer: a session of its own on the link
//! (declared in the link synchronisation, `LinkSync::proposal`), no
//! identification needed. The now playing update names the artwork's
//! transfer id; the phone then sets the transfer up with the file's size,
//! the head unit says start, the file comes in datagrams of `[id][control]
//! [data]`, and the head unit says success. The control codes and the
//! replies follow LIVI's file transfer receiver (GPL-3.0, see NOTICE).
//!
//! One file at a time, bounded by `MAX_ARTWORK`: data running past it is
//! cancelled and its bytes dropped. The setup's own fields are not read:
//! the bound holds whatever they say.

use lehu_session::MAX_ARTWORK;

// The control byte: a code, or data with its first and last flags.
const START: u8 = 0x01;
const CANCEL: u8 = 0x02;
const PAUSE: u8 = 0x03;
const SETUP: u8 = 0x04;
const SUCCESS: u8 = 0x05;
const FAILURE: u8 = 0x06;
const FIRST: u8 = 0x80;
const LAST: u8 = 0x40;

/// What a datagram asks of the head unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Send this datagram back.
    Reply([u8; 2]),
    /// The art is whole (`ArtTransfer::file`): send this back.
    Done([u8; 2]),
}

#[derive(Debug, Default)]
pub struct ArtTransfer {
    /// The transfer the newest now playing update named, if one did.
    expected: Option<u8>,
    /// The transfer under way.
    current: Option<u8>,
    file: Vec<u8>,
}

impl ArtTransfer {
    /// The now playing update named transfer `id` as the item's artwork,
    /// or (`None`) a new item without one: a transfer that ends after
    /// this is only the art if it is the one named.
    pub fn expect(&mut self, id: Option<u8>) {
        self.expected = id;
    }

    /// The file a `Step::Done` completed; whoever takes it leaves an empty
    /// buffer (or the previous art's, to be dropped).
    pub fn file(&mut self) -> &mut Vec<u8> {
        &mut self.file
    }

    /// One datagram from the phone.
    pub fn feed(&mut self, datagram: &[u8]) -> Step {
        let [id, control, data @ ..] = datagram else {
            return Step::Nothing;
        };
        let (id, control) = (*id, *control);
        if control & !(FIRST | LAST) == 0 {
            return self.data(id, control, data);
        }
        match control {
            SETUP => {
                self.current = Some(id);
                self.file.clear();
                Step::Reply([id, START])
            }
            CANCEL | PAUSE | FAILURE => {
                if self.current == Some(id) {
                    self.drop_file();
                }
                Step::Nothing
            }
            _ => Step::Nothing,
        }
    }

    fn data(&mut self, id: u8, control: u8, data: &[u8]) -> Step {
        if control & FIRST != 0 {
            // A transfer may start with its data, setup or not.
            self.current = Some(id);
            self.file.clear();
        }
        if self.current != Some(id) {
            return Step::Nothing;
        }
        if self.file.len() + data.len() > MAX_ARTWORK {
            log::debug!("carplay: album art past {MAX_ARTWORK} bytes, cancelled");
            self.drop_file();
            return Step::Reply([id, CANCEL]);
        }
        self.file.extend_from_slice(data);
        if control & LAST == 0 {
            return Step::Nothing;
        }
        self.current = None;
        if self.expected.is_some_and(|e| e != id) {
            // Not the art of the item playing now.
            self.drop_file();
            return Step::Reply([id, SUCCESS]);
        }
        Step::Done([id, SUCCESS])
    }

    fn drop_file(&mut self) {
        self.current = None;
        self.file = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_is_set_up_received_and_acknowledged() {
        let mut t = ArtTransfer::default();
        t.expect(Some(3));
        let mut setup = vec![3, SETUP];
        setup.extend_from_slice(&5u64.to_be_bytes());
        assert_eq!(t.feed(&setup), Step::Reply([3, START]));
        assert_eq!(
            t.feed(&[3, SETUP]),
            Step::Reply([3, START]),
            "a setup again"
        );
        assert_eq!(t.feed(&[3, FIRST, 1, 2]), Step::Nothing);
        assert_eq!(t.feed(&[3, 0, 3, 4]), Step::Nothing);
        assert_eq!(t.feed(&[9, 0, 9, 9]), Step::Nothing, "another transfer's");
        assert_eq!(t.feed(&[3, LAST, 5]), Step::Done([3, SUCCESS]));
        assert_eq!(t.file(), &[1, 2, 3, 4, 5]);
        // One datagram, no setup.
        t.expect(None);
        assert_eq!(t.feed(&[4, FIRST | LAST, 7]), Step::Done([4, SUCCESS]));
        assert_eq!(t.file(), &[7]);
        assert_eq!(t.feed(&[]), Step::Nothing);
        assert_eq!(t.feed(&[4]), Step::Nothing);
    }

    #[test]
    fn a_file_too_large_or_not_named_is_dropped() {
        let mut t = ArtTransfer::default();
        // Data running past the bound.
        assert_eq!(t.feed(&[2, SETUP]), Step::Reply([2, START]));
        let chunk = vec![0u8; 60_000];
        let mut d = vec![2, 0];
        d.extend_from_slice(&chunk);
        let mut steps = Vec::new();
        for _ in 0..10 {
            steps.push(t.feed(&d));
        }
        assert!(steps.contains(&Step::Reply([2, CANCEL])));
        assert!(t.file().is_empty());
        // The phone cancels.
        assert_eq!(t.feed(&[5, FIRST, 1]), Step::Nothing);
        assert_eq!(t.feed(&[5, CANCEL]), Step::Nothing);
        assert_eq!(t.feed(&[5, LAST, 2]), Step::Nothing);
        // A transfer that is not the item's art is acknowledged, not kept.
        t.expect(Some(8));
        assert_eq!(t.feed(&[7, FIRST | LAST, 1]), Step::Reply([7, SUCCESS]));
        assert!(t.file().is_empty());
    }
}
