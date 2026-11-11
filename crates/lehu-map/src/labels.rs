// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Street and place names the map draws itself, so that they read the
//! right way up however the map is turned, at the size the panel wants.
//!
//! A tile set drawn with `scripts/map-tiles.py --labels data` has no names
//! in its tiles: they are in `<map>/labels/<z>/<bx>/<by>.txt`, a file a
//! block of 8 x 8 tiles (`bx`, `by`: the tile's x and y divided by 8). Each
//! line is a label, its fields apart by tabs:
//!
//! ```text
//! kind  x  y  angle  size  text
//! ```
//!
//! `kind` is `s` (a street) or `p` (a place); `x`, `y` the middle of its box
//! in pixels of the zoom; `angle` the street's way in degrees, clockwise
//! from east; `size` the text's size in pixels; `text` the name. Lines
//! starting with `#` are comments. The script placed them so that none
//! overlaps another, each in the block of its middle and reaching no
//! further than `REACH` pixels from it. A block without labels has no
//! file; `<z>/areas.txt` lists the blocks the script covered, a line
//! `bx0 by0 bx1 by1` (first to last) a pass. A block a zoom has not
//! covered has the labels of a zoom or two out (fewer, as far apart again).
//!
//! A region packed by `scripts/map-tiles.py --pack` has the same files in
//! one, `<map>/labels.pack` ([`PACK`]), mapped and read where they lie: an
//! index of the blocks and the zooms' areas, then their text as it is. A
//! set with neither has its names in the tiles: nothing is drawn.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::blob::Blob;
use crate::glyphs::{Glyphs, HALO};
use crate::nv12::{self, Surface};
use crate::view::{TILE, WORLD_Z};

/// A label file holds 2^BLOCK x 2^BLOCK tiles.
const BLOCK: u32 = 3;

/// No part of a label is further than this from its middle, in pixels.
pub const REACH: i64 = 512;

/// Blocks kept read: more than a frame and the reach round it take (three
/// by three).
const BLOCKS: usize = 16;

/// Zooms out a missing block's labels are looked for.
const FALLBACK: u8 = 2;

/// The largest file read as a block.
const MAX_BYTES: u64 = 256 * 1024;

/// The label files packed in one, in a region folder.
pub const PACK: &str = "labels.pack";
const PACK_MAGIC: &[u8; 8] = b"LEHULABP";
const PACK_VERSION: u32 = 1;
/// Words before the index, and in each of its entries.
const PACK_HEAD: usize = 4;
const PACK_ENTRY: usize = 5;
/// An index entry's kind: the zoom's areas.txt (else a block).
const PACK_AREAS: u32 = 256;

/// `labels.pack` mapped: "LEHULABP", the version, the number of entries;
/// the entries, five words each, in order (z + 256 for a zoom's areas,
/// bx, by, the text's byte offset in the file, its length); then the text.
pub(crate) struct Pack {
    blob: Blob,
    entries: usize,
}

impl Pack {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let blob = Blob::map(path)?;
        Self::new(blob).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub(crate) fn new(blob: Blob) -> Result<Self, String> {
        let words = blob.words();
        if words.len() < PACK_HEAD || blob.byte_range(0, 8) != Some(PACK_MAGIC.as_slice()) {
            return Err("not a labels pack".into());
        }
        if words[2] != PACK_VERSION {
            return Err(format!(
                "labels pack version {}: {PACK_VERSION} only",
                words[2]
            ));
        }
        let entries = words[3] as usize;
        if entries > (words.len() - PACK_HEAD) / PACK_ENTRY {
            return Err("its index runs past its end".into());
        }
        Ok(Self { blob, entries })
    }

    fn entry(&self, i: usize) -> &[u32] {
        let at = PACK_HEAD + i * PACK_ENTRY;
        &self.blob.words()[at..at + PACK_ENTRY]
    }

