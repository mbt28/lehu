// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The decode thread: access units off the queue, pictures onto the plane.
//!
//! One decoder per session, built when the phone's video starts and stopped
//! when it ends or the camera needs the memory: its cedrus pool (about 19 of
//! the 24 MB of CMA) exists only while it runs. `stop` joins the thread,
//! which releases every held picture, closes the decoder and drops the
//! display's cached framebuffers, in that order, so the pool goes back to
//! CMA whole; the next `start` allocates it fresh. Nothing here flushes the
//! codec (see the crate's documentation for why that cannot work).

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lehu_core::clock;
use lehu_core::queue::Queue;
use lehu_drm::{Display, Frame};

use crate::annexb;
use crate::ffi::{Raw, Slots};
use crate::present::{Counts, Presenter};
use crate::probe::Codec;
use crate::resync::{End, Gate, Resync};
use crate::stats::Reporter;
use crate::unit::{AccessUnit, PADDING};

/// Refusals in a row before the decoder gives up: about a second of frames,
/// long enough to ride out a corrupt packet or a start before the first
/// keyframe, short enough to catch a stream the hardware cannot handle (the
/// kernel accepts the format and still has no block for the profile).
const MAX_CONSECUTIVE_FAILURES: u32 = 30;
/// Picture-less units (parameter sets) held for the next picture; a stream
/// that never sends one cannot grow it past this.
const MAX_PREFIX_BYTES: usize = 64 * 1024;
/// What a unit keeps resident while it waits to be reused: an ordinary
/// frame's worth; the rest of a keyframe's pages go back.
const KEEP_RESIDENT: usize = 64 * 1024;
/// With nothing pending the thread sleeps on the queue this long at a time,
/// so a stop is noticed even if the close were missed.
const IDLE_WAIT: Duration = Duration::from_secs(1);
/// With a picture pending the thread wakes this often to offer it again,
/// a safety net under the main loop's `on_flip`: a flip event the loop was
/// late to drain must not leave the last frame of a burst stuck.
const PENDING_WAIT: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum Error {
    /// The board has no hardware decoder for the codec.
    NoHardware(Codec),
    /// ffmpeg could not open the decoder; the text is its message.
    Open(Codec, String),
    Spawn(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoHardware(c) => write!(f, "no hardware decoder for {c}"),
            Error::Open(c, why) => write!(f, "cannot open the {c} decoder: {why}"),
            Error::Spawn(e) => write!(f, "cannot start the decode thread: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// A request for a keyframe, answered by the protocol (CarPlay's
/// forceKeyFrame). Called on the decode thread; it must only queue a
/// command.
pub type KeyframeRequest = Box<dyn Fn() + Send + Sync>;

/// How the decoder is set up.
#[derive(Default)]
pub struct Options {
    /// `AV_CODEC_FLAG_LOW_DELAY` (`decode.low-delay`).
    pub low_delay: bool,
    /// `AV_CODEC_FLAG2_FAST` (`decode.fast`).
    pub fast: bool,
    /// How to ask the phone for a keyframe. With it, a dropped frame holds
    /// the picture until a keyframe or 300 ms; without it the decoder
    /// decodes through the gap (Android Auto restarts its stream instead).
    pub keyframe: Option<KeyframeRequest>,
    /// Where consumed units go back to, cleared, for the protocol to fill
    /// again; without it they are freed. Full: the unit is freed.
    pub recycle: Option<Arc<Queue<AccessUnit>>>,
}

/// Counts since `start`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub input: u32,
    pub decoded: u32,
    pub shown: u32,
    pub superseded: u32,
    /// Units the input queue dropped while this decoder ran.
    pub dropped: u32,
}

struct Shared {
    display: Arc<Display>,
    input: Arc<Queue<AccessUnit>>,
    presenter: Mutex<Presenter>,
    counts: Counts,
    active: AtomicBool,
    failed: AtomicBool,
    input_count: AtomicU32,
    /// Bytes of the units taken, wrapping: the video's bitrate.
    input_bytes: AtomicU32,
    decoded: AtomicU32,
    dropped: AtomicU32,
}

impl Shared {
    fn presenter(&self) -> MutexGuard<'_, Presenter> {
        self.presenter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A running decoder. Drop or `stop` it to end the session's video.
pub struct Decoder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Decoder {
    /// Open the hardware decoder for `codec` and start the thread that feeds
    /// it from `input` and shows its pictures on `display`. Fails when there
    /// is no hardware for the codec or ffmpeg cannot open it; nothing is
    /// left behind then.
    pub fn start(
        codec: Codec,
        display: Arc<Display>,
        input: Arc<Queue<AccessUnit>>,
        options: Options,
    ) -> Result<Decoder, Error> {
        if !crate::probe::hw_available(codec) {
            return Err(Error::NoHardware(codec));
        }
        let raw = Raw::open(codec.is_hevc(), options.low_delay, options.fast)
            .map_err(|why| Error::Open(codec, why))?;
        log::info!(
            "video: {} on v4l2-request ready{}",
            raw.name(),
            if options.keyframe.is_some() {
                ", keyframes on request"
            } else {
                ""
            }
        );
        input.reopen();
        let shared = Arc::new(Shared {
            display,
            input,
            presenter: Mutex::new(Presenter::new()),
            counts: Counts::default(),
            active: AtomicBool::new(true),
            failed: AtomicBool::new(false),
            input_count: AtomicU32::new(0),
            input_bytes: AtomicU32::new(0),
            decoded: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        });
        let worker = Worker {
            shared: Arc::clone(&shared),
            raw,
            hevc: codec.is_hevc(),
            keyframe: options.keyframe,
            recycle: options.recycle,
            // Counted from here, so frames dropped before the first pop
            // (the queue fills while the decoder opens) also resync.
            seen_drops: shared.input.drops(),
            prefix: Vec::with_capacity(MAX_PREFIX_BYTES + PADDING),
            resync: Resync::default(),
            reporter: Reporter::new(clock::now_ms()),
            shown_at_line: 0,
            superseded_at_line: 0,
            failures: 0,
        };
        let thread = std::thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || worker.run())
            .map_err(|e| {
                // The worker (and its decoder) died with the closure.
                shared.display.flush_video();
                Error::Spawn(e)
            })?;
        Ok(Decoder {
            shared,
            thread: Some(thread),
        })
    }

    /// Stop the thread and give the pool back to CMA. Returns once the
    /// thread has joined: the display's cache is flushed by then.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// A flip completed (the main loop drained `Display::handle_events`):
    /// the moment to offer a pending picture again. Cheap when nothing is
    /// pending.
    pub fn on_flip(&self) {
        let mut p = self.shared.presenter();
        if p.has_pending() {
            p.retry(&*self.shared.display, None, &self.shared.counts);
        }
    }

    /// Whether a picture waits for the plane.
    pub fn has_pending(&self) -> bool {
        self.shared.presenter().has_pending()
    }

    /// The decoder gave up: it refused a run of packets. Nothing will show
    /// until it is stopped and started again (with another codec, say).
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }

    /// The thread is still consuming the queue. False once it gave up
    /// (`failed`) or found the queue closed by someone else; `stop` is still
    /// what returns the pool.
    pub fn running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn stats(&self) -> Stats {
        let s = &self.shared;
        Stats {
            input: s.input_count.load(Ordering::Relaxed),
            decoded: s.decoded.load(Ordering::Relaxed),
            shown: s.counts.shown.load(Ordering::Relaxed),
            superseded: s.counts.superseded.load(Ordering::Relaxed),
            dropped: s.dropped.load(Ordering::Relaxed),
        }
    }

    /// Bytes of video taken so far, wrapping.
    pub fn input_bytes(&self) -> u32 {
        self.shared.input_bytes.load(Ordering::Relaxed)
    }

    fn shutdown(&mut self) {
        self.shared.active.store(false, Ordering::Release);
        // Wakes the thread out of its wait; pushes still land meanwhile.
        self.shared.input.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The decode thread's own state.
struct Worker {
    shared: Arc<Shared>,
    raw: Raw,
    hevc: bool,
    keyframe: Option<KeyframeRequest>,
    recycle: Option<Arc<Queue<AccessUnit>>>,
    seen_drops: u32,
    /// Parameter sets waiting to go in front of the next picture, then that
    /// picture behind them: one access unit, as the stream would carry it.
    prefix: Vec<u8>,
    resync: Resync,
    reporter: Reporter,
    /// The shared counters as of the last stats line: the window shows the
    /// difference, since the presenter counts on another thread.
    shown_at_line: u32,
    superseded_at_line: u32,
    failures: u32,
}

impl Worker {
    fn active(&self) -> bool {
        self.shared.active.load(Ordering::Acquire)
    }

    fn run(mut self) {
        self.run_loop();
        // Teardown, in this order: the held pictures first, so the context
        // can free its pool; then the display's framebuffers, whose GEM
        // handles would otherwise keep the freed pool's memory pinned (a
        // CMA leak per rebuild, and this board's CMA fits one pool).
        {
            let mut p = self.shared.presenter();
            p.release_all(&mut Releaser(&mut self.raw));
        }
        let Worker { shared, raw, .. } = self;
        raw.close();
        shared.display.flush_video();
        // Nothing consumes the queue any more: the protocol must not wait on it.
        shared.input.close();
        let stats = Stats {
            input: shared.input_count.load(Ordering::Relaxed),
            decoded: shared.decoded.load(Ordering::Relaxed),
            shown: shared.counts.shown.load(Ordering::Relaxed),
            superseded: shared.counts.superseded.load(Ordering::Relaxed),
            dropped: shared.dropped.load(Ordering::Relaxed),
        };
        log::info!(
            "video: decoder stopped after {} in, {} decoded, {} shown",
            stats.input,
            stats.decoded,
            stats.shown
        );
    }

    fn run_loop(&mut self) {
        loop {
            let pending = self.shared.presenter().has_pending();
            let wait = if pending { PENDING_WAIT } else { IDLE_WAIT };
            let unit = self.shared.input.pop_wait(wait);
            if !self.active() {
                if let Some(u) = unit {
                    self.recycle(u);
                }
                return;
            }
            // Closed by someone else (the session ended under us): nothing
            // more will come, and waiting on a closed queue would spin.
            if unit.is_none() && self.shared.input.is_closed() {
                log::info!("video: the input queue was closed; stopping the decoder");
                return;
            }
            {
                let mut p = self.shared.presenter();
                let mut slots = Releaser(&mut self.raw);
                if pending {
                    p.retry(&*self.shared.display, Some(&mut slots), &self.shared.counts);
                }
                // Slots a main-thread retry freed go back now.
                p.settle(&mut slots);
            }
            let shown = self.shared.counts.shown.load(Ordering::Relaxed);
            let superseded = self.shared.counts.superseded.load(Ordering::Relaxed);
            self.reporter.window.shown = shown.wrapping_sub(self.shown_at_line);
            self.reporter.window.superseded = superseded.wrapping_sub(self.superseded_at_line);
            if let Some(line) = self.reporter.tick(clock::now_ms()) {
                self.shown_at_line = shown;
                self.superseded_at_line = superseded;
                log::info!("{line}");
            }
            let Some(unit) = unit else { continue };
            // The one just taken counts towards the depth seen.
            let depth = self.shared.input.len().saturating_add(1);
            self.reporter.window.depth_max = self.reporter.window.depth_max.max(depth as u32);
            self.reporter.window.input += 1;
            self.shared.input_count.fetch_add(1, Ordering::Relaxed);
            self.shared
                .input_bytes
                .fetch_add(unit.len() as u32, Ordering::Relaxed);
            let gave_up = !self.handle(unit);
            if gave_up {
                self.shared.failed.store(true, Ordering::Relaxed);
                return;
            }
        }
    }

    /// One unit. False when the decoder gave up.
    fn handle(&mut self, mut unit: AccessUnit) -> bool {
        let now = clock::now_ms();
        let drops = self.shared.input.drops();
        if drops != self.seen_drops {
            let n = drops.wrapping_sub(self.seen_drops);
            self.seen_drops = drops;
            self.reporter.window.dropped += n;
            self.shared.dropped.fetch_add(n, Ordering::Relaxed);
            if let Some(ask) = &self.keyframe {
                let was = self.resync.active();
                if self.resync.on_drop(now) {
                    ask();
                }
                if !was {
                    log::warn!(
                        "video: fell behind ({n} dropped); holding the picture until a keyframe"
                    );
                }
            }
        }
        if self.resync.active() {
            let mut kinds = annexb::kinds(unit.data(), self.hevc);
            kinds.keyframe |= unit.keyframe;
            kinds.params |= unit.config;
            let (gate, end) = self.resync.gate(now, kinds);
            match end {
                Some(End::Keyframe { waited_ms }) => {
                    log::info!("video: resynced at a keyframe after {waited_ms} ms");
                }
                Some(End::Timeout { waited_ms }) => {
                    log::warn!(
                        "video: no keyframe within {waited_ms} ms; decoding through the gap"
                    );
                }
                None => {}
            }
            if gate == Gate::Skip {
                self.recycle(unit);
                return true;
            }
        }

        // Parameter sets alone hold no picture, which the parser and the
        // decoder both reject ("missing picture in access unit"): kept and
        // fed in front of the next picture.
        if !annexb::has_picture(unit.data(), self.hevc) {
            if self.prefix.len() + unit.len() <= MAX_PREFIX_BYTES {
                self.prefix.extend_from_slice(unit.data());
            }
            self.recycle(unit);
            return true;
        }

        let ok = if self.prefix.is_empty() {
            let data = unit.padded();
            self.decode(data)
        } else {
            // Grows only for a picture larger than the room reserved, once.
            self.prefix.extend_from_slice(unit.data());
            let len = self.prefix.len();
            self.prefix.resize(len + PADDING, 0);
            let prefix = std::mem::take(&mut self.prefix);
            let ok = self.decode(&prefix[..len]);
            self.prefix = prefix;
            self.prefix.clear();
            ok
        };
        self.recycle(unit);
        ok
    }

    /// Feed one padded access unit. False when the decoder gave up.
    fn decode(&mut self, data: &[u8]) -> bool {
        let shared = Arc::clone(&self.shared);
        let started = Instant::now();
        let mut window = self.reporter.window;
        let mut on_picture = |frame: &Frame, slots: &mut dyn Slots| {
            let us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
            window.decoded(us);
            shared.decoded.fetch_add(1, Ordering::Relaxed);
            shared
                .presenter()
                .present(*frame, &*shared.display, slots, &shared.counts);
        };
        let result = self.raw.push(data, &mut on_picture);
        self.reporter.window = window;
        match result {
            Ok(_) => {
                self.failures = 0;
                true
            }
            Err(code) => {
                self.failures += 1;
                // A run of refusals is a stream without a keyframe (the
                // phone carried its encoder on from an earlier session):
                // ask for one, and again while the run lasts.
                if let Some(ask) = &self.keyframe
                    && (self.failures == 3 || self.failures.is_multiple_of(10))
                {
                    ask();
                }
                if self.failures == 1 || self.failures.is_multiple_of(10) {
                    log::warn!(
                        "video: the decoder refused {} units in a row ({})",
                        self.failures,
                        Raw::error_text(code)
                    );
                }
                if self.failures >= MAX_CONSECUTIVE_FAILURES {
                    log::error!(
                        "video: {} consecutive decode failures; giving up on this decoder",
                        self.failures
                    );
                    return false;
                }
                true
            }
        }
    }

    fn recycle(&self, mut unit: AccessUnit) {
        if let Some(q) = &self.recycle {
            unit.clear();
            // A unit waiting to be reused keeps the pages of an ordinary
            // frame, not those of the biggest keyframe it carried.
            unit.release_pages(KEEP_RESIDENT);
            q.push_discard(unit);
        }
    }
}

/// The slots outside a picture callback: only releases make sense.
struct Releaser<'a>(&'a mut Raw);

impl Slots for Releaser<'_> {
    fn hold(&mut self, _slot: usize) {}
    fn release(&mut self, slot: usize) {
        self.0.release(slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_happened() {
        assert_eq!(
            Error::NoHardware(Codec::Hevc).to_string(),
            "no hardware decoder for hevc"
        );
        assert!(
            Error::Open(Codec::H264, "x".into())
                .to_string()
                .contains("h264")
        );
    }

    #[test]
    fn a_decoder_is_sent_between_threads() {
        fn sendable<T: Send>() {}
        sendable::<Decoder>();
    }

    #[test]
    fn options_default_to_no_callbacks() {
        let o = Options::default();
        assert!(o.keyframe.is_none() && o.recycle.is_none());
        assert!(!o.low_delay && !o.fast);
    }
}
