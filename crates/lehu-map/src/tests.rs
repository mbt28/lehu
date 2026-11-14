// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas

use std::path::{Path, PathBuf};

use crate::hw::untile;
use crate::nv12::{self, ycc};
use crate::tiles::{LAND, Tiles};
use crate::tileset::TileSet;
use crate::view::{Layout, TILE, View, WORLD_Z};

pub(crate) const META: &str = r#"
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

pub(crate) fn temp(name: &str) -> PathBuf {
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
fn a_route_is_fitted_above_what_covers_the_map() {
    // Two points 1000 x 200 pixels apart at zoom 14 (a pixel there is 256
    // world pixels), on an 800 x 480 panel with a bar of 100 at its foot.
    let s = 1i64 << (WORLD_Z - 14);
    let points = [
        (1_000_000, 2_000_000),
        (1_000_000 + 1000 * s, 2_000_000 + 200 * s),
    ];
    let v = View::fitting(&points, (800, 480), [20, 20, 20, 100], 8, 16).unwrap();
    // 1000 does not fit 760 at 14; at 13 it is 500, which does.
    assert_eq!(v.zoom, 13);
    // The middle of the points is in the middle of the panel above the
    // bar: 40 pixels of zoom 13 above the panel's middle.
    let mid = (1_000_000 + 500 * s, 2_000_000 + 100 * s);
    assert_eq!(v.x, mid.0);
    assert_eq!(v.y, mid.1 + (40 << (WORLD_Z - 13)));
    // A short route: as close as the set goes.
    let v = View::fitting(&points[..1], (800, 480), [0; 4], 8, 16).unwrap();
    assert_eq!((v.zoom, v.x, v.y), (16, 1_000_000, 2_000_000));
    // Too long for any zoom: the set's first.
    let far = [(0, 0), (1 << 40, 0)];
    assert_eq!(
        View::fitting(&far, (800, 480), [0; 4], 8, 16).unwrap().zoom,
        8
    );
    assert!(View::fitting(&[], (800, 480), [0; 4], 8, 16).is_none());
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

pub(crate) fn write_png(path: &Path, rgb: [u8; 3]) {
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
pub(crate) fn nv12_at(
    canvas: &[u8],
    pitch: usize,
    chroma: usize,
    x: usize,
    y: usize,
) -> (u8, u8, u8) {
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
    let mut tiles = Tiles::new(set, None, None);
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
    let mut tiles = Tiles::new(TileSet::open(&dir).unwrap(), None, None);
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
    let mut tiles = Tiles::new(TileSet::open(&dir).unwrap(), None, None);
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
    let mut tiles = Tiles::new(TileSet::open(&dir).unwrap(), None, None);
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

const GUIDED: &str = r#"<gpx>
<wpt lat="49.6000" lon="6.1310"><name>Rue &amp; Co</name><type>left</type></wpt>
<wpt lat="49.6010" lon="6.1310"><name></name><type>arrive</type></wpt>
<trk><trkseg>
<trkpt lat="49.6000" lon="6.1300"><time>2027-01-15T08:00:00Z</time></trkpt>
<trkpt lat="49.6000" lon="6.1310"><time>2027-01-15T08:00:10Z</time></trkpt>
<trkpt lat="49.6010" lon="6.1310"><time>2027-01-15T08:00:20Z</time></trkpt>
</trkseg></trk></gpx>"#;

#[test]
fn a_drive_with_turns_says_the_next_one_and_how_far() {
    use crate::Turn;
    let track = crate::Track::parse_gpx(GUIDED).unwrap();
    let turns: Vec<_> = track
        .maneuvers()
        .iter()
        .map(|m| (m.turn, m.road.as_str()))
        .collect();
    assert_eq!(turns, [(Turn::Left, "Rue & Co"), (Turn::Arrive, "")]);
    // 0.001 degrees of longitude at 49.6 N is about 72 m; of latitude 111 m.
    let a = track.ahead(0).unwrap();
    assert_eq!(a.maneuver.turn, Turn::Left);
    assert!((70..=74).contains(&a.metres), "{}", a.metres);
    assert!((180..=186).contains(&a.left_metres), "{}", a.left_metres);
    assert_eq!(a.left_ms, 20_000);
    let b = track.ahead(5_000).unwrap();
    assert!((35..=37).contains(&b.metres), "{}", b.metres);
    // Past the turn: the arrival is next.
    let c = track.ahead(15_000).unwrap();
    assert_eq!(c.maneuver.turn, Turn::Arrive);
    assert!((54..=57).contains(&c.metres), "{}", c.metres);
    // A track without waypoints guides nowhere.
    assert!(crate::Track::parse_gpx(GPX).unwrap().ahead(0).is_none());
}

#[test]
fn a_fix_says_its_step_its_speed_and_the_roads_limit() {
    let gpx = GUIDED.replace(
        "<time>2027-01-15T08:00:00Z</time>",
        "<time>2027-01-15T08:00:00Z</time><extensions><maxspeed>30</maxspeed></extensions>",
    );
    let track = crate::Track::parse_gpx(&gpx).unwrap();
    // On the first step: 72 m in 10 s is 26 km/h, on a 30 road.
    let a = track.at(5_000);
    assert_eq!(a.step, 1);
    assert!((25..=27).contains(&a.kmh), "{}", a.kmh);
    assert_eq!(a.limit, Some(30));
    // The second: 111 m in 10 s, 40 km/h, no limit given.
    let b = track.at(15_000);
    assert_eq!(b.step, 2);
    assert!((39..=41).contains(&b.kmh), "{}", b.kmh);
    assert_eq!(b.limit, None);
}

#[test]
fn the_route_line_has_its_width_and_keeps_to_its_clip() {
    let (w, h, pitch) = (64u32, 32u32, 64usize);
    let chroma = pitch * h as usize;
    let mut canvas = vec![0u8; chroma + pitch * h as usize / 2];
    // Across the canvas at y = 16, in sixteenths.
    let line = [(0, 16 * 16), (64 * 16, 16 * 16)];
    nv12::route_by(&mut canvas, pitch, chroma, (w, h), (0, 0, 32, 32), &line, 1);
    let (fill, _, _) = ycc([66, 133, 244]);
    let (edge, _, _) = ycc([24, 82, 190]);
    let at = |x: usize, y: usize| canvas[y * pitch + x];
    assert_eq!(at(10, 16), fill);
    assert_eq!(at(10, 13), fill);
    assert_eq!(at(10, 11), edge);
    assert_eq!(at(10, 8), 0, "past the edge");
    assert_eq!(at(40, 16), 0, "outside the clip");
}

/// Four 256 tiles in a 2 x 2 grid whose luma is (x + 3y) mod 251 of the
/// grid pixel, the chroma pairs (tile number, 128).
fn pattern_tiles() -> Vec<Vec<u8>> {
    (0..4)
        .map(|t| {
            let (ox, oy) = ((t % 2) * 256, (t / 2) * 256);
            let mut tile = vec![0u8; 256 * 256 * 3 / 2];
            for y in 0..256 {
                for x in 0..256 {
                    tile[y * 256 + x] = ((ox + x + 3 * (oy + y)) % 251) as u8;
                }
            }
            for pair in tile[256 * 256..].as_chunks_mut::<2>().0 {
                pair.copy_from_slice(&[t as u8 * 10, 128]);
            }
            tile
        })
        .collect()
}

fn turned(up: (i64, i64)) -> Vec<u8> {
    let tiles = pattern_tiles();
    let grid = nv12::Grid {
        tiles: tiles.iter().map(|t| Some(t.as_slice())).collect(),
        cols: 2,
        rows: 2,
        side: 256,
        land: (1, 2, 3),
    };
    let (w, h, pitch) = (64u32, 32u32, 64usize);
    let chroma = pitch * h as usize;
    let mut out = vec![0u8; chroma + pitch * h as usize / 2];
    let across = (-up.1, up.0);
    let down = (-up.0, -up.1);
    nv12::rotate(
        &grid,
        &mut out,
        pitch,
        chroma,
        (w, h),
        (300 << 16, 200 << 16),
        (32, 16),
        across,
        down,
    );
    out
}

#[test]
fn heading_north_the_frame_is_the_map_as_it_lies() {
    let out = turned((0, -65536));
    let luma = |gx: i64, gy: i64| ((gx + 3 * gy) % 251) as u8;
    for (u, v) in [(0, 0), (32, 16), (63, 31), (10, 25)] {
        assert_eq!(
            out[v * 64 + u],
            luma(300 + u as i64 - 32, 200 + v as i64 - 16),
            "at {u},{v}"
        );
    }
    // Grid x 268..331, y 184..215: all in tile 1, whose chroma is 10.
    assert_eq!(out[64 * 32], 10);
    assert_eq!(out[64 * 32 + 62], 10);
}

#[test]
fn heading_east_right_on_the_panel_is_south() {
    let out = turned((65536, 0));
    let luma = |gx: i64, gy: i64| ((gx + 3 * gy) % 251) as u8;
    // Up the panel is east: (u, v) is (300 - (v - 16), 200 + (u - 32)).
    for (u, v) in [(0, 0), (32, 16), (63, 31), (10, 25)] {
        let (gx, gy) = (300 - (v as i64 - 16), 200 + (u as i64 - 32));
        assert_eq!(out[v * 64 + u], luma(gx, gy), "at {u},{v}");
    }
}

#[test]
fn off_the_grid_the_frame_is_land() {
    let tiles = pattern_tiles();
    let grid = nv12::Grid {
        tiles: vec![Some(tiles[0].as_slice()), None, None, None],
        cols: 2,
        rows: 2,
        side: 256,
        land: (7, 8, 9),
    };
    let mut out = vec![0u8; 64 * 32 * 3 / 2];
    // Around grid (256, 256): three quarters missing tiles, and beyond.
    nv12::rotate(
        &grid,
        &mut out,
        64,
        64 * 32,
        (64, 32),
        (256 << 16, 256 << 16),
        (32, 16),
        (65536, 0),
        (0, 65536),
    );
    assert_eq!(out[16 * 64 + 40], 7, "a missing tile");
    let mut far = vec![0u8; out.len()];
    nv12::rotate(
        &grid,
        &mut far,
        64,
        64 * 32,
        (64, 32),
        (-5000 << 16, 0),
        (32, 16),
        (65536, 0),
        (0, 65536),
    );
    assert!(far[..64 * 32].iter().all(|&v| v == 7));
    assert!(far[64 * 32..].chunks(2).all(|p| p == [8, 9]));
}

#[test]
fn at_any_angle_every_pixel_is_the_one_under_it() {
    // Chroma that differs at every pair, and a missing tile.
    let mut tiles = pattern_tiles();
    for (t, tile) in tiles.iter_mut().enumerate() {
        for (i, pair) in tile[256 * 256..]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .enumerate()
        {
            *pair = [(i % 253) as u8, (i / 128 + t * 37) as u8];
        }
    }
    let grid = nv12::Grid {
        tiles: vec![
            Some(&tiles[0][..]),
            Some(&tiles[1][..]),
            None,
            Some(&tiles[3][..]),
        ],
        cols: 2,
        rows: 2,
        side: 256,
        land: (7, 8, 9),
    };
    let sample = |gx: i64, gy: i64, chroma: bool| -> [u8; 2] {
        if !(0..512).contains(&gx) || !(0..512).contains(&gy) {
            return if chroma { [8, 9] } else { [7, 0] };
        }
        let Some(t) = grid.tiles[(gy / 256 * 2 + gx / 256) as usize] else {
            return if chroma { [8, 9] } else { [7, 0] };
        };
        let (x, y) = ((gx % 256) as usize, (gy % 256) as usize);
        if chroma {
            let at = 65536 + (y / 2) * 256 + (x & !1);
            [t[at], t[at + 1]]
        } else {
            [t[y * 256 + x], 0]
        }
    };
    let (w, h, pitch) = (200u32, 120u32, 208usize);
    let chroma = pitch * h as usize;
    for degrees in [17.0f64, 133.0, 200.0, 301.0] {
        let r = degrees.to_radians();
        let up = ((r.sin() * 65536.0) as i64, (-r.cos() * 65536.0) as i64);
        let (across, down) = ((-up.1, up.0), (-up.0, -up.1));
        let origin = (250 << 16, 270 << 16);
        let anchor = (100, 80);
        let mut out = vec![0u8; chroma + pitch * h as usize / 2];
        nv12::rotate(
            &grid,
            &mut out,
            pitch,
            chroma,
            (w, h),
            origin,
            anchor,
            across,
            down,
        );
        let at = |u: i64, v: i64| {
            (
                origin.0 + (u - anchor.0) * across.0 + (v - anchor.1) * down.0,
                origin.1 + (u - anchor.0) * across.1 + (v - anchor.1) * down.1,
            )
        };
        for v in 0..i64::from(h) {
            for u in 0..i64::from(w) {
                let (x, y) = at(u, v);
                let want = sample(x >> 16, y >> 16, false)[0];
                assert_eq!(
                    out[v as usize * pitch + u as usize],
                    want,
                    "{degrees}° luma {u},{v}"
                );
            }
        }
        for cv in 0..i64::from(h) / 2 {
            for cu in 0..i64::from(w) / 2 {
                let (x, y) = at(2 * cu, 2 * cv);
                let x = x + (across.0 + down.0) / 2;
                let y = y + (across.1 + down.1) / 2;
                let o = chroma + cv as usize * pitch + 2 * cu as usize;
                let want = sample(x >> 16, y >> 16, true);
                assert_eq!(out[o..o + 2], want, "{degrees}° chroma {cu},{cv}");
            }
        }
    }
}

/// A heading-up frame from a real tile set, as a picture to look at:
/// `LEHU_MAP_DIR=<tiles> LEHU_SHOTS=<dir> cargo test -p lehu-map turned_shot -- --ignored`.
#[test]
#[ignore]
fn turned_shot() {
    use std::f64::consts::FRAC_1_SQRT_2;
    let dir = std::env::var("LEHU_MAP_DIR").unwrap();
    let shots = std::path::PathBuf::from(std::env::var("LEHU_SHOTS").unwrap());
    let set = TileSet::open(Path::new(&dir)).unwrap();
    let mut tiles = Tiles::new(set, None, None);
    let (lat, lon) = (49.6116, 6.1300);
    let (wx, wy) = crate::world_from_degrees(lat, lon).unwrap();
    let zoom = 16u8;
    let s = WORLD_Z - zoom;
    let (px, py) = ((wx >> s) << 16, (wy >> s) << 16);
    for (name, up) in [
        ("north", (0.0f64, -1.0f64)),
        ("east", (1.0, 0.0)),
        ("south-west", (-FRAC_1_SQRT_2, FRAC_1_SQRT_2)),
    ] {
        let h = ((up.0 * 65536.0) as i64, (up.1 * 65536.0) as i64);
        let (across, down) = ((-h.1, h.0), (-h.0, -h.1));
        let (w, hgt) = (800u32, 480u32);
        let anchor = (400i64, 331i64);
        let tile = TILE << 16;
        let (tx0, ty0) = ((px >> 16) / TILE - 3, (py >> 16) / TILE - 3);
        let grid = tiles.grid(zoom, (tx0, ty0), 7, 7);
        let pitch = w as usize;
        let chroma = pitch * hgt as usize;
        let mut out = vec![0u8; chroma + pitch * hgt as usize / 2];
        let origin = (px - tx0 * tile, py - ty0 * tile);
        nv12::rotate(
            &grid,
            &mut out,
            pitch,
            chroma,
            (w, hgt),
            origin,
            anchor,
            across,
            down,
        );
        drop(grid);
        nv12::arrow(&mut out, pitch, chroma, (w, hgt), anchor, (0, -1024));
        // NV12 (limited range, BT.601) to a PPM.
        let mut ppm = format!("P6\n{w} {hgt}\n255\n").into_bytes();
        for y in 0..hgt as usize {
            for x in 0..w as usize {
                let yy = (i32::from(out[y * pitch + x]) - 16) * 298;
                let c = chroma + (y / 2) * pitch + (x & !1);
                let (u, v) = (i32::from(out[c]) - 128, i32::from(out[c + 1]) - 128);
                let k = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
                ppm.extend([k(yy + 409 * v), k(yy - 100 * u - 208 * v), k(yy + 516 * u)]);
            }
        }
        std::fs::write(shots.join(format!("up-{name}.ppm")), ppm).unwrap();
    }
}

/// What a heading-up frame costs, by its parts, on the machine it runs on:
/// `LEHU_TRACK=<drive.gpx> <test binary> heading_up_cost --ignored --nocapture`.
#[test]
#[ignore]
fn heading_up_cost() {
    use std::time::Instant;
    let one = pattern_tiles();
    let tiles: Vec<Vec<u8>> = (0..20).map(|i| one[i % 4].clone()).collect();
    let grid = nv12::Grid {
        tiles: tiles.iter().map(|t| Some(t.as_slice())).collect(),
        cols: 5,
        rows: 4,
        side: 256,
        land: (1, 2, 3),
    };
    let (w, h, pitch) = (800u32, 480u32, 800usize);
    let chroma = pitch * h as usize;
    let mut out = vec![0u8; chroma + pitch * h as usize / 2];
    let frames = 10;
    for degrees in [0.0f64, 30.0, 90.0] {
        let r = degrees.to_radians();
        let up = ((r.sin() * 65536.0) as i64, (-r.cos() * 65536.0) as i64);
        let started = Instant::now();
        for _ in 0..frames {
            nv12::rotate(
                &grid,
                &mut out,
                pitch,
                chroma,
                (w, h),
                (640 << 16, 512 << 16),
                (400, 331),
                (-up.1, up.0),
                (-up.0, -up.1),
            );
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        println!("rotate {degrees}°: {ms:.1} ms a frame");
    }
    let Ok(path) = std::env::var("LEHU_TRACK") else {
        return;
    };
    let track = crate::Track::parse_gpx(&std::fs::read_to_string(path).unwrap()).unwrap();
    let line = track.line();
    let mut worst = 0.0f64;
    let mut sum = 0.0;
    let mut n = 0;
    for ms in (0..track.duration()).step_by(5000) {
        let f = track.at(ms);
        let view = View {
            zoom: 17,
            x: f.x,
            y: f.y,
        };
        let layout = Layout::around(&view, (w, h), 3, 2);
        let points: Vec<(i64, i64)> = line.iter().map(|&p| layout.sixteenth(p)).collect();
        let started = Instant::now();
        nv12::route_by(
            &mut out,
            pitch,
            chroma,
            (w, h),
            (0, 0, i64::from(w), i64::from(h)),
            &points,
            1,
        );
        let took = started.elapsed().as_secs_f64() * 1000.0;
        worst = worst.max(took);
        sum += took;
        n += 1;
    }
    println!(
        "route: {:.1} ms a frame, {:.1} at most ({n} places)",
        sum / f64::from(n),
        worst
    );
}

/// A 200 x 100 NV12 canvas of one colour, and its pitch and chroma.
fn plain(y: u8, c: [u8; 2]) -> Vec<u8> {
    let mut buf = vec![y; 200 * 100];
    buf.extend([c[0], c[1]].repeat(100 * 50));
    buf
}

fn surface(buf: &mut [u8]) -> nv12::Surface<'_> {
    nv12::Surface {
        pixels: buf,
        pitch: 200,
        chroma: 200 * 100,
        size: (200, 100),
    }
}

/// The box of the lumas that are not `y`: left, top, right, bottom.
fn changed(buf: &[u8], y: u8) -> Option<(usize, usize, usize, usize)> {
    let mut b: Option<(usize, usize, usize, usize)> = None;
    for (i, &v) in buf[..200 * 100].iter().enumerate() {
        if v != y {
            let (x, r) = (i % 200, i / 200);
            b = Some(match b {
                None => (x, r, x, r),
                Some(o) => (o.0.min(x), o.1.min(r), o.2.max(x), o.3.max(r)),
            });
        }
    }
    b
}

fn way(degrees: f64) -> (i64, i64) {
    let r = degrees.to_radians();
    (
        (r.cos() * 65536.0).round() as i64,
        (r.sin() * 65536.0).round() as i64,
    )
}

#[test]
fn a_name_is_dark_text_on_a_white_halo_and_nothing_else_changes() {
    let mut glyphs = crate::glyphs::Glyphs::new().unwrap();
    let sprite = glyphs.sprite("Hal", 22);
    // As wide as three letters and their halo, about the size high.
    assert!((30..80).contains(&sprite.w), "{}", sprite.w);
    assert!((20..36).contains(&sprite.h), "{}", sprite.h);
    let lumas = nv12::label_lumas([60, 60, 60], [255, 255, 255]);
    let (text, halo) = (ycc([60, 60, 60]).0, ycc([255, 255, 255]).0);
    assert_eq!((lumas[255], lumas[1]), (text, halo));
    let mut buf = plain(100, [90, 160]);
    let at = (100 * 16, 50 * 16);
    nv12::stamp(
        &mut surface(&mut buf),
        (0, 0, 200, 100),
        &sprite.stamp(),
        at,
        (65536, 0),
        &lumas,
        [128, 128],
    );
    let lumas_seen: Vec<u8> = buf[..20000].iter().copied().filter(|&v| v != 100).collect();
    assert!(lumas_seen.contains(&text) && lumas_seen.contains(&halo));
    assert!(lumas_seen.iter().all(|&v| (text..=halo).contains(&v)));
    // Centred on its place, the box of its ink with the halo.
    let b = changed(&buf, 100).unwrap();
    assert_eq!((b.2 - b.0 + 1, b.3 - b.1 + 1), (sprite.w, sprite.h));
    assert!((b.0 + b.2).abs_diff(200) <= 4, "{b:?}");
    // The chroma: the halo's where it covers, else as it was.
    let pairs = buf[20000..].as_chunks::<2>().0;
    assert!(pairs.contains(&[128, 128]));
    assert!(pairs.iter().all(|p| *p == [128, 128] || *p == [90, 160]));
}

#[test]
fn a_name_keeps_to_its_clip() {
    let mut glyphs = crate::glyphs::Glyphs::new().unwrap();
    let sprite = glyphs.sprite("Grand-Rue", 22);
    let lumas = nv12::label_lumas([60, 60, 60], [255, 255, 255]);
    let mut buf = plain(100, [90, 160]);
    nv12::stamp(
        &mut surface(&mut buf),
        (0, 0, 100, 100),
        &sprite.stamp(),
        (100 * 16, 50 * 16),
        way(20.0),
        &lumas,
        [128, 128],
    );
    let b = changed(&buf, 100).unwrap();
    assert!(b.0 < 80 && b.2 == 99, "{b:?}");
    for r in 0..50 {
        let row = &buf[20000 + r * 200..20000 + (r + 1) * 200];
        assert!(
            row[100..]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|p| *p == [90, 160])
        );
    }
}

/// What `stamp` does, the plain way: every pixel and block of the canvas
/// samples the stamp, none skipped.
fn stamp_everywhere(
    buf: &mut [u8],
    s: &nv12::Stamp<'_>,
    at: (i64, i64),
    (c, sn): (i64, i64),
    lumas: &[u8; 256],
) {
    let sample = |px: i64, py: i64| -> u8 {
        let (dx, dy) = (px - at.0, py - at.1);
        let u = ((dx * c + dy * sn) >> 4) + (s.anchor.0 << 12);
        let v = ((dy * c - dx * sn) >> 4) + (s.anchor.1 << 12);
        if u < 0 || v < 0 || (u >> 16) as usize >= s.w || (v >> 16) as usize >= s.h {
            return 0;
        }
        s.px[(v >> 16) as usize * s.w + (u >> 16) as usize]
    };
    for y in 0..100i64 {
        for x in 0..200i64 {
            let i = sample(x * 16 + 8, y * 16 + 8);
            if i != 0 {
                buf[(y * 200 + x) as usize] = lumas[usize::from(i)];
            }
        }
    }
    for by in 0..50i64 {
        for bx in 0..100i64 {
            if sample(bx * 32 + 16, by * 32 + 16) != 0 {
                let o = 20000 + (by * 200 + 2 * bx) as usize;
                buf[o..o + 2].copy_from_slice(&[128, 128]);
            }
        }
    }
}

#[test]
fn at_any_angle_a_name_covers_what_it_would_pixel_by_pixel() {
    let mut glyphs = crate::glyphs::Glyphs::new().unwrap();
    let sprite = glyphs.sprite("Avenue de la Gare", 20);
    let lumas = nv12::label_lumas([200, 204, 210], [24, 26, 30]);
    for degrees in (0..360)
        .step_by(7)
        .map(f64::from)
        .chain([90.0, 180.0, 270.0])
    {
        let w = way(degrees);
        // Not on a pixel's middle, and partly off the canvas.
        let at = (97 * 16 + 5, 61 * 16 + 11);
        let mut fast = plain(100, [90, 160]);
        let mut slow = fast.clone();
        nv12::stamp(
            &mut surface(&mut fast),
            (0, 0, 200, 100),
            &sprite.stamp(),
            at,
            w,
            &lumas,
            [128, 128],
        );
        stamp_everywhere(&mut slow, &sprite.stamp(), at, w, &lumas);
        assert!(fast == slow, "{degrees}°");
    }
}

#[test]
fn a_name_turned_half_round_covers_the_same_place() {
    let mut glyphs = crate::glyphs::Glyphs::new().unwrap();
    let sprite = glyphs.sprite("Rue Notre-Dame", 22);
    let lumas = nv12::label_lumas([60, 60, 60], [255, 255, 255]);
    let at = (100 * 16, 50 * 16);
    let boxes: Vec<_> = [(65536, 0), (-65536, 0)]
        .into_iter()
        .map(|w| {
            let mut buf = plain(100, [90, 160]);
            nv12::stamp(
                &mut surface(&mut buf),
                (0, 0, 200, 100),
                &sprite.stamp(),
                at,
                w,
                &lumas,
                [128, 128],
            );
            changed(&buf, 100).unwrap()
        })
        .collect();
    // About its place: the one box the other mirrored, to a pixel.
    let (a, b) = (boxes[0], boxes[1]);
    assert!(
        (a.0 + b.2).abs_diff(199) <= 1 && (a.2 + b.0).abs_diff(199) <= 1,
        "{a:?} {b:?}"
    );
    assert!(
        (a.1 + b.3).abs_diff(99) <= 1 && (a.3 + b.1).abs_diff(99) <= 1,
        "{a:?} {b:?}"
    );
}

/// NV12 (limited range, BT.601) to RGB: the `w` x `h` part at (`x0`,
/// `y0`).
pub(crate) fn nv12_rgb(
    out: &[u8],
    pitch: usize,
    chroma: usize,
    (x0, y0): (usize, usize),
    (w, h): (usize, usize),
) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let yy = (i32::from(out[y * pitch + x]) - 16) * 298;
            let c = chroma + (y / 2) * pitch + (x & !1);
            let (u, v) = (i32::from(out[c]) - 128, i32::from(out[c + 1]) - 128);
            let k = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
            rgb.extend([k(yy + 409 * v), k(yy - 100 * u - 208 * v), k(yy + 516 * u)]);
        }
    }
    rgb
}

