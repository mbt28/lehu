// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Finding a destination by name: the streets and places of the map,
//! written on a PC by scripts/map-graph.py as `places.bin` and mapped from
//! the card.
//!
//! The file is little-endian 32-bit words: a header of 7 words (6 in
//! version 1), "LEHUFIND", the version (2), the counts of entries and
//! towns, the text's bytes and the count of points of interest; then the
//! towns, 2 words each (the name and its folded key, as byte offsets into
//! the text); then the entries, 5 words each (x, y in world pixels at zoom
//! 22, the name, its folded key, and the town's index (low 16 bits) + the
//! kind << 16 (0 a street, 1 a place) + the rank << 24 (lower is more
//! important)); then the points of interest, 5 words each (x, y, the name,
//! a detail such as the brand or the spaces, and the town's index + the
//! category << 16: 1 fuel, 2 parking, 3 charging), in the order of their
//! categories; then the text, each string a length byte and UTF-8.
//! Version 1, which LEHU still reads, has no points of interest.
//!
//! A key is the name folded: lower case, without accents, words split by
//! single spaces ("Rue d'Anvers" is "rue d anvers"). A name matches when
//! each word typed begins a word of it, or of its town's name, so "gare
//! lux" finds Avenue de la Gare in Luxembourg; the typed text is folded
//! the same way first, so case and accents never matter. A search reads
//! every entry, which for a country of streets is about a megabyte.
//!
//! The points of interest (fuel, parking, charging) are not searched by
//! name: [`Places::nearest`] finds the nearest of a category, reading only
//! that category's, a few thousand at most.

use std::path::Path;

use crate::blob::{Blob, short_bytes, short_str};

/// The file's name in the map's directory.
pub const PLACES_FILE: &str = "places.bin";

const MAGIC: &[u8; 8] = b"LEHUFIND";
const VERSION: u32 = 2;
const HEADER: usize = 7;
/// Version 1's header: no points of interest.
const HEADER_V1: usize = 6;
const TOWN: usize = 2;
const ENTRY: usize = 5;
const POI: usize = 5;

/// What a result is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FoundKind {
    #[default]
    Street,
    /// A city, a town, a village, a part of one.
    Place,
    /// A filling station.
    Fuel,
    /// A public car park.
    Parking,
    /// A charging station for electric cars.
    Charging,
}

impl FoundKind {
    /// The category's number in the file; none for a street or a place.
    fn category(self) -> Option<u32> {
        match self {
            Self::Fuel => Some(1),
            Self::Parking => Some(2),
            Self::Charging => Some(3),
            Self::Street | Self::Place => None,
        }
    }

    fn of_category(c: u32) -> Self {
        match c {
            1 => Self::Fuel,
            2 => Self::Parking,
            _ => Self::Charging,
        }
    }

    /// A point of interest's category in a word, for the screen: "Fuel",
    /// "Parking", "Charging"; empty for a street or a place.
    pub fn label(self) -> &'static str {
        match self {
            Self::Fuel => "Fuel",
            Self::Parking => "Parking",
            Self::Charging => "Charging",
            Self::Street | Self::Place => "",
        }
    }
}

/// A result: what it is called, the town it is in (empty for none), what
/// it is, where it is (world pixels at zoom 22), how far from where we
/// are, and for a point of interest a few words more (the brand, the
/// spaces, the charging points; empty for none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub town: String,
    pub kind: FoundKind,
    pub x: i64,
    pub y: i64,
    /// Metres in a straight line from the point the search was given.
    pub metres: Option<u32>,
    pub detail: String,
}

/// The names, read in place.
pub struct Places {
    blob: Blob,
    entries: usize,
    towns: usize,
    text_len: usize,
    pois: usize,
    town_at: usize,
    entry_at: usize,
    poi_at: usize,
    text_at: usize,
}

