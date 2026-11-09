// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The GAL framing: a four-byte header (channel, flags, u16BE payload size),
//! a u32BE total on a first fragment that is not also the last, then the
//! payload, encrypted or not as the flags say. There is no magic to resync
//! on, so anything out of step is fatal for the link.
//!
//! `Assembler` holds one reassembly buffer per channel and reuses it; the
//! caller appends the plain or decrypted payload into the buffer it hands
//! out. `Fragments` cuts an outgoing payload into frames of at most
//! `MAX_FRAME_PAYLOAD` bytes.

use crate::consts::*;

/// Why the byte stream cannot be followed any further.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desync {
    BadChannel(u8),
    BadFlags(u8),
    /// The announced total of a fragmented message is over the cap.
    TotalTooBig(u32),
    /// A channel's reassembled message grew over the cap.
    Overflow(u8),
}

impl std::fmt::Display for Desync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Desync::BadChannel(c) => write!(f, "frame on channel {c}"),
            Desync::BadFlags(x) => write!(f, "frame flags {x:#04x}"),
            Desync::TotalTooBig(n) => write!(f, "announced message of {n} bytes"),
            Desync::Overflow(c) => write!(f, "message overflow on channel {c}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub channel: u8,
    pub flags: u8,
    pub len: u16,
}

impl Header {
    pub const LEN: usize = 4;

    pub fn parse(bytes: [u8; 4]) -> Result<Self, Desync> {
        let h = Header {
            channel: bytes[0],
            flags: bytes[1],
            len: u16::from_be_bytes([bytes[2], bytes[3]]),
        };
        if usize::from(h.channel) >= CH_COUNT {
            return Err(Desync::BadChannel(h.channel));
        }
        if h.flags & !FRAME_FLAG_MASK != 0 {
            return Err(Desync::BadFlags(h.flags));
        }
        Ok(h)
    }

    pub fn first(&self) -> bool {
        self.flags & FRAME_FIRST != 0
    }

    pub fn last(&self) -> bool {
        self.flags & FRAME_LAST != 0
    }

    pub fn encrypted(&self) -> bool {
        self.flags & FRAME_ENCRYPTED != 0
    }

    /// A first fragment of a longer message carries the message's total
    /// size after the header.
    pub fn has_total(&self) -> bool {
        self.first() && !self.last()
    }
}

/// The u32BE total that follows the header of a first fragment.
pub fn parse_total(bytes: [u8; 4]) -> Result<u32, Desync> {
    let total = u32::from_be_bytes(bytes);
    if total as usize > MAX_MESSAGE_SIZE {
        return Err(Desync::TotalTooBig(total));
    }
    Ok(total)
}

struct Slot {
    assembling: bool,
    buf: Vec<u8>,
}

/// One reassembly buffer per channel, kept across messages.
pub struct Assembler {
    slots: Vec<Slot>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            slots: (0..CH_COUNT)
                .map(|_| Slot {
                    assembling: false,
                    buf: Vec::new(),
                })
                .collect(),
        }
    }

    /// The buffer the frame's payload goes into: cleared on a first
    /// fragment, grown at most to `ASSEMBLY_RESERVE` from the announced
    /// total. `None` for a continuation with no start, which is dropped.
    pub fn target(&mut self, h: &Header, total: u32) -> Option<&mut Vec<u8>> {
        let slot = &mut self.slots[usize::from(h.channel) % CH_COUNT];
        if h.first() {
            slot.buf.clear();
            let want = (total as usize).min(ASSEMBLY_RESERVE);
            if slot.buf.capacity() < want {
                slot.buf.reserve(want - slot.buf.len());
            }
            slot.assembling = true;
        } else if !slot.assembling {
            return None;
        }
        Some(&mut slot.buf)
    }

    /// After the payload was appended: the whole message when this frame
    /// was its last, or nothing yet. Over the cap is a desync.
    pub fn complete(&mut self, h: &Header) -> Result<Option<&[u8]>, Desync> {
        let slot = &mut self.slots[usize::from(h.channel) % CH_COUNT];
        if slot.buf.len() > MAX_MESSAGE_SIZE {
            slot.assembling = false;
            slot.buf.clear();
            return Err(Desync::Overflow(h.channel));
        }
        if !h.last() {
            return Ok(None);
        }
        slot.assembling = false;
        Ok(Some(&slot.buf))
    }

    /// The channel's buffer, holding the message `complete` just returned,
    /// taken out so the caller may use it while it works on the assembler's
    /// owner; `restore` puts it back with its capacity.
    pub fn take_message(&mut self, channel: u8) -> Vec<u8> {
        std::mem::take(&mut self.slots[usize::from(channel) % CH_COUNT].buf)
    }

    pub fn restore(&mut self, channel: u8, buf: Vec<u8>) {
        self.slots[usize::from(channel) % CH_COUNT].buf = buf;
    }

    /// Forget every partial message (a new link).
    pub fn reset(&mut self) {
        for s in &mut self.slots {
            s.assembling = false;
            s.buf.clear();
        }
    }
}

