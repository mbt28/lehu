// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A drive replayed against a route to a destination, faster than real
//! time, as LEHU follows it: each second of the drive is placed on the
//! route, and when the drive leaves it the route is found anew from there.
//!
//!     follow REGION_DIR DRIVE.gpx "LAT, LON" [--shortest] [--quiet]
//!
//! The drive is a GPX track (scripts/route-gpx.py writes one, `--via` a
//! point to take another road than the fastest); the destination where
//! the route goes. Each new route, the turns announced and the arrival are
//! printed, with the time a route took to find.

use std::path::Path;
use std::time::Instant;

use lehu_map::{Follow, Following, RouteBy, RouteOptions, Router, Track};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: follow REGION_DIR DRIVE.gpx \"LAT, LON\" [--shortest] [--quiet]");
        std::process::exit(2);
    }
    let fail = |e: String| -> ! {
        eprintln!("{e}");
        std::process::exit(1)
    };
    let router = Router::open(Path::new(&args[0])).unwrap_or_else(|e| fail(e));
    let text = std::fs::read_to_string(&args[1]).unwrap_or_else(|e| fail(e.to_string()));
    let drive = Track::parse_gpx(&text).unwrap_or_else(|e| fail(e));
    let to = lehu_map::parse_position(&args[2])
        .ok()
        .flatten()
        .unwrap_or_else(|| fail("the destination: LAT, LON".into()));
    let options = RouteOptions {
        by: if args.iter().any(|a| a == "--shortest") {
            RouteBy::Shortest
        } else {
            RouteBy::Fastest
        },
        ..RouteOptions::default()
    };
    let quiet = args.iter().any(|a| a == "--quiet");
    // --trace N: each of the first N seconds, how far off the route.
    let trace: u64 = args
        .iter()
        .position(|a| a == "--trace")
        .and_then(|i| args.get(i + 1)?.parse().ok())
        .unwrap_or(0);
    let at = |ms: u64| {
        let (lat, lon) = lehu_map::degrees_from_world(drive.at(ms).x, drive.at(ms).y);
        format!("{:>4} s  {lat:.5}, {lon:.5}", ms / 1000)
    };
    let find = |ms: u64| -> Track {
        let fix = drive.at(ms);
        let heading = (fix.kmh > 0 && fix.heading != (0, 0)).then_some(fix.heading);
        let t = Instant::now();
        let route = router
            .route_with((fix.x, fix.y), heading, to, &options)
            .unwrap_or_else(|e| fail(format!("{}: no route: {e}", at(ms))));
        println!(
            "{}  route: {:.1} km, {} min, {} turns, found in {} ms",
            at(ms),
            f64::from(route.metres()) / 1000.0,
            route.duration().div_ceil(60_000),
            route.maneuvers().len(),
            t.elapsed().as_millis()
        );
        route
    };
    let mut route = find(0);
    let mut follow = Follow::to(to);
    let mut reroutes = 0;
    let mut said = String::new();
    let mut arrived = None;
    let mut worst_off = 0.0f64;
    let end = drive.duration();
    let mut ms = 0;
    while ms <= end {
        let fix = drive.at(ms);
        let state = follow.update(&route, &fix, ms);
        if ms / 1000 < trace {
            println!(
                "{}  {state:?}, {:.0} m off, {} km/h, {:.0} s into the route",
                at(ms),
                follow.off_m(),
                fix.kmh,
                follow.ms() as f64 / 1000.0
            );
        }
        match state {
            Following::Off => {
                println!(
                    "{}  OFF the route ({:.0} m off at {} km/h): finding it anew",
                    at(ms),
                    follow.off_m(),
                    fix.kmh
                );
                reroutes += 1;
                route = find(ms);
                follow = Follow::to(to);
            }
            Following::Arrived => {
                arrived = Some(ms);
                break;
            }
            Following::On => {
                if follow.off_m().is_finite() && follow.off_m() <= lehu_map::OFF_M {
                    worst_off = worst_off.max(follow.off_m());
                }
                if let Some(a) = route.ahead(follow.ms()) {
                    let next = format!("{:?} {}", a.maneuver.turn, a.maneuver.road);
                    if !quiet && next != said {
                        println!(
                            "{}  next: {next} in {} m, {:.1} km left",
                            at(ms),
                            a.metres,
                            f64::from(a.left_metres) / 1000.0
                        );
                        said = next;
                    }
                }
            }
        }
        ms += 1000;
    }
    match arrived {
        Some(ms) => println!("{}  ARRIVED", at(ms)),
        None => println!("the drive ended {:.0} m from the route's end", {
            let fix = drive.at(end);
            route
                .locate((fix.x, fix.y), None, 1, route.len())
                .map_or(f64::NAN, |l| f64::from(route.metres()) - l.metres)
        }),
    }
    println!(
        "{reroutes} reroutes; on the route, at most {worst_off:.0} m off it; the drive {} s",
        end / 1000
    );
}