impl Places {
    /// The file in `dir`, mapped.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let path = dir.join(PLACES_FILE);
        Self::new(Blob::map(&path)?).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// A file's bytes (tests).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        Self::new(Blob::from_bytes(bytes)?)
    }

    fn new(blob: Blob) -> Result<Self, String> {
        let w = blob.words();
        if w.len() < HEADER || blob.bytes(0, 8) != Some(&MAGIC[..]) {
            return Err("not a LEHU search index".into());
        }
        let version = w[2];
        if !(1..=VERSION).contains(&version) {
            return Err(format!("version {version}, LEHU reads 1 to {VERSION}"));
        }
        let header = if version == 1 { HEADER_V1 } else { HEADER };
        if w.len() < header {
            return Err("cut short".into());
        }
        let (entries, towns, text_len) = (w[3] as usize, w[4] as usize, w[5] as usize);
        let pois = if version == 1 { 0 } else { w[6] as usize };
        let town_at = header;
        let entry_at = town_at + towns * TOWN;
        let poi_at = entry_at + entries * ENTRY;
        let text_at = poi_at + pois * POI;
        if text_at + text_len.div_ceil(4) != w.len() {
            return Err("its sections and its length disagree".into());
        }
        let places = Self {
            blob,
            entries,
            towns,
            text_len,
            pois,
            town_at,
            entry_at,
            poi_at,
            text_at,
        };
        // Every string and town it names is there (a small file: checked
        // as it is opened).
        let w = places.blob.words();
        let fits = |at: u32| (at as usize) < text_len;
        for t in w[town_at..entry_at].as_chunks::<TOWN>().0 {
            if !fits(t[0]) || !fits(t[1]) {
                return Err("a town's name is out of range".into());
            }
        }
        for e in w[entry_at..poi_at].as_chunks::<ENTRY>().0 {
            if !fits(e[2]) || !fits(e[3]) || (e[4] & 0xFFFF) as usize >= towns.max(1) {
                return Err("an entry is out of range".into());
            }
        }
        let mut last = 0;
        for p in w[poi_at..text_at].as_chunks::<POI>().0 {
            let category = p[4] >> 16;
            if !fits(p[2]) || !fits(p[3]) || (p[4] & 0xFFFF) as usize >= towns.max(1) {
                return Err("a point of interest is out of range".into());
            }
            if !(1..=3).contains(&category) || category < last {
                return Err("the points of interest are out of order".into());
            }
            last = category;
        }
        Ok(places)
    }

    /// The points of interest of a category (the words of each).
    fn pois_of(&self, kind: FoundKind) -> &[[u32; POI]] {
        let Some(c) = kind.category() else {
            return &[];
        };
        let all = self.blob.words()[self.poi_at..self.text_at]
            .as_chunks::<POI>()
            .0;
        let from = all.partition_point(|p| p[4] >> 16 < c);
        let to = all.partition_point(|p| p[4] >> 16 <= c);
        &all[from..to]
    }

    /// How many points of interest of a category there are: none for a
    /// street or a place, or with a version 1 file.
    pub fn count(&self, kind: FoundKind) -> usize {
        self.pois_of(kind).len()
    }

    /// Up to `limit` points of interest of a category, the nearest to
    /// `near` first (by the straight line). One pass over the category's.
    pub fn nearest(&self, kind: FoundKind, near: (i64, i64), limit: usize) -> Vec<Found> {
        if limit == 0 || self.pois == 0 {
            return Vec::new();
        }
        let mut best: Vec<(u64, usize)> = Vec::with_capacity(limit + 1);
        let pois = self.pois_of(kind);
        for (i, p) in pois.iter().enumerate() {
            let dx = (i64::from(p[0]) - near.0).unsigned_abs();
            let dy = (i64::from(p[1]) - near.1).unsigned_abs();
            // Under 2^31 each: the squares add up within 64 bits.
            let d = dx * dx + dy * dy;
            if best.len() == limit && best.last().is_some_and(|b| d >= b.0) {
                continue;
            }
            let at = best.partition_point(|b| b.0 <= d);
            best.insert(at, (d, i));
            best.truncate(limit);
        }
        let w = self.blob.words();
        let text = self.text();
        let towns = &w[self.town_at..self.entry_at];
        best.into_iter()
            .map(|(_, i)| {
                let p = &pois[i];
                let (x, y) = (i64::from(p[0]), i64::from(p[1]));
                Found {
                    name: short_str(text, p[2]).to_string(),
                    town: short_str(text, towns[(p[4] & 0xFFFF) as usize * TOWN]).to_string(),
                    kind: FoundKind::of_category(p[4] >> 16),
                    x,
                    y,
                    metres: Some(crate::track::ground_px(near, (x, y)) as u32),
                    detail: short_str(text, p[3]).to_string(),
                }
            })
            .collect()
    }

    /// How many names it has.
    pub fn len(&self) -> usize {
        self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    fn text(&self) -> &[u8] {
        self.blob.bytes(self.text_at, self.text_len).unwrap_or(&[])
    }

    /// Up to `limit` names that match `query`, the best first: the whole
    /// name typed, then the name beginning with it, then every word typed
    /// beginning a word of the name, then some only of its town's; among
    /// those places before streets, then the nearest to `near` (where we
    /// are), then the shortest.
    pub fn search(&self, query: &str, near: Option<(i64, i64)>, limit: usize) -> Vec<Found> {
        let q = fold(query);
        if q.is_empty() || limit == 0 || self.towns == 0 {
            return Vec::new();
        }
        let words: Vec<&[u8]> = q.split(' ').map(str::as_bytes).collect();
        let w = self.blob.words();
        let text = self.text();
        let towns = &w[self.town_at..self.entry_at];
        // The best so far, worst last: (order, entry).
        let mut best: Vec<(u64, usize)> = Vec::with_capacity(limit + 1);
        for (i, e) in w[self.entry_at..self.poi_at]
            .as_chunks::<ENTRY>()
            .0
            .iter()
            .enumerate()
        {
            let key = short_bytes(text, e[3]);
            let quality: u32 = if key == q.as_bytes() {
                0
            } else if key.starts_with(q.as_bytes()) {
                1
            } else {
                let town = short_bytes(text, towns[(e[4] & 0xFFFF) as usize * TOWN + 1]);
                let mut in_name = 0;
                let mut all = true;
                for word in &words {
                    if begins_a_word(key, word) {
                        in_name += 1;
                    } else if !begins_a_word(town, word) {
                        all = false;
                        break;
                    }
                }
                if !all || in_name == 0 {
                    continue;
                }
                if in_name == words.len() { 2 } else { 3 }
            };
            let kind = (e[4] >> 16) & 0xFF;
            let rank = e[4] >> 24;
            // Places first, the more important first; then the nearest.
            let tier = if kind == 1 { rank.min(14) } else { 15 };
            let far = near.map_or(0, |(x, y)| {
                let (dx, dy) = (
                    (i64::from(e[0]) - x).unsigned_abs(),
                    (i64::from(e[1]) - y).unsigned_abs(),
                );
                // A world pixel at zoom 22 is a few centimetres: in units
                // of 2^10 (about 25 m), up to a million.
                (dx.max(dy) >> 10).min(0xF_FFFF)
            });
            let order = (u64::from(quality) << 60)
                | (u64::from(tier) << 56)
                | (far << 36)
                | ((key.len() as u64).min(0xFF) << 28)
                | (i as u64 & 0xFFF_FFFF);
            if best.len() == limit && best.last().is_some_and(|b| order >= b.0) {
                continue;
            }
            let at = best.partition_point(|b| b.0 < order);
            best.insert(at, (order, i));
            best.truncate(limit);
        }
        best.into_iter()
            .map(|(_, i)| {
                let e = &w[self.entry_at + i * ENTRY..self.entry_at + (i + 1) * ENTRY];
                let town = short_str(text, towns[(e[4] & 0xFFFF) as usize * TOWN]);
                let (x, y) = (i64::from(e[0]), i64::from(e[1]));
                Found {
                    name: short_str(text, e[2]).to_string(),
                    town: town.to_string(),
                    kind: if (e[4] >> 16) & 0xFF == 1 {
                        FoundKind::Place
                    } else {
                        FoundKind::Street
                    },
                    x,
                    y,
                    metres: near.map(|p| crate::track::ground_px(p, (x, y)) as u32),
                    detail: String::new(),
                }
            })
            .collect()
    }
}

