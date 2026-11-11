// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Tests of the data on the card: packed tiles behind a tile set, and
//! night made from the day tiles.

use std::path::PathBuf;

use crate::nv12::{self, NIGHT_LAND, Night, ycc};
use crate::pmtiles::write::pmtiles;
use crate::tests::{nv12_at, nv12_rgb, save_png, temp, write_png};
use crate::tiles::{LAND, TILE_BYTES, Tiles};
use crate::tileset::TileSet;
use crate::view::{Layout, View};

const META_JPG: &str = r#"
name = "Packed"
format = "jpg"
tile-size = 256
min-zoom = 2
max-zoom = 16
bounds = [100000000, 200000000, 300000000, 400000000]
center = [200000000, 300000000]
attribution = "© OpenStreetMap contributors"
"#;

fn png_bytes(rgb: [u8; 3]) -> Vec<u8> {
    let dir = temp(&format!("png-{}-{}-{}", rgb[0], rgb[1], rgb[2]));
    let path = dir.join("t.png");
    write_png(&path, rgb);
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// A row of zoom 3 composed: (column, row) 0..4 x 2 from (0, 2).
fn compose_row(tiles: &mut Tiles) -> (Vec<u8>, usize, usize) {
    let layout = Layout {
        zoom: 3,
        tx: 0,
        ty: 2,
        cols: 4,
        rows: 2,
    };
    let pitch = 4 * 256;
    let chroma = pitch * 512;
    let mut canvas = vec![0u8; chroma + pitch * 256];
    tiles.compose(&layout, &mut canvas, pitch, chroma);
    (canvas, pitch, chroma)
}

#[test]
fn a_packed_tile_set_composes_as_its_loose_tiles_do() {
    let dir = temp("packed-set");
    std::fs::write(dir.join("map.toml"), META_JPG).unwrap();
    // The decoder takes a tile by its bytes: PNG ones stand in for JPEG.
    let tiles = [
        ((3, 1, 2), png_bytes([255, 0, 0])),
        ((3, 2, 2), png_bytes([0, 0, 255])),
        ((3, 3, 2), b"not a picture".to_vec()),
    ];
    std::fs::write(dir.join("tiles.pmtiles"), pmtiles(&tiles, 0)).unwrap();
    let set = TileSet::open(&dir).unwrap();
    assert!(set.is_packed() && set.is_jpeg());
    let mut t = Tiles::new(set, None, None);
    let (canvas, pitch, chroma) = compose_row(&mut t);
    let at = |x, y| nv12_at(&canvas, pitch, chroma, x, y);
    assert_eq!(at(10, 10), ycc(LAND));
    assert_eq!(at(256 + 100, 255), ycc([255, 0, 0]));
    assert_eq!(at(512 + 255, 0), ycc([0, 0, 255]));
    assert_eq!(at(768 + 3, 3), ycc(LAND));
    assert_eq!(at(1023, 511), ycc(LAND));
    assert_eq!((t.decoded, t.software, t.failed), (2, 2, 1));
    // A file that is not a pack: the set does not open, and says why.
    std::fs::write(dir.join("tiles.pmtiles"), b"PMTiles").unwrap();
    assert!(TileSet::open(&dir).unwrap_err().contains("tiles.pmtiles"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn night_from_day_darkens_the_land_and_keeps_the_roads_lighter() {
    let night = Night::new(LAND);
    let (land, road, wood, text) = (
        ycc(LAND),
        ycc([255, 255, 255]),
        ycc([173, 209, 158]),
        ycc([60, 60, 60]),
    );
    // The land is the night land's colour.
    let n = night.ycc(land);
    let want = ycc(NIGHT_LAND);
    assert!(n.0.abs_diff(want.0) <= 1 && n.1.abs_diff(want.1) <= 1 && n.2.abs_diff(want.2) <= 1);
    // A white road well above it; a wood a little; dark text lighter.
    let (l, r, w, t) = (n.0, night.ycc(road).0, night.ycc(wood).0, night.ycc(text).0);
    assert!(r >= l + 40, "road {r}, land {l}");
    assert!(w > l && w < l + 30, "wood {w}, land {l}");
    assert!(t > l + 20, "text {t}, land {l}");
    // Brighter by day, never darker at night, above the land; and all
    // in the limited range.
    let mut last = 0;
    for v in land.0..=255 {
        let o = night.ycc((v, 128, 128)).0;
        assert!(o >= last && (16..=235).contains(&o));
        last = o;
    }
}

#[test]
fn without_night_tiles_night_is_made_from_the_days() {
    let dir = temp("night-lut");
    std::fs::write(dir.join("map.toml"), crate::tests::META).unwrap();
    write_png(&dir.join("3/1/2.png"), [255, 255, 255]);
    let mut t = Tiles::new(TileSet::open(&dir).unwrap(), None, None);
    let (day, pitch, chroma) = compose_row(&mut t);
    assert!(t.set_night(true) && t.night());
    assert!(!t.set_night(true));
    let (dark, ..) = compose_row(&mut t);
    let night = Night::new(LAND);
    let at = |c: &[u8], x, y| nv12_at(c, pitch, chroma, x, y);
    assert_eq!(at(&day, 10, 10), ycc(LAND));
    assert_eq!(at(&dark, 10, 10), night.ycc(ycc(LAND)));
    assert_eq!(at(&dark, 300, 100), night.ycc(ycc([255, 255, 255])));
    // Decoded again for the night, and back again by day.
    assert_eq!(t.decoded, 2);
    assert!(t.set_night(false) && !t.night());
    let (again, ..) = compose_row(&mut t);
    assert_eq!(again, day);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the night tables cost a tile, on the machine it runs on:
/// `<test binary> night_lut_cost --ignored --nocapture`.
#[test]
#[ignore]
fn night_lut_cost() {
    let night = Night::new(LAND);
    let mut tile: Vec<u8> = (0..TILE_BYTES).map(|i| (i * 7 % 251) as u8).collect();
    let rounds = 2000;
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        night.apply(std::hint::black_box(&mut tile), 256);
    }
    let each = start.elapsed() / rounds;
    // A copy of the tile, for scale: the machine's speed and load now.
    let mut copy = vec![0u8; TILE_BYTES];
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(&mut copy).copy_from_slice(std::hint::black_box(&tile));
    }
    let copied = start.elapsed() / rounds;
    println!("night tables: {each:?} a 256 x 256 tile; a copy of it {copied:?}");
}

/// Day, night made from it, and the night tiles drawn on the PC, side by
/// side: `LEHU_MAP_DIR=<tiles with night/> LEHU_SHOTS=<dir> cargo test -p
/// lehu-map night_compare -- --ignored`. Each `night-<place>-z<zoom>.png`
/// is three 800 x 480 views, left to right.
#[test]
#[ignore]
fn night_compare() {
    let dir = PathBuf::from(std::env::var("LEHU_MAP_DIR").unwrap());
    let shots = PathBuf::from(std::env::var("LEHU_SHOTS").unwrap());
    let set = TileSet::open(&dir).unwrap();
    let drawn = set.night().expect("night/ beside the tiles").unwrap();
    let mut made = Tiles::new(set.clone(), None, None);
    let mut both = Tiles::new(set, Some(drawn), None);
    let mut names = crate::labels::Names::open(&dir);
    let places = [
        ("centre", 49.6116, 6.1300, [13u8, 15, 16]),
        ("gare", 49.6000, 6.1335, [14, 16, 0]),
        ("country", 49.70, 6.10, [10, 12, 0]),
        ("esch", 49.4958, 5.9806, [14, 0, 0]),
    ];
    let (w, h) = (800usize, 480usize);
    for (place, lat, lon, zooms) in places {
        let (x, y) = crate::world_from_degrees(lat, lon).unwrap();
        for zoom in zooms.into_iter().filter(|&z| z != 0) {
            let view = View { zoom, x, y };
            let layout = Layout::around(&view, (800, 480), 5, 3);
            let (cw, ch) = (5 * 256usize, 3 * 256usize);
            let (ox, oy) = layout.window(&view, (800, 480)).unwrap_or((240, 144));
            let mut out = vec![0u8; w * 3 * h * 3];
            for (k, night) in [false, true, true].into_iter().enumerate() {
                let tiles = if k == 2 { &mut both } else { &mut made };
                tiles.set_night(night);
                let mut canvas = vec![0u8; cw * ch * 3 / 2];
                tiles.compose(&layout, &mut canvas, cw, cw * ch);
                if let Some(n) = names.as_mut() {
                    crate::viewer::draw_names(
                        n,
                        &mut nv12::Surface {
                            pixels: &mut canvas,
                            pitch: cw,
                            chroma: cw * ch,
                            size: (cw as u32, ch as u32),
                        },
                        &layout,
                        (0, 0, cw as i64, ch as i64),
                        night,
                    );
                }
                let rgb = nv12_rgb(&canvas, cw, cw * ch, (ox as usize, oy as usize), (w, h));
                for row in 0..h {
                    let to = (row * 3 * w + k * w) * 3;
                    out[to..to + w * 3].copy_from_slice(&rgb[row * w * 3..(row + 1) * w * 3]);
                }
            }
            save_png(
                &shots.join(format!("night-{place}-z{zoom}.png")),
                &out,
                (3 * w as u32, h as u32),
            );
        }
    }
}

/// A region a PC built (`scripts/map-region.py`): its pack holds every
/// tile of the loose set it was packed from, byte for byte, and its files
/// match map.toml's sha256: `LEHU_REGION=<region> LEHU_LOOSE=<its .work>
/// cargo test -p lehu-map region_built_on_a_pc -- --ignored --nocapture`.
#[test]
#[ignore]
fn region_built_on_a_pc() {
    let region = PathBuf::from(std::env::var("LEHU_REGION").unwrap());
    let loose = PathBuf::from(std::env::var("LEHU_LOOSE").unwrap());
    let r = crate::region::Region::read(&region).unwrap();
    assert_eq!(r.health(None), crate::region::Health::Unchecked);
    let start = std::time::Instant::now();
    r.verify().unwrap();
    println!(
        "sha256 of {} files: {:?}",
        r.files().count(),
        start.elapsed()
    );
    let packed = TileSet::open(&region).unwrap();
    let plain = TileSet::open(&loose).unwrap();
    assert!(packed.is_packed() && !plain.is_packed());
    let (mut path, mut a, mut b) = (PathBuf::new(), Vec::new(), Vec::new());
    let mut n = 0;
    let start = std::time::Instant::now();
    for z in std::fs::read_dir(&loose).unwrap().flatten() {
        let Ok(zoom) = z.file_name().to_string_lossy().parse::<u8>() else {
            continue;
        };
        for xd in std::fs::read_dir(z.path()).unwrap().flatten() {
            let x: i64 = xd.file_name().to_string_lossy().parse().unwrap();
            for yf in std::fs::read_dir(xd.path()).unwrap().flatten() {
                let name = yf.file_name().to_string_lossy().into_owned();
                let y: i64 = name.trim_end_matches(".jpg").parse().unwrap();
                assert!(plain.read(zoom, x, y, &mut path, &mut a).unwrap());
                assert!(packed.read(zoom, x, y, &mut path, &mut b).unwrap());
                assert!(a == b, "{zoom}/{x}/{y}");
                n += 1;
            }
        }
    }
    println!("{n} tiles the same, read twice in {:?}", start.elapsed());
    assert!(n > 0);
    assert!(crate::labels::Names::open(&region).is_some());
}