/// Which flags a message goes out with. Plaintext is for the version
/// handshake, the TLS records themselves, the auth-complete and the pings,
/// even after TLS is up; the control class only for channel-open
/// responses; everything else is an encrypted signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Plaintext,
    EncSignal,
    EncControl,
}

impl Class {
    fn flags(self) -> u8 {
        match self {
            Class::Plaintext => 0,
            Class::EncSignal => FRAME_ENCRYPTED,
            Class::EncControl => FRAME_ENCRYPTED | FRAME_CONTROL,
        }
    }

    pub fn encrypted(self) -> bool {
        self != Class::Plaintext
    }
}

/// The frames of one outgoing payload, in order.
pub struct Fragments<'a> {
    channel: u8,
    class: Class,
    payload: &'a [u8],
    offset: usize,
    done: bool,
}

impl<'a> Fragments<'a> {
    pub fn new(channel: u8, class: Class, payload: &'a [u8]) -> Self {
        Self {
            channel,
            class,
            payload,
            offset: 0,
            done: false,
        }
    }

    /// Write the next frame (header, total if any, chunk) into `frame`,
    /// cleared first; false when every frame was written. An empty payload
    /// is one empty frame.
    pub fn next_into(&mut self, frame: &mut Vec<u8>) -> bool {
        if self.done {
            return false;
        }
        let remaining = self.payload.len() - self.offset;
        let chunk = remaining.min(MAX_FRAME_PAYLOAD);
        let first = self.offset == 0;
        let last = self.offset + chunk == self.payload.len();
        let flags = self.class.flags()
            | if first { FRAME_FIRST } else { 0 }
            | if last { FRAME_LAST } else { 0 };
        frame.clear();
        frame.push(self.channel);
        frame.push(flags);
        frame.extend_from_slice(&(chunk as u16).to_be_bytes());
        if first && !last {
            frame.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        }
        frame.extend_from_slice(&self.payload[self.offset..self.offset + chunk]);
        self.offset += chunk;
        self.done = last;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(channel: u8, class: Class, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut f = Fragments::new(channel, class, payload);
        let mut frame = Vec::new();
        while f.next_into(&mut frame) {
            out.push(frame.clone());
        }
        out
    }

    /// Feed a wire stream through the parser and the assembler, as the
    /// process thread does; the messages that came out.
    fn reassemble(stream: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, Desync> {
        let mut a = Assembler::new();
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + Header::LEN <= stream.len() {
            let h = Header::parse(stream[pos..pos + 4].try_into().unwrap())?;
            pos += 4;
            let total = if h.has_total() {
                let t = parse_total(stream[pos..pos + 4].try_into().unwrap())?;
                pos += 4;
                t
            } else {
                0
            };
            let payload = &stream[pos..pos + usize::from(h.len)];
            pos += usize::from(h.len);
            if let Some(buf) = a.target(&h, total) {
                buf.extend_from_slice(payload);
                if let Some(m) = a.complete(&h)? {
                    out.push((h.channel, m.to_vec()));
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn headers_are_checked_before_anything_is_read() {
        let h = Header::parse([3, 0x0b, 0x12, 0x34]).unwrap();
        assert_eq!((h.channel, h.len), (3, 0x1234));
        assert!(h.first() && h.last() && h.encrypted() && !h.has_total());
        assert_eq!(Header::parse([16, 3, 0, 0]), Err(Desync::BadChannel(16)));
        assert_eq!(Header::parse([0, 0x10, 0, 0]), Err(Desync::BadFlags(0x10)));
        assert!(Header::parse([0, 0x09, 0, 0]).unwrap().has_total());
        assert_eq!(parse_total([0, 0x20, 0, 0]).unwrap(), 0x200000);
        assert_eq!(
            parse_total([0, 0x20, 0, 1]),
            Err(Desync::TotalTooBig(0x200001))
        );
    }

    #[test]
    fn a_short_message_is_one_frame_and_a_long_one_carries_its_total() {
        let short = frames(0, Class::Plaintext, &[1, 2, 3]);
        assert_eq!(short, vec![vec![0, 0x03, 0, 3, 1, 2, 3]]);
        let empty = frames(3, Class::EncControl, &[]);
        assert_eq!(empty, vec![vec![3, 0x0f, 0, 0]]);
        let payload: Vec<u8> = (0..MAX_FRAME_PAYLOAD + 1).map(|i| i as u8).collect();
        let long = frames(8, Class::EncSignal, &payload);
        assert_eq!(long.len(), 2);
        assert_eq!(&long[0][..8], &[8, 0x09, 0x40, 0x00, 0, 0, 0x40, 0x01]);
        assert_eq!(long[0].len(), 8 + MAX_FRAME_PAYLOAD);
        assert_eq!(&long[1][..4], &[8, 0x0a, 0, 1]);
        assert_eq!(long[1][4], payload[MAX_FRAME_PAYLOAD]);
        let stream: Vec<u8> = long.concat();
        let got = reassemble(&stream).unwrap();
        assert_eq!(got, vec![(8, payload)]);
    }

    #[test]
    fn a_continuation_without_a_start_is_dropped_not_fatal() {
        let stream = [vec![1, 0x02, 0, 2, 9, 9], vec![1, 0x03, 0, 1, 7]].concat();
        assert_eq!(reassemble(&stream).unwrap(), vec![(1, vec![7])]);
    }

    #[test]
    fn a_message_over_the_cap_ends_the_link() {
        let mut a = Assembler::new();
        let h = Header {
            channel: 3,
            flags: FRAME_FIRST,
            len: 0,
        };
        let buf = a.target(&h, MAX_MESSAGE_SIZE as u32).unwrap();
        assert!(
            buf.capacity() <= ASSEMBLY_RESERVE + 64,
            "the total is not trusted"
        );
        buf.resize(MAX_MESSAGE_SIZE + 1, 0);
        assert_eq!(a.complete(&h), Err(Desync::Overflow(3)));
        // The channel is clean afterwards.
        assert!(
            a.target(
                &Header {
                    flags: FRAME_LAST,
                    ..h
                },
                0
            )
            .is_none()
        );
        a.reset();
    }

    #[test]
    fn frames_interleave_across_channels() {
        let a = frames(3, Class::EncSignal, &vec![1; MAX_FRAME_PAYLOAD * 2]);
        let b = frames(4, Class::EncSignal, &[2, 2]);
        let stream = [a[0].clone(), b[0].clone(), a[1].clone()].concat();
        let got = reassemble(&stream).unwrap();
        assert_eq!(got[0], (4, vec![2, 2]));
        assert_eq!(got[1].0, 3);
        assert_eq!(got[1].1.len(), MAX_FRAME_PAYLOAD * 2);
    }
}
