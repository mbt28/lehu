// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Finding a destination by name: the streets and places of the map,
//! written on a PC by scripts/map-graph.py as `places.bin` and mapped from
//! the card.
//!
//! The file is little-endian 32-bit words: a header of 9 words (7 in
//! version 2, 6 in version 1), "LEHUFIND", the version (3), the counts of
//! entries and towns, the text's bytes, the count of points of interest,
//! of houses and of words; then
//! - the towns, 2 words each (the name and its folded key, as byte offsets
//!   into the text);
//! - the entries, 5 words each (x, y in world pixels at zoom 22, the name,
//!   its folded key, and the town's index (low 16 bits) + the kind << 16
//!   (0 a street, 1 a place) + the rank << 24 (lower is more important));
//! - the points of interest, 5 words each (x, y, the name, a detail such as
//!   the brand or the spaces, and the town's index + the category << 16: 1
//!   fuel, 2 parking, 3 charging), in the order of their categories;
//! - the words: every word of every entry's key, 2 words each (the word and
//!   its entry), sorted by the word's bytes;
//! - each town's entries: the towns + 1 words, where each town's begin in
//!   the list that follows, then the entries' indices, town by town;
//! - each street's houses: the entries + 1 words, where each entry's begin
//!   in the houses, then the houses, 4 words each (x, y, the number as it
//!   is written, "57/A", and the postcode, empty for none);
//! - the text, each string a length byte and UTF-8.
//!
//! Version 2 has no words, towns' entries or houses, version 1 no points of
//! interest either; LEHU reads both, a search then reading every entry.
//!
//! A key is the name folded: lower case, without accents, words split by
//! single spaces ("Rue d'Anvers" is "rue d anvers"). A name matches when
//! each word typed begins a word of it, or of its town's name, so "gare
//! lux" finds Avenue de la Gare in Luxembourg; the typed text is folded
//! the same way first, so case and accents never matter. Of the words
//! typed, the one the fewest entries can match is looked up (in the words,
//! and the entries of the towns it begins a word of), and only those
//! entries are read: a few hundred of a country's hundred thousand.
//!
//! A word typed that begins with a digit is a house number or a postcode:
//! the streets found list their houses whose number begins with it ("5"
//! finds 5, 57 and 57/A; a letter typed apart after it is part of it, "57
//! a"), each named with the street ("Main Street 57") and its postcode;
//! a postcode typed whole keeps only the houses with it. A street without
//! the number still lists its nearest number on the same side of the
//! street, and the street itself after its houses.
//!
//! The points of interest (fuel, parking, charging) are not searched by
//! name: [`Places::nearest`] finds the nearest of a category, reading only
//! that category's, a few thousand at most.

use std::path::Path;

use crate::blob::{Blob, short_bytes, short_str};

/// The file's name in the map's directory.
pub const PLACES_FILE: &str = "places.bin";