    /// The text of entry (`kind`, `bx`, `by`): None when there is none,
    /// an error when it does not read.
    fn text(&self, kind: u32, bx: i64, by: i64) -> Option<Result<&str, String>> {
        let (Ok(bx), Ok(by)) = (u32::try_from(bx), u32::try_from(by)) else {
            return None;
        };
        let key = (kind, bx, by);
        let (mut lo, mut hi) = (0, self.entries);
        while lo < hi {
            let mid = usize::midpoint(lo, hi);
            let e = self.entry(mid);
            match (e[0], e[1], e[2]).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let (at, len) = (e[3] as usize, e[4] as usize);
                    if len as u64 > MAX_BYTES {
                        return Some(Err("a block too large".into()));
                    }
                    return Some(
                        self.blob
                            .byte_range(at, len)
                            .ok_or_else(|| "a block past the end".to_string())
                            .and_then(|b| {
                                std::str::from_utf8(b).map_err(|_| "a block not UTF-8".into())
                            }),
                    );
                }
            }
        }
        None
    }
}

/// Where a set's label files are.
enum Source {
    /// `<map>/labels/`, a file a block.
    Dir(PathBuf),
    /// `<map>/labels.pack`.
    Pack(Pack),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Street,
    Place,
}

/// One name, where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub kind: Kind,
    /// The middle of its box: world pixels at `WORLD_Z`.
    pub at: (i64, i64),
    /// The way its text runs on a north-up map, 65536 long (x right,
    /// y down); east for a place.
    pub way: (i64, i64),
    /// The text's size, pixels.
    pub size: u8,
    pub text: Box<str>,
}

/// The labels in a block file of zoom `zoom`; lines it cannot read are
/// left out.
pub fn parse(text: &str, zoom: u8) -> Vec<Label> {
    let s = WORLD_Z - zoom.min(WORLD_Z);
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.splitn(6, '\t');
            let kind = match f.next()? {
                "s" => Kind::Street,
                "p" => Kind::Place,
                _ => return None,
            };
            let x: i64 = f.next()?.parse().ok()?;
            let y: i64 = f.next()?.parse().ok()?;
            let angle: i32 = f.next()?.parse().ok()?;
            let size: u8 = f.next()?.parse().ok()?;
            let text = f.next()?.trim();
            let world = 256i64 << zoom.min(WORLD_Z);
            if !(0..world).contains(&x)
                || !(0..world).contains(&y)
                || !(-360..=360).contains(&angle)
                || !(6..=64).contains(&size)
                || text.is_empty()
            {
                return None;
            }
            let way = match kind {
                Kind::Place => (65536, 0),
                Kind::Street => {
                    // Once a label, as it is read: never a pixel at a time.
                    let r = f64::from(angle).to_radians();
                    (
                        (r.cos() * 65536.0).round() as i64,
                        (r.sin() * 65536.0).round() as i64,
                    )
                }
            };
            Some(Label {
                kind,
                at: (x << s, y << s),
                way,
                size,
                text: text.into(),
            })
        })
        .collect()
}

/// A text's way turned half round when it would read upside down: right
/// along the panel, or straight down it.
pub fn upright((x, y): (i64, i64)) -> (i64, i64) {
    if x < 0 || (x == 0 && y < 0) {
        (-x, -y)
    } else {
        (x, y)
    }
}

type Key = (u8, i64, i64);

struct Block {
    key: Key,
    used: u64,
    labels: Vec<Label>,
}

/// Blocks covered, first to last both ways: (bx0, by0, bx1, by1).
type Area = (i64, i64, i64, i64);

/// A zoom's `areas.txt`: the lines it can read.
fn parse_areas(text: &str) -> Vec<Area> {
    text.lines()
        .filter_map(|l| {
            let v: Vec<i64> = l
                .split_whitespace()
                .map(|f| f.parse().ok())
                .collect::<Option<_>>()?;
            match v[..] {
                [a, b, c, d] => Some((a, b, c, d)),
                _ => None,
            }
        })
        .collect()
}

