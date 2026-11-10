// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas

use std::path::{Path, PathBuf};

use crate::hw::untile;
use crate::nv12::{self, ycc};
use crate::tiles::{LAND, Tiles};
use crate::tileset::TileSet;
use crate::view::{Layout, TILE, View, WORLD_Z};

const META: &str = r#"
name = "Test"
format = "png"
tile-size = 256
min-zoom = 2
max-zoom = 16
bounds = [100000000, 200000000, 300000000, 400000000]
center = [200000000, 300000000]
start-zoom = 12
attribution = "© OpenStreetMap contributors"
"#;

const BOUNDS: [i64; 4] = [100_000_000, 200_000_000, 300_000_000, 400_000_000];

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lehu-map-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_tile_set_reads_its_description() {
    let set = TileSet::parse(Path::new("/maps"), META).unwrap();
    assert_eq!(set.name, "Test");
    assert_eq!((set.min_zoom, set.max_zoom, set.start_zoom), (2, 16, 12));
    assert_eq!(set.center, (200_000_000, 300_000_000));
    assert_eq!(set.bounds, BOUNDS);
}

#[test]
fn a_bad_description_says_what_is_wrong() {
    let bad = META.replace("tile-size = 256", "tile-size = 512");
    assert!(
        TileSet::parse(Path::new("/m"), &bad)
            .unwrap_err()
            .contains("tile-size")
    );
    let bad = META.replace("max-zoom = 16", "max-zoom = 1");
    assert!(
        TileSet::parse(Path::new("/m"), &bad)
            .unwrap_err()
            .contains("zoom")
    );
    let bad = META.replace("format = \"png\"", "format = \"webp\"");
    assert!(
        TileSet::parse(Path::new("/m"), &bad)
            .unwrap_err()
            .contains("format")
    );
    let bad = META.replace("[100000000, 200000000,", "[300000000, 200000000,");
    assert!(
        TileSet::parse(Path::new("/m"), &bad)
            .unwrap_err()
            .contains("bounds")
    );
    let missing = temp("missing");
    assert!(
        TileSet::open(&missing)
            .unwrap_err()
            .starts_with("No map in")
    );
}

#[test]
fn a_start_zoom_outside_the_set_is_brought_in() {
    let set = TileSet::parse(
        Path::new("/m"),
        &META.replace("start-zoom = 12", "start-zoom = 20"),
    )
    .unwrap();
    assert_eq!(set.start_zoom, 16);
}

#[test]
fn panning_moves_by_pixels_of_the_zoom_shown() {
    let mut v = View {
        zoom: 12,
        x: 200_000_000,
        y: 300_000_000,
    };
    v.pan(10, -4, &BOUNDS);
    let px = 1i64 << (WORLD_Z - 12);
    assert_eq!((v.x, v.y), (200_000_000 + 10 * px, 300_000_000 - 4 * px));
    v.zoom = 16;
    v.pan(1, 0, &BOUNDS);
    assert_eq!(v.x, 200_000_000 + 10 * px + (1 << 6));
}

#[test]
fn panning_stops_at_the_edge_of_the_map() {
    let mut v = View {
        zoom: 8,
        x: 200_000_000,
        y: 300_000_000,
    };
    v.pan(100_000, -100_000, &BOUNDS);
    assert_eq!((v.x, v.y), (BOUNDS[2], BOUNDS[1]));
}

#[test]
fn zoom_stays_in_the_set() {
    let mut v = View {
        zoom: 15,
        x: 0,
        y: 0,
    };
    v.zoom_by(1, 2, 16);
    v.zoom_by(1, 2, 16);
    assert_eq!(v.zoom, 16);
    v.zoom_by(-20, 2, 16);
    assert_eq!(v.zoom, 2);
}

#[test]
fn the_window_starts_in_the_middle_of_its_canvas() {
    let v = View {
        zoom: 12,
        x: 200_000_000,
        y: 300_000_000,
    };
    // Every offset within a tile: the window always fits, within half a
    // tile of the middle (480 and 288 pixels spare).
    for step in 0..256 {
        let mut v = v;
        v.pan(step, step, &BOUNDS);
        let l = Layout::around(&v, (800, 480), 5, 3);
        let (x, y) = l.window(&v, (800, 480)).unwrap();
        assert!(
            (240 - TILE / 2..=240 + TILE / 2).contains(&i64::from(x)),
            "{x}"
        );
        assert!(
            (144 - TILE / 2..=144 + TILE / 2).contains(&i64::from(y)),
            "{y}"
        );
    }
}