const MAGIC: &[u8; 8] = b"LEHUFIND";
const VERSION: u32 = 3;
const HEADER: usize = 9;
/// Version 2's header: no words or houses.
const HEADER_V2: usize = 7;
/// Version 1's header: no points of interest.
const HEADER_V1: usize = 6;
const TOWN: usize = 2;
const ENTRY: usize = 5;
const POI: usize = 5;
const WORD: usize = 2;
const HOUSE: usize = 4;
/// The streets whose houses a number typed is looked for in: the best this
/// many found by the other words.
const HOUSE_STREETS: usize = 24;
/// The number of the house nearest a number not on its street, at most
/// this far from it.
const NEAREST_HOUSE: u32 = 20;
/// Words typed short as they are written on signs and letters, which no
/// name's word begins with, and the words the names have in full.
const SHORT: [(&str, &str); 4] = [
    ("krt", "korut"),
    ("rkp", "rakpart"),
    ("sgt", "sugarut"),
    ("ltp", "lakotelep"),
];

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
    houses: usize,
    town_at: usize,
    entry_at: usize,
    poi_at: usize,
    // Version 3's sections (empty before): the words, each town's entries,
    // each street's houses.
    word_at: usize,
    town_first_at: usize,
    town_list_at: usize,
    house_first_at: usize,
    house_at: usize,
    text_at: usize,
    indexed: bool,
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
        if w.len() < HEADER_V1 || blob.bytes(0, 8) != Some(&MAGIC[..]) {
            return Err("not a LEHU search index".into());
        }
        let version = w[2];
        if !(1..=VERSION).contains(&version) {
            return Err(format!("version {version}, LEHU reads 1 to {VERSION}"));
        }
        let header = match version {
            1 => HEADER_V1,
            2 => HEADER_V2,
            _ => HEADER,
        };
        if w.len() < header {
            return Err("cut short".into());
        }
        let count = |i: usize| if i < header { w[i] as usize } else { 0 };
        let (entries, towns, text_len, pois) = (count(3), count(4), count(5), count(6));
        let (houses, words) = (count(7), count(8));
        let indexed = version >= 3;
        // Each section after the one before; sizes from the file, so
        // added with care (a 32-bit board).
        let mut at = header;
        let mut section = |items: usize, size: usize| -> Result<usize, String> {
            let begin = at;
            at = items
                .checked_mul(size)
                .and_then(|n| n.checked_add(at))
                .ok_or("its counts are out of range")?;
            Ok(begin)
        };
        let town_at = section(towns, TOWN)?;
        let entry_at = section(entries, ENTRY)?;
        let poi_at = section(pois, POI)?;
        let word_at = section(words, WORD)?;
        let town_first_at = section(if indexed { towns + 1 } else { 0 }, 1)?;
        let town_list_at = section(if indexed { entries } else { 0 }, 1)?;
        let house_first_at = section(if indexed { entries + 1 } else { 0 }, 1)?;
        let house_at = section(houses, HOUSE)?;
        let text_at = section(0, 1)?;
        if text_at.checked_add(text_len.div_ceil(4)) != Some(w.len()) {
            return Err("its sections and its length disagree".into());
        }
        let places = Self {
            blob,
            entries,
            towns,
            text_len,
            pois,
            houses,
            town_at,
            entry_at,
            poi_at,
            word_at,
            town_first_at,
            town_list_at,
            house_first_at,
            house_at,
            text_at,
            indexed,
        };
        // Every string, town, entry and house it names is there (checked
        // as it is opened: a search then reads them unchecked).
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
        for p in w[poi_at..word_at].as_chunks::<POI>().0 {
            let category = p[4] >> 16;
            if !fits(p[2]) || !fits(p[3]) || (p[4] & 0xFFFF) as usize >= towns.max(1) {
                return Err("a point of interest is out of range".into());
            }
            if !(1..=3).contains(&category) || category < last {
                return Err("the points of interest are out of order".into());
            }
            last = category;
        }
        if indexed {
            for x in w[word_at..town_first_at].as_chunks::<WORD>().0 {
                if !fits(x[0]) || x[1] as usize >= entries {
                    return Err("a word is out of range".into());
                }
            }
            if !ascending(&w[town_first_at..town_list_at], entries) {
                return Err("the towns' entries are out of order".into());
            }
            if w[town_list_at..house_first_at]
                .iter()
                .any(|&i| i as usize >= entries)
            {
                return Err("a town's entry is out of range".into());
            }
            if !ascending(&w[house_first_at..house_at], houses) {
                return Err("the houses are out of order".into());
            }
            for h in w[house_at..text_at].as_chunks::<HOUSE>().0 {
                if !fits(h[2]) || !fits(h[3]) {
                    return Err("a house is out of range".into());
                }
            }
        }
        Ok(places)
    }

    /// The points of interest of a category (the words of each).
    fn pois_of(&self, kind: FoundKind) -> &[[u32; POI]] {
        let Some(c) = kind.category() else {
            return &[];
        };
        let all = self.blob.words()[self.poi_at..self.word_at]
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
    /// are), then the shortest. A number typed puts the streets' houses
    /// with it first (see the module's notes).
    pub fn search(&self, query: &str, near: Option<(i64, i64)>, limit: usize) -> Vec<Found> {
        let q = fold(query);
        let typed = Typed::of(&q);
        if typed.names.is_empty() || limit == 0 || self.towns == 0 {
            return Vec::new();
        }
        let houses = !typed.numbers.is_empty() && self.houses > 0;
        let named = typed.names.join(" ");
        let w = self.blob.words();
        let text = self.text();
        let towns = &w[self.town_at..self.entry_at];
        let entries = w[self.entry_at..self.poi_at].as_chunks::<ENTRY>().0;
        // The streets first, enough of them to look for the houses in.
        let mut best = Best::new(if houses {
            limit.max(HOUSE_STREETS)
        } else {
            limit
        });
        let mut score = |i: usize| {
            let e = &entries[i];
            let key = short_bytes(text, e[3]);
            let quality: u64 = if key == named.as_bytes() {
                0
            } else if key.starts_with(named.as_bytes()) {
                1
            } else {
                let town = short_bytes(text, towns[(e[4] & 0xFFFF) as usize * TOWN + 1]);
                let mut in_name = 0;
                for word in &typed.names {
                    if begins_a_word(key, word.as_bytes()) {
                        in_name += 1;
                    } else if !begins_a_word(town, word.as_bytes()) {
                        return;
                    }
                }
                match in_name {
                    0 => return,
                    n if n == typed.names.len() => 2,
                    _ => 3,
                }
            };
            let kind = (e[4] >> 16) & 0xFF;
            let rank = e[4] >> 24;
            // Places first, the more important first; then the nearest.
            let tier = if kind == 1 { rank.min(14) } else { 15 };
            let order = (quality << 60)
                | (u64::from(tier) << 56)
                | (far(near, e[0], e[1]) << 36)
                | ((key.len() as u64).min(0xFF) << 28)
                | (i as u64 & 0xFFF_FFFF);
            best.offer(order, i);
        };
        match self.candidates(&typed.names) {
            Some(list) => list.into_iter().for_each(|i| score(i as usize)),
            None => (0..entries.len()).for_each(score),
        }
        let streets = best.into_vec();
        // The houses of those streets, before them.
        let mut found: Vec<(u64, Item)> = Vec::new();
        if houses {
            let is_street = |i: usize| (entries[i][4] >> 16) & 0xFF == 0;
            for &(_, i) in &streets {
                if is_street(i) {
                    self.houses_of(i, &typed.numbers, near, &mut found);
                }
            }
            // No street has the number: each one's nearest on its side.
            if found.is_empty() {
                for &(_, i) in &streets {
                    if is_street(i) {
                        self.nearest_house(i, &typed.numbers, near, &mut found);
                    }
                }
            }
            // The streets themselves after the houses.
            found.extend(
                streets
                    .iter()
                    .map(|&(order, i)| (order + (3 << 60), Item::Entry(i))),
            );
            found.sort_unstable_by_key(|f| f.0);
            found.truncate(limit);
        } else {
            found.extend(streets.into_iter().map(|(o, i)| (o, Item::Entry(i))));
        }
        found
            .into_iter()
            .map(|(_, item)| self.found(item, near))
            .collect()
    }

    /// The entries a search reads: of the words typed, the one the fewest
    /// entries can match is looked up, in the words and in the towns it
    /// begins a word of (every entry that matches has it in its name or
    /// its town's). None, every entry, without the index (before version
    /// 3).
    fn candidates(&self, names: &[&str]) -> Option<Vec<u32>> {
        if !self.indexed {
            return None;
        }
        let w = self.blob.words();
        let text = self.text();
        let words = w[self.word_at..self.town_first_at].as_chunks::<WORD>().0;
        let town_keys = w[self.town_at..self.entry_at].as_chunks::<TOWN>().0;
        let town_first = &w[self.town_first_at..self.town_list_at];
        let town_list = &w[self.town_list_at..self.house_first_at];
        let in_town = |t: usize| town_first[t] as usize..town_first[t + 1] as usize;
        let mut fewest: Option<(usize, std::ops::Range<usize>, Vec<usize>)> = None;
        for name in names {
            let p = name.as_bytes();
            let lo = words.partition_point(|x| short_bytes(text, x[0]) < p);
            let hi = lo
                + words[lo..].partition_point(|x| {
                    let k = short_bytes(text, x[0]);
                    &k[..k.len().min(p.len())] <= p
                });
            let mut count = hi - lo;
            let mut its_towns = Vec::new();
            for (t, k) in town_keys.iter().enumerate() {
                if begins_a_word(short_bytes(text, k[1]), p) {
                    its_towns.push(t);
                    count += in_town(t).len();
                }
            }
            if fewest.as_ref().is_none_or(|f| count < f.0) {
                fewest = Some((count, lo..hi, its_towns));
            }
        }
        let (_, range, its_towns) = fewest?;
        let mut list: Vec<u32> = words[range].iter().map(|x| x[1]).collect();
        for t in its_towns {
            list.extend_from_slice(&town_list[in_town(t)]);
        }
        list.sort_unstable();
        list.dedup();
        Some(list)
    }

    /// Street `i`'s houses that the numbers typed fit, into `found`.
    fn houses_of(
        &self,
        i: usize,
        numbers: &[String],
        near: Option<(i64, i64)>,
        found: &mut Vec<(u64, Item)>,
    ) {
        let (text, all, range) = self.houses_at(i);
        let mut key = String::new();
        for (h, house) in all[range.clone()].iter().enumerate() {
            house_key(&mut key, short_str(text, house[2]));
            if let Some(fit) = fits(&key, short_str(text, house[3]), numbers) {
                let h = range.start + h;
                found.push((house_order(fit, near, house, &key, h), Item::House(i, h)));
            }
        }
    }

    /// Street `i`'s house nearest the shortest number typed, on the same
    /// side of the street (the same parity), within NEAREST_HOUSE, into
    /// `found`; the other numbers typed are postcodes.
    fn nearest_house(
        &self,
        i: usize,
        numbers: &[String],
        near: Option<(i64, i64)>,
        found: &mut Vec<(u64, Item)>,
    ) {
        let Some(t) = (0..numbers.len()).min_by_key(|&t| numbers[t].len()) else {
            return;
        };
        let Ok(target) = numbers[t].parse::<u32>() else {
            return;
        };
        let rest = numbers
            .iter()
            .enumerate()
            .filter(|&(r, _)| r != t)
            .map(|(_, n)| n);
        let (text, all, range) = self.houses_at(i);
        let mut nearest: Option<(u32, usize)> = None;
        for h in range {
            let number = short_str(text, all[h][2]);
            let digits = number.len()
                - number
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .len();
            let Ok(n) = number[..digits.min(6)].parse::<u32>() else {
                continue;
            };
            let post = short_str(text, all[h][3]);
            if n % 2 != target % 2 || !rest.clone().all(|r| post_fits(post, r)) {
                continue;
            }
            let d = n.abs_diff(target);
            if d <= NEAREST_HOUSE && nearest.is_none_or(|b| d < b.0) {
                nearest = Some((d, h));
            }
        }
        if let Some((_, h)) = nearest {
            let mut key = String::new();
            house_key(&mut key, short_str(text, all[h][2]));
            found.push((house_order(2, near, &all[h], &key, h), Item::House(i, h)));
        }
    }

    /// The text, all the houses, and street `i`'s among them.
    fn houses_at(&self, i: usize) -> (&[u8], &[[u32; HOUSE]], std::ops::Range<usize>) {
        let w = self.blob.words();
        let first = &w[self.house_first_at..self.house_at];
        let all = w[self.house_at..self.text_at].as_chunks::<HOUSE>().0;
        (self.text(), all, first[i] as usize..first[i + 1] as usize)
    }

    /// What a search found, for the screens.
    fn found(&self, item: Item, near: Option<(i64, i64)>) -> Found {
        let w = self.blob.words();
        let text = self.text();
        let towns = &w[self.town_at..self.entry_at];
        let (i, house) = match item {
            Item::Entry(i) => (i, None),
            Item::House(i, h) => (
                i,
                Some(&w[self.house_at + h * HOUSE..self.house_at + (h + 1) * HOUSE]),
            ),
        };
        let e = &w[self.entry_at + i * ENTRY..self.entry_at + (i + 1) * ENTRY];
        let name = short_str(text, e[2]);
        let town = short_str(text, towns[(e[4] & 0xFFFF) as usize * TOWN]);
        let (name, town, x, y) = match house {
            None => (name.to_string(), town.to_string(), e[0], e[1]),
            Some(h) => {
                let post = short_str(text, h[3]);
                (
                    format!("{name} {}", short_str(text, h[2])),
                    if post.is_empty() {
                        town.to_string()
                    } else {
                        format!("{post} {town}")
                    },
                    h[0],
                    h[1],
                )
            }
        };
        let (x, y) = (i64::from(x), i64::from(y));
        Found {
            name,
            town,
            kind: if house.is_none() && (e[4] >> 16) & 0xFF == 1 {
                FoundKind::Place
            } else {
                FoundKind::Street
            },
            x,
            y,
            metres: near.map(|p| crate::track::ground_px(p, (x, y)) as u32),
            detail: String::new(),
        }
    }
}

