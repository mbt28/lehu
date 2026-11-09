// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! Apple property lists: the binary form (`bplist00`) every CarPlay AV
//! message is, and the XML form lockdown speaks. Both decoders check every
//! offset against the buffer before they read it, so a corrupt message is
//! refused, never indexed.
//!
//! The binary encoder writes Apple's canonical layout: dictionary keys
//! sorted, every key object before every value object, and a trailer of
//! exactly 32 bytes. The phone's parser is strict about all three (a
//! lenient layout made it tear the session down after RECORD).
//!
//! The binary form follows LIVI's cp/stack/bplist.ts and
//! livi-cp/src/bplist.rs (Lasse Heitgres, GPL-3.0), through FastCarPlay's
//! cp_plist.cpp; the XML form is LEHU's.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Data(Vec<u8>),
    Array(Vec<Value>),
    /// Ordered key/value pairs; the order is kept on the XML side and
    /// sorted on the binary side.
    Dict(Vec<(String, Value)>),
}

impl Value {
    pub fn dict() -> Self {
        Value::Dict(Vec::new())
    }

    pub fn str(s: &str) -> Self {
        Value::Str(s.to_string())
    }

    /// Add a key to a dictionary (anything else is left alone).
    pub fn set(&mut self, key: &str, v: Value) -> &mut Self {
        if let Value::Dict(d) = self {
            if let Some(slot) = d.iter_mut().find(|(k, _)| k == key) {
                slot.1 = v;
            } else {
                d.push((key.to_string(), v));
            }
        }
        self
    }

    pub fn with(mut self, key: &str, v: Value) -> Self {
        self.set(key, v);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_data(&self) -> Option<&[u8]> {
        match self {
            Value::Data(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn int_or(&self, key: &str, default: i64) -> i64 {
        self.get(key).and_then(Value::as_i64).unwrap_or(default)
    }

    pub fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).and_then(Value::as_str).unwrap_or(default)
    }

    pub fn bool_of(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::as_bool)
    }
}

// ── Binary encoding ───────────────────────────────────────────────────────

/// One object of the flattened tree, referenced by index.
enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Ascii(String),
    Utf16(String),
    Data(Vec<u8>),
    Array(Vec<usize>),
    Dict(Vec<(usize, usize)>),
}

fn flatten(v: &Value, objs: &mut Vec<Obj>) -> usize {
    let idx = objs.len();
    let obj = match v {
        Value::Null => Obj::Null,
        Value::Bool(b) => Obj::Bool(*b),
        Value::Int(i) => Obj::Int(*i),
        Value::Real(r) => Obj::Real(*r),
        Value::Str(s) => {
            if s.is_ascii() {
                Obj::Ascii(s.clone())
            } else {
                Obj::Utf16(s.clone())
            }
        }
        Value::Data(d) => Obj::Data(d.clone()),
        Value::Array(_) => Obj::Array(Vec::new()),
        Value::Dict(_) => Obj::Dict(Vec::new()),
    };
    objs.push(obj);
    match v {
        Value::Array(items) => {
            let refs: Vec<usize> = items.iter().map(|c| flatten(c, objs)).collect();
            objs[idx] = Obj::Array(refs);
        }
        Value::Dict(pairs) => {
            let mut order: Vec<usize> = (0..pairs.len()).collect();
            order.sort_by(|a, b| pairs[*a].0.cmp(&pairs[*b].0));
            let keys: Vec<usize> = order
                .iter()
                .map(|&k| flatten(&Value::Str(pairs[k].0.clone()), objs))
                .collect();
            let vals: Vec<usize> = order.iter().map(|&k| flatten(&pairs[k].1, objs)).collect();
            objs[idx] = Obj::Dict(keys.into_iter().zip(vals).collect());
        }
        _ => {}
    }
    idx
}

fn put_be(out: &mut Vec<u8>, v: u64, n: usize) {
    for i in (0..n).rev() {
        out.push((v >> (8 * i)) as u8);
    }
}

