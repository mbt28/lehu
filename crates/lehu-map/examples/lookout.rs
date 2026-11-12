// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A drive replayed without a route, as LEHU watches the road ahead of it
//! for speed cameras: each warning, when it would be said and how far
//! before the camera; and, to set them against, the cameras a route
//! between the drive's ends passes.
//!
//!     lookout REGION_DIR DRIVE.gpx

use std::path::Path;
use std::time::Instant;

use lehu_map::{CameraLookout, Router, Track, camera_lead};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: lookout REGION_DIR DRIVE.gpx");
        std::process::exit(2);
    }
    let fail = |e: String| -> ! {
        eprintln!("{e}");
        std::process::exit(1)
    };
    let router = Router::open(Path::new(&args[0])).unwrap_or_else(|e| fail(e));
    let text = std::fs::read_to_string(&args[1]).unwrap_or_else(|e| fail(e.to_string()));
    let drive = Track::parse_gpx(&text).unwrap_or_else(|e| fail(e));
    let at = |ms: u64| {
        let f = drive.at(ms);
        let (lat, lon) = lehu_map::degrees_from_world(f.x, f.y);
        format!("{:>4} s  {lat:.5}, {lon:.5}", ms / 1000)
    };
    // The route between the drive's ends, and the cameras on it.
    let (first, last) = (drive.at(0), drive.at(drive.duration().saturating_sub(1)));
    match router.route((first.x, first.y), None, (last.x, last.y)) {
        Ok(route) => {
            println!(
                "a route between its ends: {:.1} km, {} cameras on it",
                f64::from(route.metres()) / 1000.0,
                route.cameras().len()
            );
            for c in route.cameras() {
                println!("  at {:.0} m, limit {:?}", c.metres, c.limit);
            }
        }
        Err(e) => println!("no route between its ends: {e}"),
    }
    let mut look = CameraLookout::new();
    let (mut warned, mut worst_us, mut total_us, mut ticks) = (0, 0u128, 0u128, 0u128);
    let mut shown: Option<u32> = None;
    let mut ms = 0;
    // As LEHU does with the map open: every 200 ms.
    while ms <= drive.duration() {
        let fix = drive.at(ms);
        let t = Instant::now();
        let next = look.look(&router, &fix, ms);
        let us = t.elapsed().as_micros();
        (worst_us, total_us, ticks) = (worst_us.max(us), total_us + us, ticks + 1);
        let sign = next.map(|c| c.metres).filter(|&m| m <= 500);
        if sign.is_some() != shown.is_some() {
            match sign {
                Some(m) => println!("{}  sign on: {m} m", at(ms)),
                None => println!("{}  sign off", at(ms)),
            }
        }
        shown = sign;
        if let Some(c) = next
            && look.due(&c, fix.kmh, ms)
        {
            warned += 1;
            println!(
                "{}  SPEED CAMERA AHEAD: {} m, at {} km/h (lead {} m), limit {:?}",
                at(ms),
                c.metres,
                fix.kmh,
                camera_lead(fix.kmh),
                c.limit
            );
        }
        ms += 200;
    }
    println!(
        "{warned} warnings; a look {} us each, {} us at worst",
        total_us / ticks.max(1),
        worst_us
    );
}