/// What a search found: an entry, or a house (its street, the house).
#[derive(Clone, Copy)]
enum Item {
    Entry(usize),
    House(usize, usize),
}

/// The best results so far, worst last: (order, entry).
struct Best {
    limit: usize,
    list: Vec<(u64, usize)>,
}

impl Best {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            list: Vec::with_capacity(limit + 1),
        }
    }

    fn offer(&mut self, order: u64, i: usize) {
        if self.limit == 0
            || (self.list.len() == self.limit && self.list.last().is_some_and(|b| order >= b.0))
        {
            return;
        }
        let at = self.list.partition_point(|b| b.0 < order);
        self.list.insert(at, (order, i));
        self.list.truncate(self.limit);
    }

    fn into_vec(self) -> Vec<(u64, usize)> {
        self.list
    }
}

/// The words typed: the names' (with the short forms written out), and
/// the numbers (a letter typed apart after one is part of it: "57 a").
struct Typed<'a> {
    names: Vec<&'a str>,
    numbers: Vec<String>,
}

impl<'a> Typed<'a> {
    fn of(folded: &'a str) -> Self {
        let mut names = Vec::new();
        let mut numbers: Vec<String> = Vec::new();
        let mut after_number = false;
        for word in folded.split(' ').filter(|w| !w.is_empty()) {
            let b = word.as_bytes();
            if b[0].is_ascii_digit() {
                numbers.push(word.to_string());
                after_number = true;
            } else if after_number && b.len() == 1 && b[0].is_ascii_alphabetic() {
                if let Some(n) = numbers.last_mut() {
                    n.push_str(word);
                }
                after_number = false;
            } else {
                after_number = false;
                names.push(SHORT.iter().find(|s| s.0 == word).map_or(word, |s| s.1));
            }
        }
        Self { names, numbers }
    }
}

