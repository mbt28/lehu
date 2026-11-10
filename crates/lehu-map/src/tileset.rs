// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A directory of tiles and its `map.toml`.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::view::WORLD_Z;

/// A tile file larger than this is not a tile (the decoder's own bound).
const MAX_TILE_BYTES: u64 = lehu_image::MAX_INPUT as u64;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Meta {
    name: String,
    format: String,
    tile_size: u32,
    min_zoom: u8,
    max_zoom: u8,
    bounds: [i64; 4],
    center: [i64; 2],
    start_zoom: Option<u8>,
    attribution: String,
}

/// The tiles under one directory, as its `map.toml` describes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileSet {
    dir: PathBuf,
    ext: &'static str,
    pub name: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// The area, world pixels at `WORLD_Z`: left, top, right, bottom.
    pub bounds: [i64; 4],
    /// Where to open, world pixels at `WORLD_Z`.
    pub center: (i64, i64),
    pub start_zoom: u8,
    pub attribution: String,
}

impl TileSet {
    /// The tiles are JPEG files (else PNG).
    pub fn is_jpeg(&self) -> bool {
        self.ext == "jpg"
    }

    /// Read `<dir>/map.toml`. The error says what is wrong, for the screen.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let path = dir.join("map.toml");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(format!("No map in {}", dir.display()));
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Self::parse(dir, &text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub(crate) fn parse(dir: &Path, text: &str) -> Result<Self, String> {
        let meta: Meta = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let ext = match meta.format.as_str() {
            "jpg" | "jpeg" => "jpg",
            "png" => "png",
            other => return Err(format!("format {other:?}: jpg or png")),
        };
        if meta.tile_size != 256 {
            return Err(format!("tile-size {}: only 256", meta.tile_size));
        }
        if meta.min_zoom > meta.max_zoom || meta.max_zoom > WORLD_Z {
            return Err(format!(
                "zoom {}-{}: 0-{WORLD_Z}, first to last",
                meta.min_zoom, meta.max_zoom
            ));
        }
        let world = 256i64 << WORLD_Z;
        let [l, t, r, b] = meta.bounds;
        if !(0 <= l && l < r && r <= world && 0 <= t && t < b && b <= world) {
            return Err("bounds: left < right, top < bottom, inside the world".into());
        }
        let start = meta
            .start_zoom
            .unwrap_or(meta.min_zoom)
            .clamp(meta.min_zoom, meta.max_zoom);
        Ok(Self {
            dir: dir.to_path_buf(),
            ext,
            name: meta.name,
            min_zoom: meta.min_zoom,
            max_zoom: meta.max_zoom,
            bounds: meta.bounds,
            center: (meta.center[0].clamp(l, r), meta.center[1].clamp(t, b)),
            start_zoom: start,
            attribution: meta.attribution,
        })
    }

    /// Tile (`x`, `y`) of zoom `z` into `buf`; false when the set has none
    /// there. `path` is scratch, reused.
    pub(crate) fn read(
        &self,
        z: u8,
        x: i64,
        y: i64,
        path: &mut PathBuf,
        buf: &mut Vec<u8>,
    ) -> io::Result<bool> {
        path.clear();
        path.push(&self.dir);
        path.push(z.to_string());
        path.push(x.to_string());
        path.push(format!("{y}.{}", self.ext));
        let file = match fs::File::open(&*path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        buf.clear();
        let read = file.take(MAX_TILE_BYTES + 1).read_to_end(buf)?;
        if read as u64 > MAX_TILE_BYTES {
            return Err(io::Error::other("tile too large"));
        }
        Ok(true)
    }
}
