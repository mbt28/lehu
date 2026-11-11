// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The speed cameras on the road network, in a small file of their own,
//! `cameras.bin`, beside `roads.bin`: they change far more often than the
//! roads, so they can be replaced alone. scripts/map-graph.py writes both.
//!
//! The file is little-endian 32-bit words: "LEHUCAMS", the version (1),
//! the count of cameras, and the ID of the `roads.bin` build they were
//! placed on (two words, low first; roads.bin's header has the same).
//! Then the cameras, 6 words each: x, y (world pixels at zoom 22, on the
//! road), the edge, decimetres from its a, the ways it watches (1 a to b,
//! 2 b to a, 3 both), its speed limit in km/h (0 not known); in the order
//! of their edges, and along each from a.
//!
//! A camera names an edge of one build of the network, so a cameras.bin
//! from another build is ignored (with a line in the log). Road data of
//! version 2 has its cameras inside, in the same layout; they are read
//! from there instead.

use std::path::Path;

use crate::blob::Blob;
use crate::graph::Graph;

/// The file's name in the map's directory.
pub const CAMERAS_FILE: &str = "cameras.bin";

const MAGIC: &[u8; 8] = b"LEHUCAMS";
const VERSION: u32 = 1;
const HEADER: usize = 6;
pub(crate) const CAMERA: usize = 6;

/// A speed camera, on an edge: where, how far from the edge's a, which ways
/// along it it watches, and the speed limit it checks when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Camera {
    pub x: i64,
    pub y: i64,
    pub edge: u32,
    pub from_a_dm: u32,
    /// It watches the traffic from a to b, from b to a.
    pub forward: bool,
    pub backward: bool,
    pub limit: Option<u16>,
}

/// The cameras of one road network, checked against it.
pub struct Cameras {
    blob: Blob,
    at: usize,
    count: usize,
}

impl Cameras {
    /// `cameras.bin` in `dir` for `graph`: None when there is none, or
    /// when it is broken or for another build of the roads (each said in
    /// the log). Road data of version 2 brings its own.
    pub fn open(dir: &Path, graph: &Graph) -> Option<Self> {
        if let Some(words) = graph.v2_cameras() {
            return Self::from_words(words, graph).ok();
        }
        let path = dir.join(CAMERAS_FILE);
        if !path.exists() {
            log::info!("map: no {CAMERAS_FILE}, no speed cameras");
            return None;
        }
        match Blob::map(&path).and_then(|b| Self::new(b, graph)) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("map: {}: {e}; no speed cameras", path.display());
                None
            }
        }
    }

    /// A file's bytes, for `graph` (tests).
    pub fn from_bytes(bytes: &[u8], graph: &Graph) -> Result<Self, String> {
        Self::new(Blob::from_bytes(bytes)?, graph)
    }

    /// The cameras of a version 2 roads.bin, as its section has them.
    fn from_words(words: Vec<u32>, graph: &Graph) -> Result<Self, String> {
        let count = words.len() / CAMERA;
        let c = Self {
            blob: Blob::Owned(words),
            at: 0,
            count,
        };
        c.check(graph)?;
        Ok(c)
    }

    fn new(blob: Blob, graph: &Graph) -> Result<Self, String> {
        let w = blob.words();
        if w.len() < HEADER || blob.bytes(0, 8) != Some(&MAGIC[..]) {
            return Err("not LEHU speed cameras".into());
        }
        if w[2] != VERSION {
            return Err(format!("version {}, LEHU reads {VERSION}", w[2]));
        }
        let count = w[3] as usize;
        if HEADER + count * CAMERA != w.len() {
            return Err("its count and its length disagree".into());
        }
        let id = u64::from(w[4]) | u64::from(w[5]) << 32;
        match graph.build_id() {
            Some(roads) if roads == id => {}
            Some(roads) => {
                return Err(format!(
                    "placed on roads.bin {id:016x}, not on this one ({roads:016x})"
                ));
            }
            None => return Err("this roads.bin has no build ID to match".into()),
        }
        let c = Self {
            blob,
            at: HEADER,
            count,
        };
        c.check(graph)?;
        Ok(c)
    }

    /// Every camera is on an edge of the network, and in order.
    fn check(&self, graph: &Graph) -> Result<(), String> {
        let mut last = (0, 0);
        for i in 0..self.count {
            let c = self.words(i);
            if c[2] as usize >= graph.edges() || c[0] >= 1 << 30 || c[1] >= 1 << 30 {
                return Err(format!("camera {i}: out of range"));
            }
            if (c[2], c[3]) < last {
                return Err(format!("camera {i}: out of order"));
            }
            last = (c[2], c[3]);
        }
        Ok(())
    }

    fn words(&self, i: usize) -> &[u32] {
        let at = self.at + i * CAMERA;
        &self.blob.words()[at..at + CAMERA]
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Camera `i`, in the order of their edges.
    pub fn camera(&self, i: usize) -> Camera {
        let c = self.words(i);
        let limit = (c[5] & 0xFFFF) as u16;
        Camera {
            x: i64::from(c[0]),
            y: i64::from(c[1]),
            edge: c[2],
            from_a_dm: c[3],
            forward: c[4] & 1 != 0,
            backward: c[4] & 2 != 0,
            limit: (limit > 0).then_some(limit),
        }
    }

    /// The cameras on edge `e`, from its a.
    pub fn on(&self, e: u32) -> impl Iterator<Item = Camera> + '_ {
        let edge_of = |i: usize| self.words(i)[2];
        // The first at or after the edge: they are in its order.
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if edge_of(mid) < e {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo..self.count)
            .take_while(move |&i| edge_of(i) == e)
            .map(|i| self.camera(i))
    }
}

/// A cameras.bin's bytes, as scripts/map-graph.py writes it (tests).
#[cfg(test)]
pub(crate) fn write(build_id: u64, cameras: &[[u32; CAMERA]]) -> Vec<u8> {
    let mut w = vec![0, 0, VERSION, cameras.len() as u32];
    w.extend([build_id as u32, (build_id >> 32) as u32]);
    w.extend(cameras.iter().flatten());
    let mut bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
    bytes[..8].copy_from_slice(MAGIC);
    bytes
}
