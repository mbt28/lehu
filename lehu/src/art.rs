// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The album art, decoded off the loop: the session's compressed art goes
//! to a thread of its own, which decodes it to the Media page's size
//! (`lehu_ui::ART_SIZE`) and hands the picture back with a wake. A newer
//! art makes an older one not yet out moot; the thread starts with the
//! first art and then waits on a condvar for as long as the program runs.
//!
//! It runs at nice 10: a decode is a few hundred milliseconds of the one
//! core, and the video, the audio and the screens come first.

use std::sync::{Arc, Condvar, Mutex};

use lehu_core::poll::Wake;
use lehu_ui::{ART_SIZE, Art};

/// What a transparent picture is laid on: the screens' background.
const BACKGROUND: [u8; 3] = [0x10, 0x14, 0x18];

pub struct ArtDecoder {
    inner: Arc<Inner>,
    started: bool,
    /// The last art asked for was none.
    none: bool,
}

struct Inner {
    slot: Mutex<Slot>,
    work: Condvar,
    wake: Arc<Wake>,
}

#[derive(Default)]
struct Slot {
    /// The art to decode next, and the request it answers.
    job: Option<(u32, Vec<u8>)>,
    /// The newest request: an answer to an older one is dropped.
    asked: u32,
    /// The picture for the loop, once decoded.
    done: Option<Art>,
}

impl ArtDecoder {
    pub fn new(wake: Arc<Wake>) -> Self {
        Self {
            inner: Arc::new(Inner {
                slot: Mutex::new(Slot::default()),
                work: Condvar::new(),
                wake,
            }),
            started: false,
            none: true,
        }
    }

    /// Show `data` (JPEG or PNG) as the art; empty for none, which needs
    /// no decoding and is ready at once.
    pub fn submit(&mut self, data: Vec<u8>) {
        if data.is_empty() && self.none {
            return;
        }
        self.none = data.is_empty();
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.asked = slot.asked.wrapping_add(1);
        if data.is_empty() {
            slot.job = None;
            slot.done = Some(Art::default());
            return;
        }
        slot.job = Some((slot.asked, data));
        drop(slot);
        self.inner.work.notify_one();
        if !self.started {
            self.started = true;
            let inner = Arc::clone(&self.inner);
            let spawned = std::thread::Builder::new()
                .name("lehu-art".into())
                .stack_size(256 * 1024)
                .spawn(move || run(&inner));
            if let Err(e) = spawned {
                log::warn!("art: no decoder thread ({e}); no album art");
            }
        }
    }

    /// The newest picture, once, when one is ready.
    pub fn take(&self) -> Option<Art> {
        self.inner
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .done
            .take()
    }
}

fn run(inner: &Inner) {
    // SAFETY: plain syscall on the calling thread (0 = this thread).
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) } != 0 {
        log::debug!("art: the decoder runs at the loop's priority");
    }
    loop {
        let (asked, data) = {
            let mut slot = inner.slot.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = slot.job.take() {
                    break job;
                }
                slot = inner.work.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
        };
        let started = lehu_core::clock::now_ms();
        let art = match lehu_image::decode(&data, ART_SIZE, BACKGROUND) {
            Ok(p) => {
                log::debug!(
                    "art: {} bytes to {}x{} in {} ms",
                    data.len(),
                    p.width,
                    p.height,
                    lehu_core::clock::now_ms().saturating_sub(started)
                );
                Art::from_rgb(p.width, p.height, &p.rgb)
            }
            Err(e) => {
                log::info!(
                    "art: the phone's album art ({} bytes) is {e}; none shown",
                    data.len()
                );
                Art::default()
            }
        };
        drop(data);
        let mut slot = inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.asked == asked {
            slot.done = Some(art);
            drop(slot);
            inner.wake.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(d: &ArtDecoder) -> Art {
        for _ in 0..500 {
            if let Some(a) = d.take() {
                return a;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no art came");
    }

    /// A 2 x 1 PNG, red and blue.
    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let chunk = |out: &mut Vec<u8>, kind: &[u8], body: &[u8]| {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out.extend_from_slice(&[0; 4]); // the CRC is not checked
        };
        chunk(&mut out, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
        // zlib, one stored block: the filter byte and two pixels.
        let raw = [0u8, 255, 0, 0, 0, 0, 255];
        let mut z = vec![
            0x78,
            0x01,
            0x01,
            raw.len() as u8,
            0,
            !(raw.len() as u8),
            0xff,
        ];
        z.extend_from_slice(&raw);
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &raw {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn art_is_decoded_off_the_loop_to_its_size_and_none_is_at_once() {
        let mut d = ArtDecoder::new(Arc::new(Wake::new().unwrap()));
        assert!(d.take().is_none());
        d.submit(Vec::new());
        assert!(d.take().is_none(), "none already: nothing to tell");
        d.submit(png());
        let art = wait_for(&d);
        assert_eq!(art.size(), (ART_SIZE, ART_SIZE / 2));
        // Not a picture: none, said once.
        d.submit(b"not a picture".to_vec());
        assert!(wait_for(&d).is_none());
        d.submit(png());
        let _ = wait_for(&d);
        d.submit(Vec::new());
        assert!(d.take().expect("at once").is_none());
        // A newer art makes the one under way moot.
        d.submit(png());
        d.submit(Vec::new());
        assert!(d.take().expect("none, at once").is_none());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(d.take().is_none(), "the older answer was dropped");
    }
}