/// The label files of a set, read as they are wanted and kept a while.
pub struct Store {
    source: Source,
    blocks: Vec<Block>,
    /// Each zoom's areas, read when first wanted.
    areas: Vec<(u8, Vec<Area>)>,
    clock: u64,
    /// Files read, and those that would not.
    pub read: u32,
    pub failed: u32,
}

impl Store {
    /// The labels of the set in `map`, when it has them: packed, else a
    /// file a block.
    pub fn open(map: &Path) -> Option<Self> {
        let packed = map.join(PACK);
        let source = if packed.exists() {
            match Pack::open(&packed) {
                Ok(p) => Source::Pack(p),
                Err(e) => {
                    log::warn!("map: {e}; the names are not drawn");
                    return None;
                }
            }
        } else {
            let dir = map.join("labels");
            if !dir.is_dir() {
                return None;
            }
            Source::Dir(dir)
        };
        Some(Self {
            source,
            blocks: Vec::with_capacity(BLOCKS),
            areas: Vec::new(),
            clock: 0,
            read: 0,
            failed: 0,
        })
    }

    /// The file of block (`bx`, `by`) of zoom `z`: None when there is
    /// none.
    fn file(&mut self, (z, bx, by): Key) -> Option<Vec<Label>> {
        let dir = match &self.source {
            Source::Dir(dir) => dir,
            Source::Pack(pack) => {
                return match pack.text(u32::from(z), bx, by)? {
                    Ok(text) => {
                        self.read += 1;
                        Some(parse(text, z))
                    }
                    Err(e) => {
                        self.failed += 1;
                        log::warn!("map: labels {z}/{bx}/{by}: {e}");
                        Some(Vec::new())
                    }
                };
            }
        };
        let path = dir
            .join(z.to_string())
            .join(bx.to_string())
            .join(format!("{by}.txt"));
        let text = match fs::metadata(&path).and_then(|m| {
            if m.len() > MAX_BYTES {
                Err(io::Error::other("too large"))
            } else {
                fs::read_to_string(&path)
            }
        }) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                self.failed += 1;
                log::warn!("map: {}: {e}", path.display());
                return Some(Vec::new());
            }
        };
        self.read += 1;
        Some(parse(&text, z))
    }

    /// Block (`bx`, `by`) of zoom `z` was covered: its labels are its
    /// file's, none without one.
    fn covered(&mut self, (z, bx, by): Key) -> bool {
        let i = match self.areas.iter().position(|a| a.0 == z) {
            Some(i) => i,
            None => {
                let areas = match &self.source {
                    Source::Dir(dir) => {
                        fs::read_to_string(dir.join(z.to_string()).join("areas.txt"))
                            .map_or_else(|_| Vec::new(), |t| parse_areas(&t))
                    }
                    Source::Pack(pack) => match pack.text(PACK_AREAS | u32::from(z), 0, 0) {
                        Some(Ok(t)) => parse_areas(t),
                        _ => Vec::new(),
                    },
                };
                self.areas.push((z, areas));
                self.areas.len() - 1
            }
        };
        self.areas[i]
            .1
            .iter()
            .any(|a| (a.0..=a.2).contains(&bx) && (a.1..=a.3).contains(&by))
    }

    /// A block's labels: its file's, or those of the block a zoom or two
    /// out that lie in it.
    fn load(&mut self, (z, bx, by): Key) -> Vec<Label> {
        let span = |z: u8| (TILE << BLOCK) << (WORLD_Z - z.min(WORLD_Z));
        let side = span(z);
        let (left, top) = (bx * side, by * side);
        for k in 0..=FALLBACK.min(z) {
            let key = (z - k, bx >> k, by >> k);
            let found = self.file(key);
            if found.is_none() && self.covered(key) {
                return Vec::new();
            }
            if let Some(labels) = found {
                if k == 0 {
                    return labels;
                }
                return labels
                    .into_iter()
                    .filter(|l| {
                        (left..left + side).contains(&l.at.0) && (top..top + side).contains(&l.at.1)
                    })
                    .collect();
            }
        }
        Vec::new()
    }

    fn slot(&mut self, key: Key) -> usize {
        self.clock += 1;
        let i = match self.blocks.iter().position(|b| b.key == key) {
            Some(i) => i,
            None => {
                let block = Block {
                    key,
                    used: 0,
                    labels: self.load(key),
                };
                if self.blocks.len() < BLOCKS {
                    self.blocks.push(block);
                    self.blocks.len() - 1
                } else {
                    let i = (0..self.blocks.len())
                        .min_by_key(|&i| self.blocks[i].used)
                        .unwrap_or(0);
                    self.blocks[i] = block;
                    i
                }
            }
        };
        self.blocks[i].used = self.clock;
        i
    }

    /// The labels of `zoom` that can show in `area` (world pixels: left,
    /// top, right, bottom): those whose middle is within `REACH` of it.
    pub fn near(&mut self, zoom: u8, area: (i64, i64, i64, i64)) -> Vec<&Label> {
        let s = WORLD_Z - zoom.min(WORLD_Z);
        let reach = REACH << s;
        let (l, t, r, b) = (
            area.0 - reach,
            area.1 - reach,
            area.2 + reach,
            area.3 + reach,
        );
        let side_shift = 8 + BLOCK + u32::from(s);
        let (bx0, by0) = (l.max(0) >> side_shift, t.max(0) >> side_shift);
        let (bx1, by1) = (r.max(0) >> side_shift, b.max(0) >> side_shift);
        let mut slots = Vec::new();
        'blocks: for by in by0..=by1 {
            for bx in bx0..=bx1 {
                if slots.len() >= BLOCKS {
                    break 'blocks;
                }
                slots.push(self.slot((zoom, bx, by)));
            }
        }
        slots
            .into_iter()
            .flat_map(|i| self.blocks[i].labels.iter())
            .filter(|lb| l <= lb.at.0 && lb.at.0 <= r && t <= lb.at.1 && lb.at.1 <= b)
            .collect()
    }
}

