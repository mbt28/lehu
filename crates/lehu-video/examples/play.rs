// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Plays an Annex-B file on the board's panel the way a phone's stream
//! arrives: one access unit at a time, paced, through the bounded queue and
//! the hardware decoder. For the samples in `sample/`: `test.h264` as it
//! is; an MP4 after `ffmpeg -i in.mp4 -c:v copy -bsf:v h264_mp4toannexb
//! out.h264`.
//!
//!     play FILE [--hevc] [--fps N] [--queue N]
//!
//! Needs the display (DRM master), so it runs on the board, as root or the
//! lehu user. Prints the decoder's statistics lines and a summary.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lehu_core::poll::{Interest, Poller};
use lehu_core::queue::Queue;
use lehu_core::settings::LogLevel;
use lehu_video::{AccessUnit, Codec, Decoder, Options};

/// CarPlay's back-pressure: wait while this many units are queued.
const DEPTH: usize = 4;
const WAIT: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    lehu_core::log::init(LogLevel::Debug);
    let mut path = None;
    let mut hevc = false;
    let mut fps = 30u32;
    let mut queue = 16usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hevc" => hevc = true,
            "--fps" => {
                fps = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30)
                    .max(1)
            }
            "--queue" => {
                queue = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(16)
                    .max(1)
            }
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => return usage(&format!("unexpected {other}")),
        }
    }
    let Some(path) = path else {
        return usage("a file is needed");
    };
    let codec = if hevc { Codec::Hevc } else { Codec::H264 };
    let display = match lehu_drm::Display::open_any() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("display: {e}");
            return ExitCode::FAILURE;
        }
    };
    let units = match lehu_video::annexb::read_units(&path, hevc) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let input: Arc<Queue<AccessUnit>> = Arc::new(Queue::new(queue));
    let decoder = match Decoder::start(
        codec,
        Arc::clone(&display),
        Arc::clone(&input),
        Options {
            low_delay: true,
            fast: true,
            keyframe: Some(Box::new(|| log::info!("play: a keyframe was asked for"))),
            recycle: None,
        },
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decoder: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The feeder plays the phone: paced units, back-pressure, then done.
    let feeder = {
        let input = Arc::clone(&input);
        let interval = Duration::from_micros(1_000_000 / u64::from(fps));
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut n = 0u64;
            for unit in units {
                let due = start + interval * u32::try_from(n).unwrap_or(u32::MAX);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                input.push_wait_drop_oldest(unit, DEPTH, WAIT);
                n += 1;
            }
            n
        })
    };

    // The main loop's part: drain flip events and retry the pending frame.
    let mut poller = Poller::with_capacity(1);
    let drm = poller.add(display.fd().as_raw_fd(), Interest::Read);
    while !feeder.is_finished() || !input.is_empty() || decoder.has_pending() {
        if poller.wait(Some(Duration::from_millis(100))).is_err() {
            break;
        }
        if drm.is_some_and(|i| poller.readable(i)) && display.handle_events() > 0 {
            decoder.on_flip();
        }
        if decoder.failed() {
            break;
        }
    }
    // The last picture stays up a moment before the plane goes.
    std::thread::sleep(Duration::from_millis(500));
    let fed = feeder.join().unwrap_or(0);
    let stats = decoder.stats();
    let failed = decoder.failed();
    decoder.stop();
    println!(
        "played {fed} units: {} in, {} decoded, {} shown, {} superseded, {} dropped{}",
        stats.input,
        stats.decoded,
        stats.shown,
        stats.superseded,
        stats.dropped,
        if failed { " (the decoder gave up)" } else { "" }
    );
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn usage(why: &str) -> ExitCode {
    eprintln!("play: {why}\nusage: play FILE [--hevc] [--fps N] [--queue N]");
    ExitCode::FAILURE
}
