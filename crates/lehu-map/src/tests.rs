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