/// `word` begins one of the words of `key` (single spaces between them).
fn begins_a_word(key: &[u8], word: &[u8]) -> bool {
    if word.is_empty() {
        return true;
    }
    let mut start = 0;
    loop {
        if key[start..].starts_with(word) {
            return true;
        }
        match key[start..].iter().position(|&c| c == b' ') {
            Some(s) => start += s + 1,
            None => return false,
        }
    }
}

/// Text as it is matched: lower case, without accents, every run of
/// anything but letters and digits one space ("Rue d'Anvers" is "rue d
/// anvers", "Esch-sur-Alzette" is "esch sur alzette"). The same as
/// scripts/map-graph.py's fold for the Latin letters; other scripts keep
/// their letters, in lower case.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        let mut put = |s: &str| {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push_str(s);
        };
        if c.is_ascii_alphanumeric() {
            put(c.to_ascii_lowercase().encode_utf8(&mut [0; 4]));
        } else if let Some(s) = latin(c) {
            put(s);
        } else if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                put(l.encode_utf8(&mut [0; 4]));
            }
        } else {
            space = true;
        }
    }
    out
}

/// A Latin letter with an accent as the letters it is matched as.
fn latin(c: char) -> Option<&'static str> {
    // U+00C0 to U+017F, in order: Latin-1's letters, then Latin
    // Extended-A; "" for the two signs among them.
    const TABLE: [&str; 192] = [
        "a", "a", "a", "a", "a", "a", "ae", "c", "e", "e", "e", "e", "i", "i", "i", "i", // C0
        "d", "n", "o", "o", "o", "o", "o", "", "o", "u", "u", "u", "u", "y", "th", "ss", // D0
        "a", "a", "a", "a", "a", "a", "ae", "c", "e", "e", "e", "e", "i", "i", "i", "i", // E0
        "d", "n", "o", "o", "o", "o", "o", "", "o", "u", "u", "u", "u", "y", "th", "y", // F0
        "a", "a", "a", "a", "a", "a", "c", "c", "c", "c", "c", "c", "c", "c", "d", "d", // 100
        "d", "d", "e", "e", "e", "e", "e", "e", "e", "e", "e", "e", "g", "g", "g", "g", // 110
        "g", "g", "g", "g", "h", "h", "h", "h", "i", "i", "i", "i", "i", "i", "i", "i", // 120
        "i", "i", "ij", "ij", "j", "j", "k", "k", "k", "l", "l", "l", "l", "l", "l",
        "l", // 130
        "l", "l", "l", "n", "n", "n", "n", "n", "n", "n", "n", "n", "o", "o", "o", "o", // 140
        "o", "o", "oe", "oe", "r", "r", "r", "r", "r", "r", "s", "s", "s", "s", "s",
        "s", // 150
        "s", "s", "t", "t", "t", "t", "t", "t", "u", "u", "u", "u", "u", "u", "u", "u", // 160
        "u", "u", "u", "u", "w", "w", "y", "y", "y", "z", "z", "z", "z", "z", "z", "s", // 170
    ];
    let i = (c as u32).checked_sub(0xC0)?;
    TABLE.get(i as usize).copied().filter(|s| !s.is_empty())
}