fn put_marker(out: &mut Vec<u8>, kind: u8, len: usize) {
    if len < 15 {
        out.push((kind << 4) | len as u8);
        return;
    }
    out.push((kind << 4) | 0x0f);
    if len <= 0xff {
        out.push(0x10);
        put_be(out, len as u64, 1);
    } else if len <= 0xffff {
        out.push(0x11);
        put_be(out, len as u64, 2);
    } else {
        out.push(0x12);
        put_be(out, len as u64, 4);
    }
}

fn byte_width(max: u64) -> usize {
    if max <= 0xff {
        1
    } else if max <= 0xffff {
        2
    } else if max <= 0xffff_ffff {
        4
    } else {
        8
    }
}

fn encode_obj(o: &Obj, out: &mut Vec<u8>, ref_size: usize) {
    match o {
        Obj::Null => out.push(0x00),
        Obj::Bool(b) => out.push(if *b { 0x09 } else { 0x08 }),
        Obj::Int(i) => {
            let u = *i as u64;
            if (0..=0xff).contains(i) {
                out.push(0x10);
                put_be(out, u, 1);
            } else if (0..=0xffff).contains(i) {
                out.push(0x11);
                put_be(out, u, 2);
            } else if (0..=0xffff_ffff).contains(i) {
                out.push(0x12);
                put_be(out, u, 4);
            } else {
                // Eight bytes carry the negatives too.
                out.push(0x13);
                put_be(out, u, 8);
            }
        }
        Obj::Real(r) => {
            out.push(0x23);
            put_be(out, r.to_bits(), 8);
        }
        Obj::Ascii(s) => {
            put_marker(out, 0x5, s.len());
            out.extend_from_slice(s.as_bytes());
        }
        Obj::Utf16(s) => {
            let units: Vec<u16> = s.encode_utf16().collect();
            put_marker(out, 0x6, units.len());
            for u in units {
                put_be(out, u64::from(u), 2);
            }
        }
        Obj::Data(d) => {
            put_marker(out, 0x4, d.len());
            out.extend_from_slice(d);
        }
        Obj::Array(refs) => {
            put_marker(out, 0xa, refs.len());
            for r in refs {
                put_be(out, *r as u64, ref_size);
            }
        }
        Obj::Dict(pairs) => {
            put_marker(out, 0xd, pairs.len());
            for (k, _) in pairs {
                put_be(out, *k as u64, ref_size);
            }
            for (_, v) in pairs {
                put_be(out, *v as u64, ref_size);
            }
        }
    }
}

/// Encode a value tree as `bplist00`.
pub fn encode(root: &Value) -> Vec<u8> {
    let mut objs = Vec::new();
    flatten(root, &mut objs);
    let count = objs.len();
    let ref_size = byte_width(count.saturating_sub(1) as u64);

    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(count);
    for o in &objs {
        offsets.push(out.len() as u64);
        encode_obj(o, &mut out, ref_size);
    }
    let table = out.len() as u64;
    let offset_size = byte_width(table);
    for off in offsets {
        put_be(&mut out, off, offset_size);
    }
    // The trailer: five unused bytes, the sort version, the two widths, then
    // the counts. A sixth unused byte would make it 33 bytes, and the phone
    // rejects a table that does not end exactly 32 bytes before the end.
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    out.push(offset_size as u8);
    out.push(ref_size as u8);
    put_be(&mut out, count as u64, 8);
    put_be(&mut out, 0, 8);
    put_be(&mut out, table, 8);
    out
}

// ── Binary decoding ───────────────────────────────────────────────────────

/// A value tree nests at most this deep; deeper is a loop or an attack.
const MAX_DEPTH: u32 = 32;