/// A house's number as it is matched, into `out`: folded, a letter after
/// digits joined to them ("57/A" is "57a", "30-32" "30 32").
fn house_key(out: &mut String, number: &str) {
    out.clear();
    let mut after_digit = false;
    for word in fold(number).split(' ').filter(|w| !w.is_empty()) {
        let b = word.as_bytes();
        let letter = b.len() == 1 && b[0].is_ascii_alphabetic();
        if !(letter && after_digit) && !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        after_digit = b.last().is_some_and(u8::is_ascii_digit);
    }
}

/// How a house fits the numbers typed: Some(0) one is its number whole,
/// Some(1) one begins it; None when one is neither its number's beginning
/// nor its postcode's, or none is its number's.
fn fits(key: &str, postcode: &str, numbers: &[String]) -> Option<u32> {
    let mut fit = None;
    for n in numbers {
        let whole = key.split(' ').any(|k| k == n);
        if whole || key.split(' ').any(|k| k.starts_with(n.as_str())) {
            let f = u32::from(!whole);
            fit = Some(fit.map_or(f, |b: u32| b.min(f)));
        } else if !post_fits(postcode, n) {
            return None;
        }
    }
    fit
}

/// A number typed begins a house's postcode (spaces in either aside).
fn post_fits(postcode: &str, typed: &str) -> bool {
    let mut post = postcode
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase());
    !typed.is_empty() && typed.bytes().all(|c| post.next() == Some(c))
}

