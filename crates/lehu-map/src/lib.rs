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
//!
//! Destinations are searched by name and routes found on the board too,
//! from a road network and a list of names written on a PC
//! (`scripts/map-graph.py`) and mapped from the card: see [`Router`].

mod ahead;
mod blob;
mod cameras;
mod drive;
mod follow;
mod glyphs;
mod graph;
mod hw;
mod labels;
mod nv12;
mod pmtiles;
pub mod region;
mod route;
mod search;
mod sprite;
mod tiles;
mod tileset;
mod track;
mod view;
mod viewer;
pub mod voice;

pub use ahead::{CameraAhead, CameraLookout};
pub use cameras::{CAMERAS_FILE, Camera, Cameras};
pub use drive::Drive;
pub use follow::{ARRIVE_M, ARRIVE_NEAR_M, Follow, Following, OFF_M, OFF_MS, WRONG_WAY_MS};
pub use graph::{Graph, NEAR_M, ROADS_FILE, RouteBy, RouteOptions, Snap};
pub use route::Router;
pub use search::{Found, FoundKind, PLACES_FILE, Places, fold};
pub use sprite::MarkerPlane;
pub use tileset::TileSet;
pub use track::{
    Ahead, Fix, Leg, LegCamera, LegStep, Located, Maneuver, RouteCamera, Track, Turn,
    metres_between,
};
pub use view::{View, WORLD_Z, degrees_from_world, parse_position, world_from_degrees};
pub use viewer::{CAMERA_ZOOM, CameraMarks, Marker, Route, Turning, Viewer};
pub use voice::{CameraWatch, Clip, Voice, camera_lead};

#[cfg(test)]
mod routing_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod tests;