#[test]
fn scrolling_keeps_the_canvas_until_the_window_leaves_it() {
    let mut v = View {
        zoom: 12,
        x: 200_000_000,
        y: 300_000_000,
    };
    let panel = (800, 480);
    let l = Layout::around(&v, panel, 5, 3);
    let (x0, _) = l.window(&v, panel).unwrap();
    v.pan(-(x0 as i32), 0, &BOUNDS);
    assert_eq!(l.window(&v, panel).map(|w| w.0), Some(0));
    v.pan(-1, 0, &BOUNDS);
    assert_eq!(l.window(&v, panel), None);
    // Another zoom is never this canvas.
    let mut z = v;
    z.zoom = 13;
    assert_eq!(l.window(&z, panel), None);
}

fn write_png(path: &Path, rgb: [u8; 3]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 256, 256);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().unwrap();
    let data: Vec<u8> = (0..256 * 256).flat_map(|_| rgb).collect();
    w.write_image_data(&data).unwrap();
}

/// The luma and the chroma pair at pixel (`x`, `y`) of an NV12 canvas.
fn nv12_at(canvas: &[u8], pitch: usize, chroma: usize, x: usize, y: usize) -> (u8, u8, u8) {
    let c = chroma + (y / 2) * pitch + (x & !1);
    (canvas[y * pitch + x], canvas[c], canvas[c + 1])
}