pub(crate) fn save_png(path: &Path, rgb: &[u8], (w, h): (u32, u32)) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgb).unwrap();
}

/// The heading-up frame, as the viewer has it for an 800 x 480 panel: 32
/// pixels each side, 160 rows more; the window 16 rows above its bottom,
/// the view's point at 50 % across and 69 % down the window.
const UP_FRAME: (u32, u32) = (864, 640);
const UP_WINDOW: (usize, usize) = (32, 144);
const UP_ANCHOR: (i64, i64) = (32 + 400, 144 + 331);

fn heading(degrees: f64) -> (i64, i64) {
    let r = degrees.to_radians();
    ((r.sin() * 65536.0) as i64, (-r.cos() * 65536.0) as i64)
}

/// Heading-up frames with the names drawn over them, from a set drawn with
/// `--labels data`, as the panel shows them, and a north-up canvas:
/// `LEHU_MAP_DIR=<tiles> LEHU_SHOTS=<dir> cargo test -p lehu-map labelled_shot -- --ignored`.
#[test]
#[ignore]
fn labelled_shot() {
    let dir = std::path::PathBuf::from(std::env::var("LEHU_MAP_DIR").unwrap());
    let shots = std::path::PathBuf::from(std::env::var("LEHU_SHOTS").unwrap());
    let set = TileSet::open(&dir).unwrap();
    let night = set.night().map(|n| n.unwrap());
    let mut tiles = Tiles::new(set, night, None);
    let mut names = crate::labels::Names::open(&dir).expect("label data");
    let (w, h) = UP_FRAME;
    let pitch = w as usize;
    let chroma = pitch * h as usize;
    let mut buf = vec![0u8; chroma + pitch * h as usize / 2];
    // Luxembourg City, by the station, and the old town; and Esch, out
    // of the city's deeper zooms in the example set.
    let places = [
        ("gare", 49.6000, 6.1335),
        ("centre", 49.6116, 6.1300),
        ("esch", 49.4958, 5.9806),
    ];
    for (night, shades) in [(false, "day"), (true, "night")] {
        tiles.set_night(night);
        for (place, lat, lon) in places {
            let (x, y) = crate::world_from_degrees(lat, lon).unwrap();
            for zoom in [16u8, 17, 18] {
                for (name, degrees) in [("north", 0.0), ("east", 90.0), ("sw", 225.0), ("s", 180.0)]
                {
                    if night && (zoom != 17 || degrees != 225.0) {
                        continue;
                    }
                    let view = View { zoom, x, y };
                    crate::viewer::render_up(
                        &mut tiles,
                        Some(&mut names),
                        &mut nv12::Surface {
                            pixels: &mut buf,
                            pitch,
                            chroma,
                            size: (w, h),
                        },
                        &view,
                        heading(degrees),
                        Some(crate::Marker::Arrow(x, y, (0, -1024))),
                        None,
                        None,
                        None,
                        UP_ANCHOR,
                        0,
                        0,
                        1,
                        &mut Default::default(),
                    );
                    let rgb = nv12_rgb(&buf, pitch, chroma, UP_WINDOW, (800, 480));
                    let file = shots.join(format!("names-{shades}-{place}-z{zoom}-{name}.png"));
                    save_png(&file, &rgb, (800, 480));
                }
            }
        }
    }
    // North up, as composed.
    tiles.set_night(false);
    let (x, y) = crate::world_from_degrees(49.6116, 6.1300).unwrap();
    let view = View { zoom: 17, x, y };
    let layout = Layout::around(&view, (800, 480), 5, 3);
    let (cw, chh) = (5 * 256usize, 3 * 256usize);
    let mut canvas = vec![0u8; cw * chh * 3 / 2];
    tiles.compose(&layout, &mut canvas, cw, cw * chh);
    crate::viewer::draw_names(
        &mut names,
        &mut nv12::Surface {
            pixels: &mut canvas,
            pitch: cw,
            chroma: cw * chh,
            size: (cw as u32, chh as u32),
        },
        &layout,
        (0, 0, cw as i64, chh as i64),
        false,
    );
    let rgb = nv12_rgb(&canvas, cw, cw * chh, (0, 0), (cw, chh));
    save_png(
        &shots.join("names-north-up-z17.png"),
        &rgb,
        (cw as u32, chh as u32),
    );
}