struct Decoder<'a> {
    b: &'a [u8],
    offset_size: usize,
    ref_size: usize,
    count: u64,
    table: u64,
    /// Objects still allowed to be decoded. A reference is followed every
    /// time it appears, so objects that refer to a shared child twice,
    /// level under level, decode exponentially many values from a few
    /// bytes. Every reference takes at least a byte of the buffer, so a
    /// plist decodes at most its length in values; past that it is an
    /// attack.
    budget: std::cell::Cell<usize>,
}

impl Decoder<'_> {
    fn be(&self, pos: usize, n: usize) -> Option<u64> {
        let end = pos.checked_add(n)?;
        let bytes = self.b.get(pos..end)?;
        Some(bytes.iter().fold(0u64, |v, &x| (v << 8) | u64::from(x)))
    }

    fn offset_of(&self, index: u64) -> Option<usize> {
        if index >= self.count {
            return None;
        }
        let pos = usize::try_from(
            self.table
                .checked_add(index.checked_mul(self.offset_size as u64)?)?,
        )
        .ok()?;
        usize::try_from(self.be(pos, self.offset_size)?).ok()
    }

    /// The length of a marker's object, resolving the extended form; `pos`
    /// moves past the length int.
    fn length(&self, pos: &mut usize, low: u8) -> Option<usize> {
        if low != 0x0f {
            return Some(usize::from(low));
        }
        let m = *self.b.get(*pos)?;
        *pos += 1;
        if m >> 4 != 0x1 || (m & 0x0f) > 3 {
            return None;
        }
        let n = 1usize << (m & 0x0f);
        let len = self.be(*pos, n)?;
        *pos += n;
        usize::try_from(len).ok()
    }

    fn parse(&self, index: u64, depth: u32) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        let left = self.budget.get().checked_sub(1)?;
        self.budget.set(left);
        let mut pos = self.offset_of(index)?;
        let marker = *self.b.get(pos)?;
        pos += 1;
        let (hi, lo) = (marker >> 4, marker & 0x0f);
        Some(match hi {
            0x0 => match marker {
                0x08 => Value::Bool(false),
                0x09 => Value::Bool(true),
                _ => Value::Null,
            },
            0x1 => {
                if lo > 3 {
                    return None;
                }
                let n = 1usize << lo;
                Value::Int(self.be(pos, n)? as i64)
            }
            0x2 => match lo {
                3 => Value::Real(f64::from_bits(self.be(pos, 8)?)),
                2 => Value::Real(f64::from(f32::from_bits(self.be(pos, 4)? as u32))),
                _ => return None,
            },
            0x4 => {
                let len = self.length(&mut pos, lo)?;
                Value::Data(self.b.get(pos..pos.checked_add(len)?)?.to_vec())
            }
            0x5 => {
                let len = self.length(&mut pos, lo)?;
                let s = self.b.get(pos..pos.checked_add(len)?)?;
                Value::Str(String::from_utf8_lossy(s).into_owned())
            }
            0x6 => {
                let units = self.length(&mut pos, lo)?;
                let end = pos.checked_add(units.checked_mul(2)?)?;
                let s = self.b.get(pos..end)?;
                let u16s: Vec<u16> = s
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                Value::Str(String::from_utf16_lossy(&u16s))
            }
            0xa => {
                let n = self.length(&mut pos, lo)?;
                // The reference table must be inside the buffer before any
                // child is followed.
                self.b
                    .get(pos..pos.checked_add(n.checked_mul(self.ref_size)?)?)?;
                let mut items = Vec::with_capacity(n.min(256));
                for k in 0..n {
                    let r = self.be(pos + k * self.ref_size, self.ref_size)?;
                    items.push(self.parse(r, depth + 1)?);
                }
                Value::Array(items)
            }
            0xd => {
                let n = self.length(&mut pos, lo)?;
                self.b
                    .get(pos..pos.checked_add(n.checked_mul(2)?.checked_mul(self.ref_size)?)?)?;
                let mut pairs = Vec::with_capacity(n.min(256));
                for k in 0..n {
                    let kr = self.be(pos + k * self.ref_size, self.ref_size)?;
                    let vr = self.be(pos + (n + k) * self.ref_size, self.ref_size)?;
                    let key = match self.parse(kr, depth + 1)? {
                        Value::Str(s) => s,
                        _ => String::new(),
                    };
                    pairs.push((key, self.parse(vr, depth + 1)?));
                }
                Value::Dict(pairs)
            }
            _ => return None,
        })
    }
}

