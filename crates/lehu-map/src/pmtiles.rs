// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A region's tiles in one file: PMTiles version 3
//! (<https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>), as
//! `scripts/map-tiles.py --pack` writes it: JPEG tiles, nothing
//! compressed, the tiles in tile id order.
//!
//! The header and the root directory are the first 16 KiB; they are read
//! once, at open, the root parsed into entries. A tile is found in the root,
//! or in the leaf directory the root points to (read and parsed when first
//! wanted, the last few kept), then read with one `pread`. Compared with a
//! file a tile, the card has no directory to walk and no 4 KiB block a
//! tile to spare, and a PC tool (`pmtiles show`, pmtiles.io) can look in.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The tiles' file in a region folder.
pub const FILE: &str = "tiles.pmtiles";

const HEADER: usize = 127;
/// The header and the root directory lie within this many bytes.
const ROOT_SPAN: usize = 16384;
/// A directory larger than this is not one we wrote (a leaf of 4096
/// entries takes some 30 KiB).
const MAX_DIR: u64 = 4 << 20;
/// Leaf directories kept parsed: a frame's tiles at a zoom, and the zooms
/// out they are enlarged from, are in a few.
const LEAVES: usize = 4;
/// Leaves inside leaves, at most (the format's own bound).
const DEPTH: usize = 3;

/// What `tile_type` says a tile is: JPEG.
const TYPE_JPEG: u8 = 3;
/// What `*_compression` says: none.
const COMPRESSION_NONE: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    /// Tiles from `tile_id` on with this same content; 0: a leaf
    /// directory, `offset` into the leaf section.
    run: u32,
}

struct Leaf {
    at: u64,
    used: u64,
    entries: Vec<Entry>,
}

/// An open tiles file.
pub struct Pack {
    file: File,
    path: PathBuf,
    root: Vec<Entry>,
    leaf_at: u64,
    data_at: u64,
    data_len: u64,
    leaves: Mutex<(Vec<Leaf>, u64)>,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pack").field("path", &self.path).finish()
    }
}

/// PMTiles' tile id of tile (`x`, `y`) of zoom `z`: the tiles of all the
/// zooms before, then its place along the zoom's Hilbert curve.
pub fn tile_id(z: u8, x: u32, y: u32) -> u64 {
    let z = u32::from(z.min(31));
    let mut acc = ((1u64 << (2 * z)) - 1) / 3;
    let n = 1u64 << z;
    let (mut x, mut y) = (u64::from(x), u64::from(y));
    let mut s = n >> 1;
    while s > 0 {
        let rx = u64::from(x & s != 0);
        let ry = u64::from(y & s != 0);
        acc += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    acc
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

/// A varint from `b` at `*at`, moved past it.
fn varint(b: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*at).ok_or("a directory ends early")?;
        *at += 1;
        if shift >= 64 {
            return Err("a number too long in a directory".into());
        }
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

/// A directory's entries, uncompressed as we write them.
fn parse_dir(b: &[u8]) -> Result<Vec<Entry>, String> {
    let mut at = 0;
    let n = varint(b, &mut at)? as usize;
    // Each entry takes four bytes at least.
    if n > b.len() / 4 + 1 {
        return Err("a directory with more entries than bytes".into());
    }
    let mut entries = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run: 0
        };
        n
    ];
    let mut last = 0u64;
    for e in &mut entries {
        last = last
            .checked_add(varint(b, &mut at)?)
            .ok_or("tile ids overflow")?;
        e.tile_id = last;
    }
    for e in &mut entries {
        e.run = u32::try_from(varint(b, &mut at)?).map_err(|_| "a run too long")?;
    }
    for e in &mut entries {
        e.length = u32::try_from(varint(b, &mut at)?).map_err(|_| "an entry too long")?;
    }
    for i in 0..n {
        let v = varint(b, &mut at)?;
        entries[i].offset = if v == 0 && i > 0 {
            entries[i - 1].offset + u64::from(entries[i - 1].length)
        } else {
            v.checked_sub(1).ok_or("an offset of 0 first")?
        };
    }
    Ok(entries)
}

/// The entry for `id`: its own, or the leaf or run it falls in.
fn find(entries: &[Entry], id: u64) -> Option<Entry> {
    let i = entries
        .partition_point(|e| e.tile_id <= id)
        .checked_sub(1)?;
    let e = entries[i];
    if e.run == 0 || id - e.tile_id < u64::from(e.run) {
        Some(e)
    } else {
        None
    }
}