/// What the names cost on a heading-up frame, on the machine it runs on:
/// `<test binary> labels_cost --ignored --nocapture`, and with
/// `LEHU_MAP_DIR=<tiles drawn with --labels data>` whole frames with and
/// without them, at Luxembourg City.
#[test]
#[ignore]
fn labels_cost() {
    use std::time::Instant;
    const STREETS: [&str; 20] = [
        "Boulevard Royal",
        "Avenue de la Liberté",
        "Rue de la Gare",
        "Grand-Rue",
        "Rue Notre-Dame",
        "Boulevard de la Pétrusse",
        "Avenue Émile Reuter",
        "Rue du Fossé",
        "Côte d'Eich",
        "Rue de Strasbourg",
        "Boulevard d'Avranches",
        "Rue Michel Rodange",
        "Avenue de la Porte-Neuve",
        "Rue Philippe II",
        "Rue des Capucins",
        "Montée de Clausen",
        "Rue de Hollerich",
        "Route d'Esch",
        "Rue Sigismond",
        "Boulevard Prince Henri",
    ];
    // Forty names over the frame at zoom 17, every way, and two places.
    let map = temp("labels-cost");
    let (bx, by) = (1000i64, 700i64);
    let dir = map.join(format!("labels/17/{bx}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (ox, oy) = (bx * 2048, by * 2048);
    let mut text = String::new();
    for i in 0..40i64 {
        let (x, y) = (ox + 60 + (i % 5) * 190, oy + 40 + (i / 5) * 78);
        text += &format!(
            "s\t{x}\t{y}\t{}\t22\t{}\n",
            (i * 37) % 180 - 89,
            STREETS[i as usize % 20]
        );
    }
    text += &format!("p\t{}\t{}\t0\t24\tLuxembourg\n", ox + 500, oy + 330);
    text += &format!("p\t{}\t{}\t0\t22\tLimpertsberg\n", ox + 300, oy + 600);
    std::fs::write(dir.join(format!("{by}.txt")), text).unwrap();
    let mut names = crate::labels::Names::open(&map).unwrap();
    let (w, h) = UP_FRAME;
    let pitch = w as usize;
    let chroma = pitch * h as usize;
    let mut buf = vec![0u8; chroma + pitch * h as usize / 2];
    let s = WORLD_Z - 17;
    let centre = ((ox + 500) << s, (oy + 330) << s);
    let mut draw = |names: &mut crate::labels::Names, degrees: f64| {
        let r = degrees.to_radians();
        let (c, sn) = ((r.cos() * 65536.0) as i64, (r.sin() * 65536.0) as i64);
        let before = names.drawn;
        names.draw(
            &mut nv12::Surface {
                pixels: &mut buf,
                pitch,
                chroma,
                size: (w, h),
            },
            (0, 0, i64::from(w), i64::from(h)),
            17,
            (
                centre.0 - (600 << s),
                centre.1 - (600 << s),
                centre.0 + (600 << s),
                centre.1 + (600 << s),
            ),
            false,
            |p| {
                let (dx, dy) = ((p.0 - centre.0) >> (s - 4), (p.1 - centre.1) >> (s - 4));
                (
                    UP_ANCHOR.0 * 16 + ((dx * c - dy * sn) >> 16),
                    UP_ANCHOR.1 * 16 + ((dx * sn + dy * c) >> 16),
                )
            },
            |m| ((m.0 * c - m.1 * sn) >> 16, (m.0 * sn + m.1 * c) >> 16),
        );
        names.drawn - before
    };
    let started = Instant::now();
    let n = draw(&mut names, 0.0);
    println!(
        "names: {n} laid out and drawn, first time: {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let frames = 10;
    for degrees in [0.0f64, 30.0, 90.0, 200.0] {
        let started = Instant::now();
        let mut n = 0;
        for _ in 0..frames {
            n = draw(&mut names, degrees);
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        println!("names turned {degrees}°: {n} drawn, {ms:.2} ms a frame");
    }
    let _ = std::fs::remove_dir_all(&map);
    let Ok(dir) = std::env::var("LEHU_MAP_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let set = TileSet::open(&dir).unwrap();
    let mut tiles = Tiles::new(
        set,
        None,
        crate::hw::HwJpeg::open(256, 256 * 1024).ok().map(|h| h.0),
    );
    let mut names = crate::labels::Names::open(&dir).expect("label data");
    let (x, y) = crate::world_from_degrees(49.6116, 6.1300).unwrap();
    for zoom in [16u8, 17, 18] {
        let view = View { zoom, x, y };
        for degrees in [0.0f64, 30.0] {
            let mut frame = |names: Option<&mut crate::labels::Names>| {
                crate::viewer::render_up(
                    &mut tiles,
                    names,
                    &mut nv12::Surface {
                        pixels: &mut buf,
                        pitch,
                        chroma,
                        size: (w, h),
                    },
                    &view,
                    heading(degrees),
                    None,
                    None,
                    None,
                    None,
                    UP_ANCHOR,
                    0,
                    0,
                    1,
                    &mut Default::default(),
                );
            };
            // The tiles decoded and the names laid out first.
            frame(Some(&mut names));
            let time = |f: &mut dyn FnMut()| {
                let started = Instant::now();
                for _ in 0..5 {
                    f();
                }
                started.elapsed().as_secs_f64() * 1000.0 / 5.0
            };
            let bare = time(&mut || frame(None));
            let before = names.drawn;
            let named = time(&mut || frame(Some(&mut names)));
            println!(
                "zoom {zoom}, turned {degrees}°: {:.1} ms a frame, {:.1} with {} names",
                bare,
                named,
                (names.drawn - before) / 5
            );
        }
    }
}

#[test]
fn a_camera_sign_is_drawn_inside_its_clip() {
    let (w, h) = (64usize, 48usize);
    let (pitch, chroma) = (w, w * h);
    let blank = || {
        let mut b = vec![120u8; w * h * 3 / 2];
        b[chroma..].fill(128);
        b
    };
    let mut buf = blank();
    let whole = (0, 0, w as i64, h as i64);
    nv12::camera_sign(
        &mut buf,
        pitch,
        chroma,
        (w as u32, h as u32),
        whole,
        (32, 24),
    );
    let luma = |b: &[u8], x: usize, y: usize| b[y * pitch + x];
    let pair = |b: &[u8], x: usize, y: usize| {
        let c = chroma + (y / 2) * pitch + (x & !1);
        (b[c], b[c + 1])
    };
    let (ring, fill, ink) = (ycc([220, 38, 38]), ycc([255; 3]), ycc([33; 3]));
    // The ring is red, the lens white, the body round it dark.
    assert_eq!(luma(&buf, 32 + 10, 24), ring.0);
    assert_eq!(pair(&buf, 32 + 10, 24), (ring.1, ring.2));
    assert_eq!(luma(&buf, 32, 25), fill.0);
    assert_eq!(luma(&buf, 32 + 4, 24 + 3), ink.0);
    assert_eq!(pair(&buf, 32, 24), (128, 128));
    // Outside the sign, nothing.
    assert_eq!(luma(&buf, 32 + 12, 24), 120);
    assert_eq!(luma(&buf, 2, 2), 120);
    // Clipped to the left half: the right half is left as it was.
    let mut half = blank();
    nv12::camera_sign(
        &mut half,
        pitch,
        chroma,
        (w as u32, h as u32),
        (0, 0, 32, h as i64),
        (32, 24),
    );
    assert_eq!(luma(&half, 32 + 10, 24), 120);
    assert_eq!(pair(&half, 32 + 10, 24), (128, 128));
    assert_eq!(luma(&half, 32 - 10, 24), ring.0);
    // Off the canvas: nothing, and no panic.
    let mut off = blank();
    nv12::camera_sign(
        &mut off,
        pitch,
        chroma,
        (w as u32, h as u32),
        whole,
        (-40, 200),
    );
    assert_eq!(off, blank());
    // LEHU_SHOTS=<dir>: the sign four times its size, to look at.
    if let Ok(dir) = std::env::var("LEHU_SHOTS") {
        let rgb = nv12_rgb(&buf, pitch, chroma, (16, 8), (32, 32));
        let mut big = Vec::with_capacity(rgb.len() * 16);
        for y in 0..128 {
            for x in 0..128 {
                let at = ((y / 4) * 32 + x / 4) * 3;
                big.extend_from_slice(&rgb[at..at + 3]);
            }
        }
        save_png(&Path::new(&dir).join("camera-sign.png"), &big, (128, 128));
    }
}

/// What turning a frame and drawing a route cost, on the machine this
/// runs on (cross-built for the board, it says what the CPU there does):
/// `LEHU_MAP_DIR=<region> cargo test -p lehu-map turn_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn turn_bench() {
    use std::time::Instant;
    let dir = std::env::var("LEHU_MAP_DIR").unwrap();
    let set = TileSet::open(Path::new(&dir)).unwrap();
    let center = set.center;
    let mut tiles = Tiles::new(set, None, None);
    let zoom = 16u8;
    let s = WORLD_Z - zoom;
    let (px, py) = ((center.0 >> s) << 16, (center.1 >> s) << 16);
    let (w, hgt) = (864u32, 528u32);
    let pitch = w as usize;
    let chroma = pitch * hgt as usize;
    let mut out = vec![0u8; chroma + pitch * hgt as usize / 2];
    let anchor = (432i64, 331i64);
    let tile = TILE << 16;
    let (tx0, ty0) = ((px >> 16) / TILE - 3, (py >> 16) / TILE - 3);
    // The tiles decoded first.
    let _ = tiles.grid(zoom, (tx0, ty0), 7, 7);
    for degrees in [0.0f64, 10.0, 30.0, 45.0, 60.0, 90.0, 135.0] {
        let r = degrees.to_radians();
        let h = ((r.sin() * 65536.0) as i64, (-r.cos() * 65536.0) as i64);
        let (across, down) = ((-h.1, h.0), (-h.0, -h.1));
        let origin = (px - tx0 * tile, py - ty0 * tile);
        let mut best = f64::MAX;
        for _ in 0..3 {
            let grid = tiles.grid(zoom, (tx0, ty0), 7, 7);
            let t = Instant::now();
            nv12::rotate(
                &grid,
                &mut out,
                pitch,
                chroma,
                (w, hgt),
                origin,
                anchor,
                across,
                down,
            );
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("rotate {w}x{hgt} at {degrees:>5.1} deg: {best:.1} ms");
    }
    // A route: a serpentine across the frame, `n` points, in 1/16 px.
    for (n, amp) in [(100usize, 150.0f64), (400, 150.0), (400, 2000.0)] {
        let points: Vec<(i64, i64)> = (0..n)
            .map(|i| {
                let x = i as f64 / (n - 1) as f64 * 1600.0 - 400.0;
                let y = 264.0 + amp * (i as f64 * 0.3).sin();
                ((x * 16.0) as i64, (y * 16.0) as i64)
            })
            .collect();
        let clip = (0, 0, i64::from(w), i64::from(hgt));
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            nv12::route_by(&mut out, pitch, chroma, (w, hgt), clip, &points, 1);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        let inside = points
            .iter()
            .filter(|p| (0..i64::from(hgt) * 16).contains(&p.1))
            .count();
        println!("route of {n} points ({inside} in view, amplitude {amp} px): {best:.1} ms");
    }
}

/// What each step of getting a tile costs on this machine (cross-built for
/// the board, it says what the chip there does):
/// `LEHU_MAP_DIR=<region> cargo test -p lehu-map decode_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn decode_bench() {
    use std::time::Instant;
    let dir = std::env::var("LEHU_MAP_DIR").unwrap();
    let set = TileSet::open(Path::new(&dir)).unwrap();
    let center = set.center;
    // A zoom the set has everywhere (Hungary: 8 to 14 over the country).
    let zoom = 14u8;
    let s = WORLD_Z - zoom;
    let (tx, ty) = ((center.0 >> s) / TILE, (center.1 >> s) / TILE);
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    // The file: from the pack, warm and cold-ish (49 different tiles).
    let mut path = std::path::PathBuf::new();
    let mut file = Vec::new();
    let t = Instant::now();
    let mut n = 0;
    let mut bytes = 0;
    for dy in -3..=3 {
        for dx in -3..=3 {
            if set
                .read(zoom, tx + dx, ty + dy, &mut path, &mut file)
                .unwrap()
            {
                n += 1;
                bytes += file.len();
            }
        }
    }
    println!(
        "read {n} tiles from the pack: {:.2} ms each, {} bytes each",
        ms(t) / n as f64,
        bytes / n.max(1)
    );
    set.read(zoom, tx, ty, &mut path, &mut file).unwrap();
    let t = Instant::now();
    for _ in 0..20 {
        set.read(zoom, tx, ty, &mut path, &mut file).unwrap();
    }
    println!("read the same tile again: {:.2} ms", ms(t) / 20.0);
    // The engine: the shim's call alone, then the copy out and untile.
    let mut out = vec![0u8; crate::tiles::TILE_BYTES];
    match crate::hw::HwJpeg::open(TILE as u32, 256 * 1024) {
        Ok((mut hw, dev)) => {
            hw.decode(&file, &mut out).unwrap();
            let t = Instant::now();
            for _ in 0..20 {
                hw.decode(&file, &mut out).unwrap();
            }
            println!(
                "engine decode + copy + untile ({dev}): {:.2} ms",
                ms(t) / 20.0
            );
            let staging = vec![0u8; crate::tiles::TILE_BYTES];
            let t = Instant::now();
            for _ in 0..20 {
                crate::hw::untile(&staging, TILE as usize, (TILE * TILE) as usize, &mut out);
            }
            println!("  of which untile (cached source): {:.2} ms", ms(t) / 20.0);
        }
        Err(e) => println!("no engine: {e}"),
    }
    let t = Instant::now();
    let p = lehu_image::decode(&file, TILE as u32, set.land).unwrap();
    let took = ms(t);
    let t = Instant::now();
    nv12::from_rgb(&p.rgb, TILE as usize, &mut out);
    println!(
        "software decode: {took:.1} ms, RGB to NV12: {:.2} ms",
        ms(t)
    );
    let mut big = vec![0u8; crate::tiles::TILE_BYTES];
    let t = Instant::now();
    for _ in 0..20 {
        nv12::enlarge_quarter(&out, TILE as usize, (1, 0), &mut big);
    }
    println!("enlarge a quarter: {:.2} ms", ms(t) / 20.0);
    let night = nv12::Night::new(set.land);
    let t = Instant::now();
    for _ in 0..20 {
        night.apply(&mut out, TILE as usize);
    }
    println!("night lookup on a tile: {:.2} ms", ms(t) / 20.0);
    // 49 different tiles through the engine, read and decode apart.
    if let Ok((mut hw, _)) = crate::hw::HwJpeg::open(TILE as u32, 256 * 1024) {
        let (mut read_ms, mut dec_ms, mut n) = (0.0, 0.0, 0);
        for dy in -3..=3 {
            for dx in -3..=3 {
                let t = Instant::now();
                let there = set
                    .read(zoom, tx + dx + 10, ty + dy + 10, &mut path, &mut file)
                    .unwrap();
                read_ms += ms(t);
                if there {
                    let t = Instant::now();
                    hw.decode(&file, &mut out).unwrap();
                    dec_ms += ms(t);
                    n += 1;
                }
            }
        }
        println!(
            "{n} different tiles: read {:.2} ms each, engine decode + copy + untile {:.2} ms each",
            read_ms / n.max(1) as f64,
            dec_ms / n.max(1) as f64
        );
        let t = Instant::now();
        for _ in 0..20 {
            let b = vec![0u8; crate::tiles::TILE_BYTES].into_boxed_slice();
            std::hint::black_box(&b);
        }
        println!(
            "a tile's buffer allocated and zeroed: {:.2} ms",
            ms(t) / 20.0
        );
    }
    // A cold grid and a cold canvas.
    let hw = crate::hw::HwJpeg::open(TILE as u32, 256 * 1024)
        .ok()
        .map(|(h, _)| h);
    let mut tiles = Tiles::new(TileSet::open(Path::new(&dir)).unwrap(), None, hw);
    let t = Instant::now();
    let g = tiles.grid(zoom, (tx - 3, ty - 3), 7, 7);
    let have = g.tiles.iter().filter(|t| t.is_some()).count();
    drop(g);
    println!(
        "cold grid of 49 tiles ({have} there): {:.1} ms, {} decoded",
        ms(t),
        tiles.decoded
    );
    let t = Instant::now();
    let g = tiles.grid(zoom, (tx - 3, ty - 3), 7, 7);
    drop(g);
    println!("the same grid warm: {:.2} ms", ms(t));
    let view = View {
        zoom,
        x: center.0,
        y: center.1,
    };
    let layout = Layout::around(&view, (800, 480), 5, 3);
    let (w, h) = (5 * TILE as usize, 3 * TILE as usize);
    let mut canvas = vec![0u8; w * h * 3 / 2];
    let t = Instant::now();
    tiles.compose(&layout, &mut canvas, w, w * h);
    println!(
        "compose a 5x3 canvas (tiles warm, canvas pages cold): {:.1} ms",
        ms(t)
    );
    let t = Instant::now();
    tiles.compose(&layout, &mut canvas, w, w * h);
    println!("compose it again (canvas pages warm): {:.1} ms", ms(t));
    let t = Instant::now();
    let mut other = vec![0u8; w * h * 3 / 2];
    other.copy_from_slice(&canvas);
    println!(
        "a plain memcpy of the canvas, {} KB, pages cold: {:.1} ms",
        w * h * 3 / 2 / 1024,
        ms(t)
    );
    let t = Instant::now();
    other.copy_from_slice(&canvas);
    println!("the same memcpy, pages warm: {:.1} ms", ms(t));
}

/// What the names cost, cold and warm, on this machine (cross-built for
/// the board, it says what the chip there does):
/// `LEHU_MAP_DIR=<region> cargo test -p lehu-map names_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn names_bench() {
    use std::time::Instant;
    let dir = std::env::var("LEHU_MAP_DIR").unwrap();
    let set = TileSet::open(Path::new(&dir)).unwrap();
    let center = set.center;
    let Some(mut names) = crate::labels::Names::open(set.dir()) else {
        println!("no names in {dir}");
        return;
    };
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let (w, h) = (864u32, 528u32);
    let pitch = w as usize;
    let chroma = pitch * h as usize;
    let mut buf = vec![0u8; chroma + pitch * h as usize / 2];
    // Budapest, where the names are dense: 47.4979 N, 19.0402 E.
    let (bx, by) = crate::world_from_degrees(47.4979, 19.0402).unwrap();
    for (what, x, y) in [
        ("the region's centre", center.0, center.1),
        ("Budapest", bx, by),
    ] {
        for zoom in [14u8, 16] {
            let view = View { zoom, x, y };
            let layout = Layout::around(&view, (800, 480), 5, 3);
            let whole = (0, 0, i64::from(w), i64::from(h));
            let mut times = Vec::new();
            let before = names.drawn;
            for _ in 0..3 {
                let t = Instant::now();
                crate::viewer::draw_names(
                    &mut names,
                    &mut nv12::Surface {
                        pixels: &mut buf,
                        pitch,
                        chroma,
                        size: (w, h),
                    },
                    &layout,
                    whole,
                    false,
                );
                times.push(ms(t));
            }
            let drawn = (names.drawn - before) / 3;
            println!(
                "{what} zoom {zoom}: {drawn} names; cold {:.1} ms, warm {:.1} ms, {:.1} ms",
                times[0], times[1], times[2]
            );
            // The same area a little further on: new blocks, some names new.
            let s = WORLD_Z - zoom;
            let moved = View {
                zoom,
                x: x + (600i64 << s),
                y,
            };
            let layout2 = Layout::around(&moved, (800, 480), 5, 3);
            let t = Instant::now();
            crate::viewer::draw_names(
                &mut names,
                &mut nv12::Surface {
                    pixels: &mut buf,
                    pitch,
                    chroma,
                    size: (w, h),
                },
                &layout2,
                whole,
                false,
            );
            println!("  600 px on: {:.1} ms", ms(t));
        }
    }
}

/// What drawing a long route costs a frame, by its length in points.
#[test]
#[ignore]
fn route_bench() {
    use std::time::Instant;
    let (w, h) = (864u32, 528u32);
    let pitch = w as usize;
    let chroma = pitch * h as usize;
    let mut out = vec![0u8; chroma + pitch * h as usize / 2];
    let clip = (0, 0, i64::from(w), i64::from(h));
    for n in [200usize, 1000, 5000, 20000] {
        // A road wandering off across the country, points every 6 px as a
        // route at zoom 17 has them, only the first few hundred in view.
        let points: Vec<(i64, i64)> = (0..n)
            .map(|i| {
                let x = 100.0 + i as f64 * 5.0 + 30.0 * (i as f64 * 0.05).sin();
                let y = 260.0 + i as f64 * 1.5 + 20.0 * (i as f64 * 0.11).cos();
                ((x * 16.0) as i64, (y * 16.0) as i64)
            })
            .collect();
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            nv12::route_by(&mut out, pitch, chroma, (w, h), clip, &points, 1);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("route of {n} points: {best:.1} ms a frame");
    }
}