#[test]
fn a_canvas_is_composed_from_the_tiles_with_land_where_there_are_none() {
    let dir = temp("compose");
    std::fs::write(dir.join("map.toml"), META).unwrap();
    write_png(&dir.join("3/1/2.png"), [255, 0, 0]);
    write_png(&dir.join("3/2/2.png"), [0, 0, 255]);
    std::fs::create_dir_all(dir.join("3/3")).unwrap();
    std::fs::write(dir.join("3/3/2.png"), b"not a picture").unwrap();
    let set = TileSet::open(&dir).unwrap();
    let mut tiles = Tiles::new(set, None);
    // Columns 0..4 of row 2 at zoom 3: none, red, blue, broken; then
    // the row below, all missing.
    let layout = Layout {
        zoom: 3,
        tx: 0,
        ty: 2,
        cols: 4,
        rows: 2,
    };
    let pitch = 4 * 256 + 64; // a pitch wider than the rows
    let chroma = pitch * 512;
    let mut canvas = vec![0u8; chroma + pitch * 256];
    tiles.compose(&layout, &mut canvas, pitch, chroma);
    let at = |x, y| nv12_at(&canvas, pitch, chroma, x, y);
    assert_eq!(at(10, 10), ycc(LAND));
    assert_eq!(at(256 + 100, 255), ycc([255, 0, 0]));
    assert_eq!(at(512 + 255, 0), ycc([0, 0, 255]));
    assert_eq!(at(768 + 3, 3), ycc(LAND));
    assert_eq!(at(300, 300), ycc(LAND));
    assert_eq!(at(1023, 511), ycc(LAND));
    // The padding past the rows is never written, in either plane.
    assert_eq!(canvas[pitch - 1], 0);
    assert_eq!(canvas[chroma + pitch - 1], 0);
    assert_eq!((tiles.decoded, tiles.software, tiles.failed), (2, 2, 1));
    // Composed again, nothing is decoded or looked for twice.
    tiles.compose(&layout, &mut canvas, pitch, chroma);
    assert_eq!((tiles.decoded, tiles.failed), (2, 1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_engines_tiled_picture_comes_out_linear_in_the_limited_range() {
    // A 64 x 64 picture: each pixel's luma its column + row, its chroma
    // pairs (column, row) of the half-height plane, laid out in 32 x 32
    // tiles the way the engine writes them.
    let side = 64;
    let luma = side * side;
    let mut tiled = vec![0u8; luma * 3 / 2];
    let put = |buf: &mut [u8], base: usize, x: usize, y: usize, v: u8| {
        let at = base + ((y / 32) * (side / 32) + x / 32) * 1024 + (y % 32) * 32 + x % 32;
        buf[at] = v;
    };
    for y in 0..side {
        for x in 0..side {
            put(&mut tiled, 0, x, y, (x + y) as u8);
        }
    }
    for y in 0..side / 2 {
        for x in 0..side {
            put(
                &mut tiled,
                luma,
                x,
                y,
                if x % 2 == 0 { x as u8 } else { 255 - y as u8 },
            );
        }
    }
    let mut out = vec![0u8; luma * 3 / 2];
    untile(&tiled, side, luma, &mut out);
    for (x, y) in [(0, 0), (31, 0), (32, 0), (5, 33), (63, 63)] {
        assert_eq!(
            out[y * side + x],
            nv12::FULL_TO_LIMITED_Y[x + y],
            "luma at {x},{y}"
        );
    }
    for (x, y) in [(0, 0), (33, 0), (40, 31), (62, 31)] {
        let want = if x % 2 == 0 { x as u8 } else { 255 - y as u8 };
        assert_eq!(
            out[luma + y * side + x],
            nv12::FULL_TO_LIMITED_C[usize::from(want)]
        );
    }
    // Full-range black, white and neutral chroma.
    assert_eq!(nv12::FULL_TO_LIMITED_Y[0], 16);
    assert_eq!(nv12::FULL_TO_LIMITED_Y[255], 235);
    assert_eq!(nv12::FULL_TO_LIMITED_C[0], 16);
    assert_eq!(nv12::FULL_TO_LIMITED_C[128], 128);
    assert_eq!(nv12::FULL_TO_LIMITED_C[255], 240);
}

#[test]
fn colours_convert_to_the_limited_range() {
    assert_eq!(ycc([0, 0, 0]), (16, 128, 128));
    assert_eq!(ycc([255, 255, 255]), (235, 128, 128));
    let (_, cb, cr) = ycc([255, 0, 0]);
    assert!(cr > 230 && cb < 110);
}

#[test]
fn tiles_outside_the_world_are_land() {
    let dir = temp("edge");
    std::fs::write(dir.join("map.toml"), META).unwrap();
    let mut tiles = Tiles::new(TileSet::open(&dir).unwrap(), None);
    let layout = Layout {
        zoom: 2,
        tx: -1,
        ty: 3,
        cols: 2,
        rows: 2,
    };
    let pitch = 2 * 256;
    let chroma = pitch * 512;
    let mut canvas = vec![0u8; chroma + pitch * 256];
    tiles.compose(&layout, &mut canvas, pitch, chroma);
    assert_eq!(nv12_at(&canvas, pitch, chroma, 0, 0), ycc(LAND));
    assert_eq!(nv12_at(&canvas, pitch, chroma, 511, 511), ycc(LAND));
    assert_eq!((tiles.decoded, tiles.failed), (0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_canvas_ahead_holds_the_view_where_it_leaves_the_one_shown() {
    let panel = (800, 480);
    let s = WORLD_Z - 12;
    for heading in [
        (-7, 0),
        (5, 0),
        (0, -3),
        (0, 9),
        (-4, -4),
        (6, -2),
        (3, 8),
        (-1, 5),
    ] {
        for start in [(0, 0), (101, 37), (-90, 60)] {
            let mut v = View {
                zoom: 12,
                x: 200_000_000 + (start.0 << s),
                y: 300_000_000 + (start.1 << s),
            };
            let shown = Layout::around(&v, panel, 5, 3);
            let ahead = shown.ahead(&v, heading, panel).unwrap();
            // Walk along the heading until the view leaves the canvas shown:
            // the one ahead holds it there, and for a while past it.
            let mut steps = 0;
            while shown.window(&v, panel).is_some() {
                v.x += heading.0 << s;
                v.y += heading.1 << s;
                steps += 1;
                assert!(steps < 10_000, "never left the canvas");
            }
            for _ in 0..8 {
                assert!(
                    ahead.window(&v, panel).is_some(),
                    "heading {heading:?} from {start:?}: off the canvas ahead"
                );
                v.x += heading.0 << s;
                v.y += heading.1 << s;
            }
        }
    }
    // Standing still, nothing is ahead.
    let v = View {
        zoom: 12,
        x: 200_000_000,
        y: 300_000_000,
    };
    let l = Layout::around(&v, panel, 5, 3);
    assert_eq!(l.ahead(&v, (0, 0), panel), None);
}

#[test]
fn a_position_in_degrees_lands_on_its_tile() {
    // Luxembourg City: tile 16/33883/22335, as scripts/map-tiles.py has it.
    let (x, y) = crate::parse_position("49.6116, 6.1300").unwrap().unwrap();
    let s = WORLD_Z - 16;
    assert_eq!(((x >> s) / TILE, (y >> s) / TILE), (33883, 22335));
    // The equator and the prime meridian: the middle of the world.
    let mid = TILE << (WORLD_Z - 1);
    assert_eq!(crate::world_from_degrees(0.0, 0.0), Some((mid, mid)));
    assert_eq!(crate::parse_position("  "), Ok(None));
    assert!(crate::parse_position("49.6").is_err());
    assert!(crate::parse_position("91, 6").is_err());
    assert!(crate::parse_position("north, east").is_err());
}

#[test]
fn the_dot_is_drawn_round_and_clipped_at_the_canvas_edge() {
    let (w, h, pitch) = (64u32, 48u32, 80usize);
    let chroma = pitch * h as usize;
    let mut canvas = vec![0u8; chroma + pitch * h as usize / 2];
    nv12::dot(&mut canvas, pitch, chroma, (w, h), (20, 20));
    let (dot_y, dot_cb, dot_cr) = ycc([26, 115, 232]);
    let (ring_y, _, _) = ycc([255, 255, 255]);
    assert_eq!(canvas[20 * pitch + 20], dot_y);
    assert_eq!(canvas[20 * pitch + 20 + 11], ring_y);
    assert_eq!(canvas[20 * pitch + 20 + 14], 0, "outside the ring");
    assert_eq!(
        canvas[(20 + 9) * pitch + 20 + 9],
        ring_y,
        "the ring's corner"
    );
    let c = chroma + 10 * pitch + 20;
    assert_eq!((canvas[c], canvas[c + 1]), (dot_cb, dot_cr));
    // Past the padding (a pitch wider than the canvas), nothing is written.
    assert!(
        canvas
            .iter()
            .enumerate()
            .all(|(i, &v)| (i % pitch) < w as usize || v == 0)
    );
    // Half off the canvas, or wholly: no panic, the rest stays.
    nv12::dot(&mut canvas, pitch, chroma, (w, h), (-5, 47));
    nv12::dot(&mut canvas, pitch, chroma, (w, h), (500, -500));
    assert_eq!(canvas[47 * pitch + 2], dot_y);
}

#[test]
fn the_map_zooms_past_the_sets_last_level() {
    let set = TileSet::parse(Path::new("/m"), META).unwrap();
    assert_eq!(set.top_zoom(), 16 + crate::tileset::OVERZOOM);
    let deep = META.replace("max-zoom = 16", "max-zoom = 22");
    assert_eq!(
        TileSet::parse(Path::new("/m"), &deep).unwrap().top_zoom(),
        WORLD_Z
    );
}

#[test]
fn a_missing_tile_is_the_one_a_zoom_out_enlarged() {
    let dir = temp("enlarge");
    std::fs::write(dir.join("map.toml"), META).unwrap();
    // Zoom 3 has tile 1/1, red; zoom 4 has nothing.
    write_png(&dir.join("3/1/1.png"), [255, 0, 0]);
    let mut tiles = Tiles::new(TileSet::open(&dir).unwrap(), None);
    let layout = Layout {
        zoom: 4,
        tx: 2,
        ty: 2,
        cols: 2,
        rows: 2,
    };
    let pitch = 2 * 256;
    let chroma = pitch * 512;
    let mut canvas = vec![0u8; chroma + pitch * 256];
    tiles.compose(&layout, &mut canvas, pitch, chroma);
    // All four quarters of the red tile, each made from it once.
    for (x, y) in [(0, 0), (300, 10), (10, 300), (511, 511)] {
        assert_eq!(
            nv12_at(&canvas, pitch, chroma, x, y),
            ycc([255, 0, 0]),
            "at {x},{y}"
        );
    }
    assert_eq!((tiles.decoded, tiles.enlarged), (1, 4));
    // Five zooms in, too far to enlarge: land.
    let far = Layout {
        zoom: 8,
        tx: 32,
        ty: 32,
        cols: 1,
        rows: 1,
    };
    tiles.compose(&far, &mut canvas, pitch, chroma);
    assert_eq!(nv12_at(&canvas, pitch, chroma, 5, 5), ycc(LAND));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_enlarged_quarter_keeps_its_place_and_meets_its_neighbour() {
    // Luma a ramp across (x), chroma pairs constant: the left quarter at
    // twice the size runs from about 0 to about 127, smoothly.
    let side = 64;
    let mut src = vec![128u8; side * side * 3 / 2];
    for y in 0..side {
        for x in 0..side {
            src[y * side + x] = (x * 4) as u8;
        }
    }
    let mut out = vec![0u8; side * side * 3 / 2];
    nv12::enlarge_quarter(&src, side, (0, 0), &mut out);
    let row = &out[10 * side..11 * side];
    assert!(row[0] <= 4);
    assert!((120..=130).contains(&row[side - 1]), "{}", row[side - 1]);
    assert!(row.windows(2).all(|p| p[1] >= p[0] && p[1] - p[0] <= 4));
    assert!(out[side * side..].iter().all(|&c| c == 128));
}

#[test]
fn enlarging_matches_the_four_sample_rule_everywhere() {
    // Every output sample against 9/3/3/1 sixteenths of its source samples,
    // the edges clamped: all four quarters, luma and chroma, odd values.
    let side = 64;
    let src: Vec<u8> = (0..side * side * 3 / 2)
        .map(|i| (i * 37 % 251) as u8)
        .collect();
    for q in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        let mut out = vec![0u8; side * side * 3 / 2];
        nv12::enlarge_quarter(&src, side, q, &mut out);
        let check = |plane: &[u8], got: &[u8], sw: usize, n: usize, half: usize| {
            let at = |x: usize, y: usize, c: usize| u32::from(plane[(y * sw + x) * n + c]);
            for dy in 0..sw {
                for dx in 0..sw {
                    let (sx, sy) = (q.0 * half + dx / 2, q.1 * half + dy / 2);
                    let nx = if dx % 2 == 0 {
                        sx.saturating_sub(1)
                    } else {
                        (sx + 1).min(sw - 1)
                    };
                    let ny = if dy % 2 == 0 {
                        sy.saturating_sub(1)
                    } else {
                        (sy + 1).min(sw - 1)
                    };
                    for c in 0..n {
                        let v = 9 * at(sx, sy, c)
                            + 3 * at(nx, sy, c)
                            + 3 * at(sx, ny, c)
                            + at(nx, ny, c);
                        assert_eq!(
                            u32::from(got[(dy * sw + dx) * n + c]),
                            (v + 8) / 16,
                            "{q:?} {dx},{dy}"
                        );
                    }
                }
            }
        };
        check(&src[..side * side], &out[..side * side], side, 1, side / 2);
        check(
            &src[side * side..],
            &out[side * side..],
            side / 2,
            2,
            side / 4,
        );
    }
}

#[test]
fn a_gpx_time_is_seconds_since_1970() {
    use crate::track::seconds_for_test as s;
    assert_eq!(s("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(s("2000-03-01T00:00:00Z"), Some(951_868_800));
    assert_eq!(s("2027-01-15T08:00:05Z"), Some(1_800_000_005));
    assert_eq!(s("soon"), None);
}

const GPX: &str = r#"<?xml version="1.0"?>
<gpx version="1.1"><trk><trkseg>
<trkpt lat="49.6000" lon="6.1300"><time>2027-01-15T08:00:00Z</time></trkpt>
<trkpt lon="6.1310" lat="49.6000"><time>2027-01-15T08:00:10Z</time></trkpt>
<trkpt lat='49.6010' lon='6.1310'><time>2027-01-15T08:00:20Z</time></trkpt>
</trkseg></trk></gpx>"#;

#[test]
fn a_drive_is_replayed_along_its_points_in_time() {
    let track = crate::Track::parse_gpx(GPX).unwrap();
    assert_eq!(track.duration(), 20_000);
    let p0 = crate::world_from_degrees(49.6, 6.13).unwrap();
    let p1 = crate::world_from_degrees(49.6, 6.131).unwrap();
    let p2 = crate::world_from_degrees(49.601, 6.131).unwrap();
    let a = track.at(0);
    assert_eq!((a.x, a.y), p0);
    // Halfway along the first step, heading east.
    let b = track.at(5_000);
    assert_eq!((b.x, b.y), ((p0.0 + p1.0) / 2, p0.1));
    assert_eq!(b.heading, (1024, 0));
    // On the second step, heading north (up: y falls).
    let c = track.at(15_000);
    assert_eq!(c.x, p1.0);
    assert!(c.y < p1.1 && c.y > p2.1);
    assert_eq!(c.heading, (0, -1024));
    // Past the end: from the start again.
    assert_eq!(track.at(20_000 + 5_000), b);
}

#[test]
fn a_drive_without_times_goes_a_point_a_second_and_bad_ones_are_refused() {
    let plain = GPX.replace("<time>", "<x>").replace("</time>", "</x>");
    assert_eq!(crate::Track::parse_gpx(&plain).unwrap().duration(), 2_000);
    assert!(crate::Track::parse_gpx("<gpx></gpx>").is_err());
    assert!(crate::Track::parse_gpx(r#"<trkpt lat="1"/><trkpt lat="2"/>"#).is_err());
    assert!(
        crate::Track::parse_gpx(r#"<trkpt lat="95" lon="1"/><trkpt lat="1" lon="1"/>"#).is_err()
    );
}

#[test]
fn the_arrow_points_along_the_heading() {
    let (w, h, pitch) = (64u32, 64u32, 64usize);
    let chroma = pitch * h as usize;
    let mut canvas = vec![0u8; chroma + pitch * h as usize / 2];
    // Heading east: the tip to the right of the middle, the notch behind.
    nv12::arrow(&mut canvas, pitch, chroma, (w, h), (32, 32), (1024, 0));
    let (blue, _, _) = ycc([26, 115, 232]);
    let (white, _, _) = ycc([255, 255, 255]);
    let at = |x: usize, y: usize| canvas[y * pitch + x];
    assert_eq!(at(32 + 12, 32), blue, "near the tip");
    assert_eq!(at(32 + 17, 32), white, "the tip's edge");
    assert_eq!(at(32 - 12, 32), 0, "the notch behind is open");
    assert_eq!(at(32 - 10, 32 - 9), blue, "a wing");
    assert_eq!(at(32 + 10, 32 - 10), 0, "beside the tip");
    // Not moving yet: the dot.
    let mut still = vec![0u8; canvas.len()];
    nv12::arrow(&mut still, pitch, chroma, (w, h), (32, 32), (0, 0));
    assert_eq!(still[32 * pitch + 32], blue);
    assert_eq!(still[32 * pitch + 32 - 12], white, "the dot's ring");
}

#[test]
fn a_markers_patch_comes_back_as_the_map_under_it() {
    let dir = temp("patch");
    std::fs::write(dir.join("map.toml"), META).unwrap();
    write_png(&dir.join("3/1/2.png"), [255, 0, 0]);
    write_png(&dir.join("3/2/2.png"), [0, 0, 255]);
    let mut tiles = Tiles::new(TileSet::open(&dir).unwrap(), None);
    let layout = Layout {
        zoom: 3,
        tx: 0,
        ty: 2,
        cols: 4,
        rows: 2,
    };
    let pitch = 4 * 256 + 32;
    let chroma = pitch * 512;
    let size = (1024, 512);
    let mut clean = vec![0u8; chroma + pitch * 256];
    tiles.compose(&layout, &mut clean, pitch, chroma);
    // Arrows across tile edges, and one half off the canvas.
    for (at, heading) in [
        ((511, 255), (700, -700)),
        ((256, 300), (0, 1024)),
        ((3, 509), (-1024, 0)),
    ] {
        let mut canvas = clean.clone();
        nv12::arrow(&mut canvas, pitch, chroma, size, at, heading);
        assert_ne!(canvas, clean);
        let r = nv12::MARKER_REACH;
        tiles.compose_rect(
            &layout,
            &mut canvas,
            pitch,
            chroma,
            (at.0 - r, at.1 - r, at.0 + r, at.1 + r),
        );
        assert!(canvas == clean, "at {at:?}: the patch left a trace");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_marker_reaches_past_its_patch() {
    let (w, h, pitch) = (128u32, 128u32, 128usize);
    let chroma = pitch * h as usize;
    let r = nv12::MARKER_REACH;
    for step in 0..32 {
        let a = f64::from(step) * std::f64::consts::PI / 16.0;
        let heading = ((a.cos() * 1024.0) as i32, (a.sin() * 1024.0) as i32);
        let mut canvas = vec![0u8; chroma + pitch * h as usize / 2];
        nv12::arrow(&mut canvas, pitch, chroma, (w, h), (64, 64), heading);
        for y in 0..h as i64 {
            for x in 0..w as i64 {
                if canvas[y as usize * pitch + x as usize] != 0 {
                    assert!(
                        (x - 64).abs() < r && (y - 64).abs() < r,
                        "{heading:?} at {x},{y}"
                    );
                }
            }
        }
    }
}
