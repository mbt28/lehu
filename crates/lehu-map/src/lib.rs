// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The offline map: OpenStreetMap tiles read from the SD card, shown on the
//! video plane, scrolled in four directions and zoomed.
//!
//! The tiles are drawn on a PC (`scripts/map-tiles.py`) into
//! `<dir>/<z>/<x>/<y>.jpg`, 256 x 256, with `<dir>/map.toml` describing the
//! set. Here they are only decoded and copied: the board has no FPU, and a
//! JPEG is what its video engine decodes in hardware, in about a
//! millisecond a tile (software takes a quarter of a second).
//!
//! The picture is a canvas larger than the panel, composed from whole
//! tiles, of which the video plane shows a panel-sized window. Scrolling
//! moves the window, a commit and no copy; only when the window would leave
//! the canvas is a new one composed around it, from decoded tiles kept in
//! memory. Everything runs on a thread of its own, so the screens and the
//! input never wait for a decode. Coordinates are integers: world pixels
//! at zoom 22.
//!
//! Street and place names are either in the tiles or, for a set drawn with
//! `--labels data`, drawn here over them (crate::labels), so that they
//! stay the right way up when the map turns heading up.
//!
//! Along a guided drive, [`voice`] decides the spoken turn prompts, which
//! the program plays from clips made on a PC.

mod drive;
mod glyphs;
mod hw;
mod labels;
mod nv12;
mod sprite;
mod tiles;
mod tileset;
mod track;
mod view;
mod viewer;
pub mod voice;

pub use drive::Drive;
pub use tileset::TileSet;
pub use track::{Ahead, Fix, Maneuver, Track, Turn};
pub use view::{View, WORLD_Z, parse_position, world_from_degrees};
pub use viewer::{Marker, Route, Viewer};
pub use voice::{Clip, Voice};

#[cfg(test)]
mod tests;