impl Pack {
    /// The file at `path`, checked to be one LEHU reads: version 3, JPEG,
    /// nothing compressed.
    pub fn open(path: &Path) -> Result<Self, String> {
        let at = |e: String| format!("{}: {e}", path.display());
        let file = File::open(path).map_err(|e| at(e.to_string()))?;
        let len = file.metadata().map_err(|e| at(e.to_string()))?.len();
        let span = ROOT_SPAN.min(len as usize);
        if span < HEADER {
            return Err(at("too short for a tiles file".into()));
        }
        let mut head = vec![0u8; span];
        file.read_exact_at(&mut head, 0)
            .map_err(|e| at(e.to_string()))?;
        if &head[..7] != b"PMTiles" {
            return Err(at("not a PMTiles file".into()));
        }
        if head[7] != 3 {
            return Err(at(format!("PMTiles version {}: 3 only", head[7])));
        }
        let (root_at, root_len) = (u64_at(&head, 8), u64_at(&head, 16));
        let (leaf_at, leaf_len) = (u64_at(&head, 40), u64_at(&head, 48));
        let (data_at, data_len) = (u64_at(&head, 56), u64_at(&head, 64));
        let (internal, tiles, kind) = (head[97], head[98], head[99]);
        if internal != COMPRESSION_NONE || tiles != COMPRESSION_NONE {
            return Err(at(
                "compressed directories or tiles: pack it with scripts/map-tiles.py".into(),
            ));
        }
        if kind != TYPE_JPEG {
            return Err(at(format!("tile type {kind}: JPEG only")));
        }
        let end = |a: u64, l: u64| a.checked_add(l).filter(|&e| e <= len);
        if end(leaf_at, leaf_len).is_none() || end(data_at, data_len).is_none() {
            return Err(at("its sections run past its end (cut short?)".into()));
        }
        let root_end = end(root_at, root_len)
            .filter(|&e| e as usize <= span)
            .ok_or_else(|| at("the root directory is not in the first 16 KiB".into()))?;
        let root = parse_dir(&head[root_at as usize..root_end as usize]).map_err(at)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            root,
            leaf_at,
            data_at,
            data_len,
            leaves: Mutex::new((Vec::with_capacity(LEAVES), 0)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tile's place in the data section: None when the file has none.
    fn locate(&self, id: u64) -> io::Result<Option<(u64, u32)>> {
        let mut found = find(&self.root, id);
        for _ in 0..DEPTH {
            match found {
                None => return Ok(None),
                Some(e) if e.run > 0 => return Ok(Some((e.offset, e.length))),
                Some(leaf) => found = self.in_leaf(leaf, id)?,
            }
        }
        Err(io::Error::other("leaf directories too deep"))
    }

    /// `id` looked up in a leaf directory, read when it is not kept.
    fn in_leaf(&self, leaf: Entry, id: u64) -> io::Result<Option<Entry>> {
        let mut guard = self
            .leaves
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (leaves, clock) = &mut *guard;
        *clock += 1;
        if let Some(l) = leaves.iter_mut().find(|l| l.at == leaf.offset) {
            l.used = *clock;
            return Ok(find(&l.entries, id));
        }
        if u64::from(leaf.length) > MAX_DIR {
            return Err(io::Error::other("a leaf directory too large"));
        }
        let mut buf = vec![0u8; leaf.length as usize];
        self.file
            .read_exact_at(&mut buf, self.leaf_at + leaf.offset)?;
        let entries = parse_dir(&buf).map_err(io::Error::other)?;
        let found = find(&entries, id);
        let fresh = Leaf {
            at: leaf.offset,
            used: *clock,
            entries,
        };
        if leaves.len() < LEAVES {
            leaves.push(fresh);
        } else if let Some(old) = leaves.iter_mut().min_by_key(|l| l.used) {
            *old = fresh;
        }
        Ok(found)
    }

    /// Tile (`x`, `y`) of zoom `z` into `buf`; false when the file has
    /// none there. At most `max` bytes.
    pub fn read(&self, z: u8, x: u32, y: u32, buf: &mut Vec<u8>, max: u64) -> io::Result<bool> {
        if z > 31 || u64::from(x) >= 1u64 << z || u64::from(y) >= 1u64 << z {
            return Ok(false);
        }
        let Some((offset, length)) = self.locate(tile_id(z, x, y))? else {
            return Ok(false);
        };
        if u64::from(length) > max {
            return Err(io::Error::other("tile too large"));
        }
        if offset + u64::from(length) > self.data_len {
            return Err(io::Error::other("tile past the end of the data"));
        }
        buf.clear();
        buf.resize(length as usize, 0);
        self.file.read_exact_at(buf, self.data_at + offset)?;
        Ok(true)
    }

    /// Leaf directories read so far are kept: how many (for tests).
    #[cfg(test)]
    pub(crate) fn leaves_kept(&self) -> usize {
        self.leaves.lock().map_or(0, |g| g.0.len())
    }
}

#[cfg(test)]
pub(crate) mod write {
    //! A PMTiles file as `scripts/map-tiles.py --pack` writes one, for the
    //! tests: the same layout, built in memory.

    use super::{HEADER, ROOT_SPAN, tile_id};

    /// A tile to pack: (z, x, y) and its bytes.
    pub(crate) type Tile = ((u8, u32, u32), Vec<u8>);

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// (tile id, offset, length, run)
    fn dir(entries: &[(u64, u64, u64, u64)]) -> Vec<u8> {
        let mut b = Vec::new();
        varint(&mut b, entries.len() as u64);
        let mut last = 0;
        for e in entries {
            varint(&mut b, e.0 - last);
            last = e.0;
        }
        for e in entries {
            varint(&mut b, e.3);
        }
        for e in entries {
            varint(&mut b, e.2);
        }
        for (i, e) in entries.iter().enumerate() {
            if i > 0 && e.1 == entries[i - 1].1 + entries[i - 1].2 {
                varint(&mut b, 0);
            } else {
                varint(&mut b, e.1 + 1);
            }
        }
        b
    }

    /// The file holding `tiles` ((z, x, y), bytes); `leaf`: entries a leaf
    /// directory, 0 for a root directory only (when it fits).
    pub(crate) fn pmtiles(tiles: &[Tile], leaf: usize) -> Vec<u8> {
        let mut sorted: Vec<(u64, &[u8], u8)> = tiles
            .iter()
            .map(|((z, x, y), b)| (tile_id(*z, *x, *y), b.as_slice(), *z))
            .collect();
        sorted.sort_by_key(|t| t.0);
        let mut data = Vec::new();
        let mut entries: Vec<(u64, u64, u64, u64)> = Vec::new();
        let mut seen: Vec<(&[u8], u64)> = Vec::new();
        for (id, body, _) in &sorted {
            let off = match seen.iter().find(|s| s.0 == *body) {
                Some(s) => s.1,
                None => {
                    let off = data.len() as u64;
                    data.extend_from_slice(body);
                    seen.push((body, off));
                    off
                }
            };
            match entries.last_mut() {
                Some(l) if l.0 + l.3 == *id && l.1 == off => l.3 += 1,
                _ => entries.push((*id, off, body.len() as u64, 1)),
            }
        }
        let (root, leaves) = if leaf == 0 {
            (dir(&entries), Vec::new())
        } else {
            let mut leaves = Vec::new();
            let mut top = Vec::new();
            for chunk in entries.chunks(leaf) {
                let d = dir(chunk);
                top.push((chunk[0].0, leaves.len() as u64, d.len() as u64, 0));
                leaves.extend_from_slice(&d);
            }
            (dir(&top), leaves)
        };
        assert!(HEADER + root.len() <= ROOT_SPAN);
        let meta = b"{}";
        let md_at = (HEADER + root.len()) as u64;
        let leaf_at = md_at + meta.len() as u64;
        let data_at = leaf_at + leaves.len() as u64;
        let mut h = b"PMTiles\x03".to_vec();
        for v in [
            HEADER as u64,
            root.len() as u64,
            md_at,
            meta.len() as u64,
            leaf_at,
            leaves.len() as u64,
            data_at,
            data.len() as u64,
            sorted.len() as u64,
            entries.len() as u64,
            seen.len() as u64,
        ] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        let zs: Vec<u8> = sorted.iter().map(|t| t.2).collect();
        h.extend_from_slice(&[
            1,
            1,
            1,
            3,
            zs.iter().copied().min().unwrap_or(0),
            zs.iter().copied().max().unwrap_or(0),
        ]);
        for _ in 0..4 {
            h.extend_from_slice(&0i32.to_le_bytes());
        }
        h.push(0);
        h.extend_from_slice(&0i32.to_le_bytes());
        h.extend_from_slice(&0i32.to_le_bytes());
        assert_eq!(h.len(), HEADER);
        [h, root, meta.to_vec(), leaves, data].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::write::{Tile, pmtiles};
    use super::{LEAVES, Pack, tile_id};

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lehu-pmtiles-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tile_ids_are_the_formats_own() {
        // From the format's reference implementation.
        for ((z, x, y), id) in [
            ((0, 0, 0), 0),
            ((1, 0, 0), 1),
            ((1, 0, 1), 2),
            ((1, 1, 1), 3),
            ((1, 1, 0), 4),
            ((2, 0, 0), 5),
            ((12, 2120, 1388), 19_849_701),
            ((16, 33930, 22296), 5_081_710_819),
            ((20, 543_000, 357_000), 1_300_918_124_159),
            ((22, 4_194_303, 0), 23_456_248_059_220),
        ] {
            assert_eq!(tile_id(z, x, y), id, "{z}/{x}/{y}");
        }
    }

    /// Tiles of zooms 3 to 6 over a corner, each a few bytes naming it;
    /// some the same as others (runs, and contents shared).
    pub(crate) fn some_tiles() -> Vec<Tile> {
        let mut tiles = Vec::new();
        for z in 3u8..=6 {
            let n = 1u32 << (z - 2);
            for x in 0..n {
                for y in 0..n {
                    let body = if z == 6 && x < 4 {
                        b"sea".to_vec()
                    } else {
                        format!("tile {z}/{x}/{y}").into_bytes()
                    };
                    tiles.push(((z, x, y), body));
                }
            }
        }
        tiles
    }

    fn check(file: &[u8], tiles: &[Tile], name: &str) -> Pack {
        let path = temp(name).join("tiles.pmtiles");
        std::fs::write(&path, file).unwrap();
        let pack = Pack::open(&path).unwrap();
        let mut buf = Vec::new();
        for ((z, x, y), body) in tiles {
            assert!(
                pack.read(*z, *x, *y, &mut buf, 1 << 20).unwrap(),
                "{z}/{x}/{y}"
            );
            assert_eq!(&buf, body, "{z}/{x}/{y}");
        }
        // Missing: past the corner, other zooms, outside the world.
        for (z, x, y) in [
            (3, 7, 7),
            (2, 0, 0),
            (7, 0, 0),
            (6, 16, 0),
            (3, 8, 0),
            (40, 0, 0),
        ] {
            assert!(
                !pack.read(z, x, y, &mut buf, 1 << 20).unwrap(),
                "{z}/{x}/{y}"
            );
        }
        // Not past the bound given.
        assert!(pack.read(3, 0, 0, &mut buf, 3).is_err());
        pack
    }

    #[test]
    fn a_pack_gives_back_every_tile_from_its_root_directory() {
        let tiles = some_tiles();
        let pack = check(&pmtiles(&tiles, 0), &tiles, "root");
        assert_eq!(pack.leaves_kept(), 0);
    }

    #[test]
    fn a_pack_gives_back_every_tile_through_leaf_directories() {
        let tiles = some_tiles();
        // Leaves of 3 entries: dozens of them, more than are kept.
        let pack = check(&pmtiles(&tiles, 3), &tiles, "leaves");
        assert_eq!(pack.leaves_kept(), LEAVES);
    }

    #[test]
    fn a_pack_cut_short_or_of_another_kind_does_not_open() {
        let file = pmtiles(&some_tiles(), 3);
        let path = temp("bad").join("tiles.pmtiles");
        let open = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            Pack::open(&path).map(|_| ()).unwrap_err()
        };
        assert!(open(&file[..file.len() - 1]).contains("cut short"));
        assert!(open(&file[..100]).contains("too short"));
        assert!(open(&[b'x'; 200]).contains("not a PMTiles"));
        let mut gz = file.clone();
        gz[97] = 2; // gzip directories
        assert!(open(&gz).contains("compressed"));
        let mut png = file.clone();
        png[99] = 2;
        assert!(open(&png).contains("JPEG only"));
        let mut v2 = file;
        v2[7] = 2;
        assert!(open(&v2).contains("version 2"));
    }
}