/// The names' colours: text over a halo, as `scripts/map-tiles.py` has
/// them by day and with `--night`.
struct Palette {
    street: [u8; 3],
    place: [u8; 3],
    halo: [u8; 3],
}

const DAY: Palette = Palette {
    street: [60, 60, 60],
    place: [40, 40, 40],
    halo: [255, 255, 255],
};

const NIGHT: Palette = Palette {
    street: [200, 204, 210],
    place: [226, 228, 232],
    halo: [24, 26, 30],
};

/// What a label is stamped with: its lumas and its chroma pair.
struct Ink {
    lumas: [u8; 256],
    pair: [u8; 2],
}

impl Ink {
    fn of(text: [u8; 3], halo: [u8; 3]) -> Self {
        let (_, cb, cr) = nv12::ycc(halo);
        Self {
            lumas: nv12::label_lumas(text, halo),
            pair: [cb, cr],
        }
    }
}

/// The names of a set and what draws them.
pub struct Names {
    store: Store,
    glyphs: Glyphs,
    /// Street and place, by day and at night.
    inks: [Ink; 4],
    /// Labels drawn since the start (for the log).
    pub drawn: u32,
}

impl Names {
    /// The names of the set in `map`, when it has them as data.
    pub fn open(map: &Path) -> Option<Self> {
        let store = Store::open(map)?;
        let glyphs = Glyphs::new()?;
        let ink = |p: &Palette| [Ink::of(p.street, p.halo), Ink::of(p.place, p.halo)];
        let [a, b] = ink(&DAY);
        let [c, d] = ink(&NIGHT);
        Some(Self {
            store,
            glyphs,
            inks: [a, b, c, d],
            drawn: 0,
        })
    }