/// A house's place among the results: by how it fits, then the nearest,
/// then the shortest number.
fn house_order(fit: u32, near: Option<(i64, i64)>, house: &[u32], key: &str, h: usize) -> u64 {
    (u64::from(fit) << 60)
        | (15 << 56)
        | (far(near, house[0], house[1]) << 36)
        | ((key.len() as u64).min(0xFF) << 28)
        | (h as u64 & 0xFFF_FFFF)
}

/// How far a point is from `near`, coarsely: in units of 2^10 world pixels
/// at zoom 22 (about 25 m), up to a million; 0 with no `near`.
fn far(near: Option<(i64, i64)>, x: u32, y: u32) -> u64 {
    near.map_or(0, |(nx, ny)| {
        let (dx, dy) = (
            (i64::from(x) - nx).unsigned_abs(),
            (i64::from(y) - ny).unsigned_abs(),
        );
        (dx.max(dy) >> 10).min(0xF_FFFF)
    })
}

/// `first` (a section of where each item's things begin) rises from 0 to
/// `end`.
fn ascending(first: &[u32], end: usize) -> bool {
    first.first().is_none_or(|&f| f == 0)
        && first.last().is_none_or(|&l| l as usize == end)
        && first.windows(2).all(|p| p[0] <= p[1])
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

    /// A house: (its entry, number, postcode, x, y), in the order of their
    /// entries.
    pub(crate) type House<'a> = (u32, &'a str, &'a str, i64, i64);

    /// The same with points of interest (in the order of their
    /// categories), as version `version` writes it (1: without them).
    pub(crate) fn write_with(
        entries: &[(&str, u32, u32, u32, i64, i64)],
        pois: &[Poi<'_>],
        towns: &[&str],
        version: u32,
    ) -> Vec<u8> {
        write_houses(entries, pois, towns, &[], version)
    }

    /// The same with houses (version 3).
    pub(crate) fn write_houses(
        entries: &[(&str, u32, u32, u32, i64, i64)],
        pois: &[Poi<'_>],
        towns: &[&str],
        houses: &[House<'_>],
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
        let mut words: Vec<(String, u32)> = entries
            .iter()
            .enumerate()
            .flat_map(|(i, e)| {
                super::fold(e.0)
                    .split(' ')
                    .map(|w| (w.to_string(), i as u32))
                    .collect::<Vec<_>>()
            })
            .collect();
        words.sort();
        words.dedup();
        let word_words: Vec<u32> = words.iter().flat_map(|(w, i)| [add(w), *i]).collect();
        let mut by_town: Vec<u32> = (0..entries.len() as u32).collect();
        by_town.sort_by_key(|&i| (entries[i as usize].1, i));
        let town_first: Vec<u32> = (0..=towns.len() as u32)
            .map(|t| by_town.partition_point(|&i| entries[i as usize].1 < t) as u32)
            .collect();
        let house_first: Vec<u32> = (0..=entries.len() as u32)
            .map(|e| houses.partition_point(|h| h.0 < e) as u32)
            .collect();
        let house_words: Vec<u32> = houses
            .iter()
            .flat_map(|&(_, number, post, x, y)| [x as u32, y as u32, add(number), add(post)])
            .collect();
        w.extend([0, 0, version, entries.len() as u32, towns.len() as u32]);
        w.push(text.len() as u32);
        if version > 1 {
            w.push(pois.len() as u32);
        }
        if version > 2 {
            w.extend([houses.len() as u32, words.len() as u32]);
        }
        w.extend(town_words);
        w.extend(entry_words);
        if version > 1 {
            w.extend(poi_words);
        }
        if version > 2 {
            w.extend(word_words);
            w.extend(town_first);
            w.extend(by_town);
            w.extend(house_first);
            w.extend(house_words);
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
