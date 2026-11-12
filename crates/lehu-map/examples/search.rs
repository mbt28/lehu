// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Searches a region's places.bin and times it, as the Go to screen does
//! at every key typed:
//!
//!     search REGION_DIR "LAT, LON" QUERY...
//!
//! Each query is searched as typed, one letter more at a time, and the
//! results of the whole query are printed.

use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: search REGION_DIR \"LAT, LON\" QUERY...");
        std::process::exit(2);
    }
    let t = Instant::now();
    let places = lehu_map::Places::open(Path::new(&args[0])).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    println!("opened {} names in {:.1} ms", places.len(), ms(t));
    let near = lehu_map::parse_position(&args[1]).ok().flatten();
    for query in &args[2..] {
        let mut worst = 0.0f64;
        let mut total = 0.0;
        let mut keys = 0;
        for (i, _) in query.char_indices().skip(1).chain([(query.len(), ' ')]) {
            let t = Instant::now();
            let found = places.search(&query[..i], near, 20);
            let took = ms(t);
            std::hint::black_box(found);
            worst = worst.max(took);
            total += took;
            keys += 1;
        }
        println!(
            "\n{query:?}: {keys} keys, {:.1} ms each, {worst:.1} ms at worst",
            total / f64::from(keys)
        );
        for f in places.search(query, near, 8) {
            let km = f.metres.map_or(String::new(), |m| {
                format!("{:.1} km", f64::from(m) / 1000.0)
            });
            println!("  {:<40} {:<24} {km}", f.name, f.town);
        }
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}