/// Decode a `bplist00` buffer. `None` for anything malformed.
pub fn decode(buf: &[u8]) -> Option<Value> {
    if buf.len() < 8 + 32 || &buf[..8] != b"bplist00" {
        return None;
    }
    let trailer = buf.len() - 32;
    let mut d = Decoder {
        b: buf,
        offset_size: usize::from(buf[trailer + 6]),
        ref_size: usize::from(buf[trailer + 7]),
        count: 0,
        table: 0,
        budget: std::cell::Cell::new(buf.len()),
    };
    d.count = d.be(trailer + 8, 8)?;
    let top = d.be(trailer + 16, 8)?;
    d.table = d.be(trailer + 24, 8)?;
    if !matches!(d.offset_size, 1 | 2 | 4 | 8) || !matches!(d.ref_size, 1 | 2 | 4 | 8) {
        return None;
    }
    if d.count == 0 || d.count > (buf.len() as u64) {
        return None;
    }
    d.parse(top, 0)
}

// ── XML ───────────────────────────────────────────────────────────────────

fn xml_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

fn xml_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("<string></string>"),
        Value::Bool(true) => out.push_str("<true/>"),
        Value::Bool(false) => out.push_str("<false/>"),
        Value::Int(i) => {
            let _ = write!(out, "<integer>{i}</integer>");
        }
        Value::Real(r) => {
            let _ = write!(out, "<real>{r}</real>");
        }
        Value::Str(s) => {
            out.push_str("<string>");
            xml_escape(s, out);
            out.push_str("</string>");
        }
        Value::Data(d) => {
            out.push_str("<data>");
            out.push_str(&base64_encode(d));
            out.push_str("</data>");
        }
        Value::Array(a) => {
            out.push_str("<array>");
            for item in a {
                xml_value(item, out);
            }
            out.push_str("</array>");
        }
        Value::Dict(d) => {
            out.push_str("<dict>");
            for (k, item) in d {
                out.push_str("<key>");
                xml_escape(k, out);
                out.push_str("</key>");
                xml_value(item, out);
            }
            out.push_str("</dict>");
        }
    }
}

/// Encode as an XML property list, the form lockdown and usbmuxd use.
pub fn encode_xml(root: &Value) -> Vec<u8> {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n",
    );
    xml_value(root, &mut out);
    out.push_str("\n</plist>\n");
    out.into_bytes()
}

/// A tiny XML tokenizer: enough for property lists, which use no
/// attributes that matter, no namespaces and no entities beyond the five.
struct Xml<'a> {
    s: &'a str,
}