    /// Draw the names of `zoom` that show in `area` (world pixels) on
    /// `out`, only inside `clip` (its pixels): each where `place` puts it
    /// (world pixels to 1/16 pixels of `out`), a street along its way as
    /// `turn` lies it on `out` (65536 long), the right way up; a place
    /// level. In the night's colours when `night`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        out: &mut Surface<'_>,
        clip: (i64, i64, i64, i64),
        zoom: u8,
        area: (i64, i64, i64, i64),
        night: bool,
        place: impl Fn((i64, i64)) -> (i64, i64),
        turn: impl Fn((i64, i64)) -> (i64, i64),
    ) {
        let labels = self.store.near(zoom, area);
        for label in labels {
            let at = place(label.at);
            // Off the clip by more than it can reach: not even laid out.
            // An em a letter is more than any of the font's but a few.
            let chars = label.text.chars().count() as i64;
            let reach =
                (chars * i64::from(label.size) / 2 + i64::from(label.size) + 2 * i64::from(HALO))
                    * 16;
            if at.0 + reach < clip.0 * 16
                || at.0 - reach > clip.2 * 16
                || at.1 + reach < clip.1 * 16
                || at.1 - reach > clip.3 * 16
            {
                continue;
            }
            let (way, ink) = match label.kind {
                Kind::Street => (upright(turn(label.way)), 0),
                Kind::Place => ((65536, 0), 1),
            };
            let ink = &self.inks[ink + 2 * usize::from(night)];
            let sprite = self.glyphs.sprite(&label.text, label.size);
            nv12::stamp(out, clip, &sprite.stamp(), at, way, &ink.lumas, ink.pair);
            self.drawn += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lehu-labels-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `labels.pack` as `scripts/map-tiles.py --pack` writes it, from
    /// files (z + 256 x kind, bx, by, text).
    fn pack(files: &[(u32, u32, u32, &str)]) -> Vec<u8> {
        let mut files = files.to_vec();
        files.sort_by_key(|f| (f.0, f.1, f.2));
        let mut at = 16 + 20 * files.len();
        let mut out = b"LEHULABP".to_vec();
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (k, bx, by, text) in &files {
            for w in [*k, *bx, *by, at as u32, text.len() as u32] {
                out.extend_from_slice(&w.to_le_bytes());
            }
            at += text.len();
        }
        for f in &files {
            out.extend_from_slice(f.3.as_bytes());
        }
        out.resize(out.len().next_multiple_of(4), 0);
        out
    }

    #[test]
    fn packed_labels_read_as_the_files_they_were_packed_from() {
        // Zoom 12's block (0, 0); zoom 13 covers (0, 0) to (0, 1) with no
        // labels in (0, 1); zoom 14 not at all, so a block of it has
        // zoom 13's labels that lie in it (or 12's).
        let files: [(u32, u32, u32, &str); 4] = [
            (12, 0, 0, "s\t100\t100\t0\t22\tTwelve\n"),
            (
                13,
                0,
                0,
                "# lehu-labels 1\ns\t300\t300\t0\t22\tThirteen\np\t1000\t1000\t0\t22\tPlace\n",
            ),
            (13 | PACK_AREAS, 0, 0, "0 0 0 1\n"),
            (12 | PACK_AREAS, 0, 0, "0 0 0 0\n"),
        ];
        let loose = temp("loose");
        for (k, bx, by, text) in files {
            let z = k & 0xff;
            let path = if k & PACK_AREAS != 0 {
                loose.join(format!("labels/{z}/areas.txt"))
            } else {
                loose.join(format!("labels/{z}/{bx}/{by}.txt"))
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let packed = temp("packed");
        fs::write(packed.join(PACK), pack(&files)).unwrap();
        let mut a = Store::open(&loose).unwrap();
        let mut b = Store::open(&packed).unwrap();
        assert!(matches!(b.source, Source::Pack(_)));
        let mut seen = 0;
        for zoom in 11..=15u8 {
            let s = WORLD_Z - zoom;
            for area in [
                (0, 0, 400 << s, 400 << s),
                (0, 2048 << s, 400 << s, 2400 << s),
            ] {
                let x: Vec<Label> = a.near(zoom, area).into_iter().cloned().collect();
                let y: Vec<Label> = b.near(zoom, area).into_iter().cloned().collect();
                assert_eq!(x, y, "zoom {zoom}, {area:?}");
                seen += x.len();
            }
        }
        assert!(seen >= 3, "{seen}");
        assert_eq!((b.read, b.failed), (a.read, a.failed));
        // Not a pack, or one of another version: no names, not a crash.
        fs::write(packed.join(PACK), b"LEHULABP\x02\0\0\0\0\0\0\0").unwrap();
        assert!(Store::open(&packed).is_none());
        fs::write(packed.join(PACK), b"labels??").unwrap();
        assert!(Store::open(&packed).is_none());
        let _ = fs::remove_dir_all(&loose);
        let _ = fs::remove_dir_all(&packed);
    }

    #[test]
    fn a_label_file_reads_its_lines_and_leaves_out_bad_ones() {
        let text = "# lehu-labels 1\n\
                    s\t1000\t2000\t0\t22\tRue de la Gare\n\
                    s\t1000\t2000\t90\t22\tBoulevard Royal\n\
                    p\t300\t400\t0\t24\tLuxembourg\n\
                    x\t1\t1\t0\t22\tunknown kind\n\
                    s\t1\t1\t0\t200\ttoo large\n\
                    s\t1\t1\tnorth\t22\tno angle\n\
                    s\t1\t1\t0\t22\t\n\
                    \n";
        let labels = parse(text, 18);
        assert_eq!(labels.len(), 3);
        // Pixels of zoom 18 are 16 world pixels.
        assert_eq!(labels[0].at, (16_000, 32_000));
        assert_eq!(labels[0].way, (65536, 0));
        assert_eq!(&*labels[0].text, "Rue de la Gare");
        assert_eq!((labels[1].way, labels[1].size), ((0, 65536), 22));
        assert_eq!(labels[2].kind, Kind::Place);
        assert_eq!((labels[2].at, labels[2].size), ((4800, 6400), 24));
    }

    #[test]
    fn a_name_never_reads_upside_down() {
        // Right along the panel, or straight down it: kept.
        assert_eq!(upright((65536, 0)), (65536, 0));
        assert_eq!(upright((46341, 46341)), (46341, 46341));
        assert_eq!(upright((46341, -46341)), (46341, -46341));
        assert_eq!(upright((0, 65536)), (0, 65536));
        // Leftwards, or up: turned half round.
        assert_eq!(upright((-65536, 0)), (65536, 0));
        assert_eq!(upright((-46341, 46341)), (46341, -46341));
        assert_eq!(upright((-46341, -46341)), (46341, 46341));
        assert_eq!(upright((0, -65536)), (0, 65536));
    }

    #[test]
    fn a_missing_block_has_the_labels_a_zoom_out_in_it() {
        let map = temp("fallback");
        assert!(Store::open(&map).is_none());
        // Block (1, 1) of zoom 10 spans pixels 2048..4096 there: at zoom
        // 11, blocks 2 and 3 each way. Zoom 11 covered blocks (2, 2) to
        // (2, 3), and has labels in none of them; nor in (5, 5).
        let dir = map.join("labels/10/1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.txt"),
            "s\t2100\t2100\t0\t22\tWest\ns\t3900\t2100\t0\t22\tEast\n",
        )
        .unwrap();
        let dir = map.join("labels/11");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("areas.txt"), "2 2 2 3\n5 5 5 5\nbad line\n").unwrap();
        let mut store = Store::open(&map).unwrap();
        let s = WORLD_Z - 11;
        // Block (2, 2) of zoom 11 was covered: nothing from zoom 10.
        assert!(store.load((11, 2, 2)).is_empty());
        // Block (3, 2) was not: zoom 10's labels in it, East alone.
        let east = store.load((11, 3, 2));
        assert_eq!(east.len(), 1);
        assert_eq!(&*east[0].text, "East");
        assert_eq!(east[0].at, (3900 << (s + 1), 2100 << (s + 1)));
        // From two zooms in too; three are too far.
        assert_eq!(store.load((12, 7, 4)).len(), 1);
        assert!(store.load((13, 15, 8)).is_empty());
        let _ = fs::remove_dir_all(&map);
    }

    #[test]
    fn the_labels_near_an_area_are_those_that_can_reach_it() {
        let map = temp("near");
        let dir = map.join("labels/12/0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("0.txt"),
            "s\t100\t100\t0\t22\tIn\ns\t700\t100\t0\t22\tReach\np\t100\t1000\t0\t22\tFar\n",
        )
        .unwrap();
        let mut store = Store::open(&map).unwrap();
        let s = WORLD_Z - 12;
        // Pixels 0..300 each way: In, and Reach within REACH of it; Far
        // is not.
        let near = store.near(12, (0, 0, 300 << s, 300 << s));
        let names: Vec<&str> = near.iter().map(|l| &*l.text).collect();
        assert_eq!(names, ["In", "Reach"]);
        let _ = fs::remove_dir_all(&map);
    }

    /// The pixels `draw` changed on a land-coloured canvas, as a box.
    fn drawn_box(turn: (i64, i64), kind: &str) -> (usize, usize, usize, usize) {
        let map = temp(&format!("turn-{kind}-{}", turn.0));
        let dir = map.join("labels/16/0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("0.txt"),
            format!("{kind}\t200\t200\t0\t22\tAvenue de la Liberté\n"),
        )
        .unwrap();
        let mut names = Names::open(&map).unwrap();
        let (w, h) = (400usize, 400usize);
        let mut buf = vec![50u8; w * h * 3 / 2];
        let mut out = Surface {
            pixels: &mut buf,
            pitch: w,
            chroma: w * h,
            size: (w as u32, h as u32),
        };
        let s = WORLD_Z - 16;
        // The map turned about (200, 200): a way on it lies along `turn`
        // ((x, y) of east), the point where it is.
        let (c, sn) = turn;
        names.draw(
            &mut out,
            (0, 0, w as i64, h as i64),
            16,
            (0, 0, 400 << s, 400 << s),
            false,
            |p| ((p.0 >> s) * 16, (p.1 >> s) * 16),
            |m| ((m.0 * c - m.1 * sn) >> 16, (m.0 * sn + m.1 * c) >> 16),
        );
        let _ = fs::remove_dir_all(&map);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for y in 0..h {
            for x in 0..w {
                if buf[y * w + x] != 50 {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                }
            }
        }
        (x0, y0, x1, y1)
    }

    #[test]
    fn a_street_name_turns_with_the_map_and_a_place_stays_level() {
        let wide = |b: (usize, usize, usize, usize)| b.2 - b.0 > 3 * (b.3 - b.1);
        let tall = |b: (usize, usize, usize, usize)| b.3 - b.1 > 3 * (b.2 - b.0);
        // About its middle, (200, 200): along the text to a pixel or two;
        // across it the ink can sit off the box's middle (this name has
        // capitals and no descenders).
        let middle = |b: (usize, usize, usize, usize)| {
            let (x, y) = ((b.0 + b.2).abs_diff(400), (b.1 + b.3).abs_diff(400));
            x.min(y) <= 4 && x.max(y) <= 10
        };
        for turn in [(65536, 0), (-65536, 0)] {
            let b = drawn_box(turn, "s");
            assert!(wide(b) && middle(b), "{turn:?}: {b:?}");
        }
        for turn in [(0, 65536), (0, -65536)] {
            let b = drawn_box(turn, "s");
            assert!(tall(b) && middle(b), "{turn:?}: {b:?}");
        }
        // A place is level, the map turned or not.
        assert_eq!(drawn_box((0, 65536), "p"), drawn_box((65536, 0), "p"));
        assert!(wide(drawn_box((0, 65536), "p")));
    }
}