/// An index written as scripts/map-graph.py writes it, for tests.
#[cfg(test)]
pub(crate) mod build {
    /// (name, town index, street or place, rank, x, y), and the towns.
    pub(crate) fn write(entries: &[(&str, u32, u32, u32, i64, i64)], towns: &[&str]) -> Vec<u8> {
        write_with(entries, &[], towns, super::VERSION)
    }

    /// A point of interest: (name, detail, town index, category, x, y).
    pub(crate) type Poi<'a> = (&'a str, &'a str, u32, u32, i64, i64);

    /// The same with points of interest (in the order of their
    /// categories), as version `version` writes it (1: without them).
    pub(crate) fn write_with(
        entries: &[(&str, u32, u32, u32, i64, i64)],
        pois: &[Poi<'_>],
        towns: &[&str],
        version: u32,
    ) -> Vec<u8> {
        let mut text: Vec<u8> = Vec::new();
        let mut add = |s: &str| {
            let at = text.len() as u32;
            text.push(s.len() as u8);
            text.extend_from_slice(s.as_bytes());
            at
        };
        let mut w: Vec<u32> = Vec::new();
        let town_words: Vec<u32> = towns
            .iter()
            .flat_map(|t| [add(t), add(&super::fold(t))])
            .collect();
        let entry_words: Vec<u32> = entries
            .iter()
            .flat_map(|&(name, town, kind, rank, x, y)| {
                [
                    x as u32,
                    y as u32,
                    add(name),
                    add(&super::fold(name)),
                    town | (kind << 16) | (rank << 24),
                ]
            })
            .collect();
        let poi_words: Vec<u32> = pois
            .iter()
            .flat_map(|&(name, detail, town, category, x, y)| {
                [
                    x as u32,
                    y as u32,
                    add(name),
                    add(detail),
                    town | (category << 16),
                ]
            })
            .collect();
        w.extend([0, 0, version, entries.len() as u32, towns.len() as u32]);
        w.push(text.len() as u32);
        if version > 1 {
            w.push(pois.len() as u32);
        }
        w.extend(town_words);
        w.extend(entry_words);
        if version > 1 {
            w.extend(poi_words);
        }
        while !text.len().is_multiple_of(4) {
            text.push(0);
        }
        let mut bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
        bytes[..8].copy_from_slice(super::MAGIC);
        bytes.extend(&text);
        bytes
    }
}