enum Tok<'a> {
    Open(&'a str),
    Close(&'a str),
    Empty(&'a str),
    Text(&'a str),
}

impl<'a> Xml<'a> {
    fn next(&mut self) -> Option<Tok<'a>> {
        loop {
            let s = self.s;
            if s.is_empty() {
                return None;
            }
            if let Some(rest) = s.strip_prefix('<') {
                let end = rest.find('>')?;
                let tag = &rest[..end];
                self.s = &rest[end + 1..];
                if tag.starts_with('?') || tag.starts_with('!') {
                    continue;
                }
                if let Some(name) = tag.strip_prefix('/') {
                    return Some(Tok::Close(name.trim()));
                }
                let name = tag.split(|c: char| c.is_whitespace() || c == '/').next()?;
                if tag.ends_with('/') {
                    return Some(Tok::Empty(name));
                }
                return Some(Tok::Open(name));
            }
            let end = s.find('<').unwrap_or(s.len());
            let text = &s[..end];
            self.s = &s[end..];
            if !text.trim().is_empty() {
                return Some(Tok::Text(text));
            }
        }
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_text_until(x: &mut Xml, close: &str) -> Option<String> {
    let mut text = String::new();
    loop {
        match x.next()? {
            Tok::Text(t) => text.push_str(t),
            Tok::Close(c) if c == close => return Some(text),
            _ => return None,
        }
    }
}

fn xml_parse(x: &mut Xml, tok: Tok, depth: u32) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    Some(match tok {
        Tok::Empty("true") => Value::Bool(true),
        Tok::Empty("false") => Value::Bool(false),
        Tok::Empty("dict") => Value::dict(),
        Tok::Empty("array") => Value::Array(Vec::new()),
        Tok::Empty("string") | Tok::Empty("data") => Value::Str(String::new()),
        Tok::Open("string") => Value::Str(xml_unescape(&xml_text_until(x, "string")?)),
        Tok::Open("date") => Value::Str(xml_text_until(x, "date")?),
        Tok::Open("integer") => Value::Int(xml_text_until(x, "integer")?.trim().parse().ok()?),
        Tok::Open("real") => Value::Real(xml_text_until(x, "real")?.trim().parse().ok()?),
        Tok::Open("data") => Value::Data(base64_decode(&xml_text_until(x, "data")?)?),
        Tok::Open("array") => {
            let mut items = Vec::new();
            loop {
                match x.next()? {
                    Tok::Close("array") => break,
                    t => items.push(xml_parse(x, t, depth + 1)?),
                }
            }
            Value::Array(items)
        }
        Tok::Open("dict") => {
            let mut pairs = Vec::new();
            loop {
                match x.next()? {
                    Tok::Close("dict") => break,
                    Tok::Open("key") => {
                        let key = xml_unescape(&xml_text_until(x, "key")?);
                        let t = x.next()?;
                        pairs.push((key, xml_parse(x, t, depth + 1)?));
                    }
                    Tok::Empty("key") => {
                        let t = x.next()?;
                        pairs.push((String::new(), xml_parse(x, t, depth + 1)?));
                    }
                    _ => return None,
                }
            }
            Value::Dict(pairs)
        }
        _ => return None,
    })
}

/// Decode an XML property list.
pub fn decode_xml(buf: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut x = Xml { s: text };
    match x.next()? {
        Tok::Open("plist") => {}
        _ => return None,
    }
    let t = x.next()?;
    xml_parse(&mut x, t, 0)
}

/// Decode either form, by the magic.
pub fn decode_any(buf: &[u8]) -> Option<Value> {
    if buf.starts_with(b"bplist00") {
        decode(buf)
    } else {
        decode_xml(buf)
    }
}

// ── base64, for the XML data elements ─────────────────────────────────────

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        Value::dict()
            .with("zeta", Value::Int(-5))
            .with("alpha", Value::Bool(true))
            .with("name", Value::str("LEHU"))
            .with("wide", Value::str("Üñí"))
            .with("data", Value::Data(vec![1, 2, 3, 0xff]))
            .with("big", Value::Int(0x1_0000_0000))
            .with("real", Value::Real(1.5))
            .with(
                "list",
                Value::Array(vec![
                    Value::Int(7),
                    Value::dict().with("k", Value::Int(300)),
                ]),
            )
            .with("long", Value::Data(vec![9; 300]))
    }

    #[test]
    fn binary_round_trip() {
        let v = sample();
        let bytes = encode(&v);
        let back = decode(&bytes).expect("decodes");
        // Keys come back sorted.
        assert_eq!(back.get("zeta"), Some(&Value::Int(-5)));
        assert_eq!(back.get("wide"), Some(&Value::str("Üñí")));
        assert_eq!(back.get("big"), Some(&Value::Int(0x1_0000_0000)));
        assert_eq!(back.get("real"), Some(&Value::Real(1.5)));
        assert_eq!(
            back.get("long").and_then(Value::as_data).map(<[u8]>::len),
            Some(300)
        );
        let list = back.get("list").and_then(Value::as_array).expect("array");
        assert_eq!(list[1].int_or("k", 0), 300);
        if let Value::Dict(d) = &back {
            let keys: Vec<&str> = d.iter().map(|(k, _)| k.as_str()).collect();
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            assert_eq!(keys, sorted);
        } else {
            panic!("not a dict");
        }
    }

    #[test]
    fn binary_layout_is_canonical() {
        let bytes = encode(
            &Value::dict()
                .with("b", Value::Int(1))
                .with("a", Value::Int(2)),
        );
        assert_eq!(&bytes[..8], b"bplist00");
        let trailer = bytes.len() - 32;
        // Five unused bytes, sort version, widths, count 5, top 0.
        assert_eq!(&bytes[trailer..trailer + 8], &[0, 0, 0, 0, 0, 0, 1, 1]);
        assert_eq!(bytes[trailer + 15], 5);
        let table = u64::from_be_bytes(bytes[trailer + 24..trailer + 32].try_into().unwrap());
        assert_eq!(table as usize + 5, trailer);
        // Dict object, then both keys ("a" before "b"), then both values.
        assert_eq!(bytes[8], 0xd2);
        assert_eq!(&bytes[13..15], &[0x51, b'a']);
        assert_eq!(&bytes[15..17], &[0x51, b'b']);
    }

    #[test]
    fn malformed_binary_is_refused() {
        assert!(decode(b"bplist00").is_none());
        let good = encode(&sample());
        for cut in [10, 40, good.len() - 1] {
            assert!(decode(&good[..cut]).is_none() || cut == good.len() - 1);
        }
        // An offset table pointing past the end.
        let mut bad = good.clone();
        let t = bad.len() - 32;
        let past = bad.len() as u64;
        bad[t + 24..t + 32].copy_from_slice(&past.to_be_bytes());
        assert!(decode(&bad).is_none());
        // An object count far beyond the buffer.
        let mut bad = good.clone();
        bad[t + 8..t + 16].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(decode(&bad).is_none());
        // A self-referencing array must not recurse forever.
        let mut bad = good;
        bad[t + 16..t + 24].copy_from_slice(&0u64.to_be_bytes());
        let _ = decode(&bad);
    }

    #[test]
    fn xml_round_trip_and_reads_real_records() {
        let v = sample();
        let xml = encode_xml(&v);
        let back = decode_xml(&xml).expect("xml decodes");
        assert_eq!(back, v);
        let record = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>HostID</key>
	<string>437BECA5-056E-4D84-247E-9AAC939F034A</string>
	<key>EscrowBag</key>
	<data>
	AQID
	</data>
	<key>Flag</key>
	<true/>
	<key>Empty</key>
	<dict/>
	<key>Port</key>
	<integer>62078</integer>
	<key>Label &amp; co</key>
	<string>a &lt; b</string>
</dict>
</plist>
"#;
        let v = decode_any(record).expect("record");
        assert_eq!(
            v.str_or("HostID", ""),
            "437BECA5-056E-4D84-247E-9AAC939F034A"
        );
        assert_eq!(
            v.get("EscrowBag").and_then(Value::as_data),
            Some(&[1u8, 2, 3][..])
        );
        assert_eq!(v.bool_of("Flag"), Some(true));
        assert_eq!(v.int_or("Port", 0), 62078);
        assert_eq!(v.str_or("Label & co", ""), "a < b");
        assert!(decode_xml(b"<plist><dict><key>x</key></dict></plist>").is_none());
        assert!(decode_xml(b"<html/>").is_none());
    }

    #[test]
    fn base64() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert!(base64_decode("Zg!=").is_none());
    }
}
