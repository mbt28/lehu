// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! OPACK, Apple's compact binary object format: what the CarPlay Ultra
//! data streams (vehicle state, UI sync) carry. One tag byte per value,
//! small integers and short lengths folded into it, little-endian numbers,
//! containers counted up to 14 or open and ended by a terminator, and
//! back-references to values already read.
//!
//! The decoder refuses before it reads: every length is checked against
//! the bytes left, nesting is bounded, and back-references cannot make it
//! allocate more than a few times the message.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Data(Vec<u8>),
    Uuid([u8; 16]),
    Array(Vec<Value>),
    Dict(Vec<(Value, Value)>),
}

impl Value {
    pub fn str(s: &str) -> Self {
        Value::Str(s.into())
    }

    /// A dictionary's value under a string key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(entries) => entries
                .iter()
                .find(|(k, _)| matches!(k, Value::Str(s) if s == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
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
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Dict(entries) => Some(entries),
            _ => None,
        }
    }

    /// Values, and the bytes in strings and data: what a copy costs.
    pub fn weight(&self) -> usize {
        match self {
            Value::Str(s) => 1 + s.len(),
            Value::Data(d) => 1 + d.len(),
            Value::Array(a) => 1 + a.iter().map(Value::weight).sum::<usize>(),
            Value::Dict(d) => {
                1 + d
                    .iter()
                    .map(|(k, v)| k.weight() + v.weight())
                    .sum::<usize>()
            }
            _ => 1,
        }
    }
}

/// A dictionary with string keys.
pub fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dict(
        entries
            .into_iter()
            .map(|(k, v)| (Value::str(k), v))
            .collect(),
    )
}

const TRUE: u8 = 0x01;
const FALSE: u8 = 0x02;
const END: u8 = 0x03;
const NULL: u8 = 0x04;
const UUID: u8 = 0x05;
const DATE: u8 = 0x06;
const SMALL_INT: u8 = 0x08;
const SMALL_INT_MAX: i64 = 39;
const INT8: u8 = 0x30;
const FLOAT32: u8 = 0x35;
const FLOAT64: u8 = 0x36;
const STRING: u8 = 0x40;
const DATA: u8 = 0x70;
const SHORT_LEN: usize = 0x20;
const REF: u8 = 0xA0;
const REF_WIDE: u8 = 0xC0;
const ARRAY: u8 = 0xD0;
const DICT: u8 = 0xE0;
const SHORT_COUNT: usize = 14;
const OPEN: u8 = 0x0F;
/// Deeper than any message the phone sends.
const MAX_DEPTH: usize = 24;

pub fn decode(buf: &[u8]) -> Option<Value> {
    let mut reader = Reader {
        buf,
        at: 0,
        seen: Vec::new(),
        budget: buf.len().saturating_mul(4).saturating_add(4096),
    };
    let v = reader.value(0)?;
    (reader.at == buf.len()).then_some(v)
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
    /// The values a back-reference may name, in the order they came.
    seen: Vec<Value>,
    /// What back-references may still copy.
    budget: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.buf.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    fn unsigned(&mut self, width: usize) -> Option<u64> {
        if width > 8 {
            return None;
        }
        Some(
            self.take(width)?
                .iter()
                .rev()
                .fold(0, |acc, b| (acc << 8) | u64::from(*b)),
        )
    }

    fn signed(&mut self, width: usize) -> Option<i64> {
        let raw = self.unsigned(width)?;
        let shift = 64 - 8 * width as u32;
        Some(((raw << shift) as i64) >> shift)
    }

    /// A length folded into the tag, or in the 1 to 4 bytes after it.
    fn len(&mut self, tag: u8, base: u8) -> Option<usize> {
        let n = usize::from(tag - base);
        if n <= SHORT_LEN {
            return Some(n);
        }
        usize::try_from(self.unsigned(n - SHORT_LEN)?).ok()
    }

    fn remember(&mut self, v: Value) -> Value {
        self.seen.push(v.clone());
        v
    }

    fn reference(&mut self, index: usize) -> Option<Value> {
        let v = self.seen.get(index)?;
        self.budget = self.budget.checked_sub(v.weight())?;
        Some(v.clone())
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        let tag = *self.take(1)?.first()?;
        Some(match tag {
            TRUE => Value::Bool(true),
            FALSE => Value::Bool(false),
            NULL => Value::Null,
            UUID => {
                let v = Value::Uuid(self.take(16)?.try_into().ok()?);
                self.remember(v)
            }
            DATE | FLOAT64 => {
                let v = Value::Real(f64::from_bits(self.unsigned(8)?));
                self.remember(v)
            }
            FLOAT32 => {
                let v = Value::Real(f64::from(f32::from_bits(self.unsigned(4)? as u32)));
                self.remember(v)
            }
            SMALL_INT..INT8 => Value::Int(i64::from(tag - SMALL_INT)),
            0x30..=0x33 => {
                let v = Value::Int(self.signed(1 << (tag - INT8))?);
                self.remember(v)
            }
            0x40..=0x64 => {
                let n = self.len(tag, STRING)?;
                let v = Value::Str(String::from_utf8_lossy(self.take(n)?).into_owned());
                self.remember(v)
            }
            0x70..=0x94 => {
                let n = self.len(tag, DATA)?;
                let v = Value::Data(self.take(n)?.to_vec());
                self.remember(v)
            }
            REF..=REF_WIDE => self.reference(usize::from(tag - REF))?,
            0xC1..=0xC4 => {
                let index = usize::try_from(self.unsigned(usize::from(tag - REF_WIDE))?).ok()?;
                self.reference(index)?
            }
            0xD0..=0xDF => {
                let count = (tag - ARRAY != OPEN).then_some(usize::from(tag - ARRAY));
                let mut items = Vec::new();
                while count.is_none_or(|n| items.len() < n) {
                    if count.is_none() && self.buf.get(self.at) == Some(&END) {
                        self.at += 1;
                        break;
                    }
                    items.push(self.value(depth + 1)?);
                }
                Value::Array(items)
            }
            0xE0..=0xEF => {
                let count = (tag - DICT != OPEN).then_some(usize::from(tag - DICT));
                let mut entries = Vec::new();
                while count.is_none_or(|n| entries.len() < n) {
                    if count.is_none() && self.buf.get(self.at) == Some(&END) {
                        self.at += 1;
                        break;
                    }
                    let key = self.value(depth + 1)?;
                    entries.push((key, self.value(depth + 1)?));
                }
                Value::Dict(entries)
            }
            _ => return None,
        })
    }
}

pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, v);
    out
}

fn write_len(out: &mut Vec<u8>, base: u8, n: usize) {
    if n <= SHORT_LEN {
        out.push(base + n as u8);
        return;
    }
    let width = match n {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    out.push(base + SHORT_LEN as u8 + width as u8);
    out.extend_from_slice(&(n as u64).to_le_bytes()[..width]);
}

/// The count in the tag, or open: true when a terminator must follow.
fn write_count(out: &mut Vec<u8>, base: u8, n: usize) -> bool {
    if n <= SHORT_COUNT {
        out.push(base + n as u8);
        false
    } else {
        out.push(base + OPEN);
        true
    }
}

fn write(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(NULL),
        Value::Bool(b) => out.push(if *b { TRUE } else { FALSE }),
        Value::Int(n @ 0..=SMALL_INT_MAX) => out.push(SMALL_INT + *n as u8),
        Value::Int(n) => {
            let width: usize = if i8::try_from(*n).is_ok() {
                1
            } else if i16::try_from(*n).is_ok() {
                2
            } else if i32::try_from(*n).is_ok() {
                4
            } else {
                8
            };
            out.push(INT8 + width.trailing_zeros() as u8);
            out.extend_from_slice(&n.to_le_bytes()[..width]);
        }
        Value::Real(f) => {
            out.push(FLOAT64);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Str(s) => {
            write_len(out, STRING, s.len());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Data(d) => {
            write_len(out, DATA, d.len());
            out.extend_from_slice(d);
        }
        Value::Uuid(u) => {
            out.push(UUID);
            out.extend_from_slice(u);
        }
        Value::Array(items) => {
            let open = write_count(out, ARRAY, items.len());
            for item in items {
                write(out, item);
            }
            if open {
                out.push(END);
            }
        }
        Value::Dict(entries) => {
            let open = write_count(out, DICT, entries.len());
            for (k, v) in entries {
                write(out, k);
                write(out, v);
            }
            if open {
                out.push(END);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_go_out_and_come_back() {
        let long = "x".repeat(300);
        let v = Value::Array(vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(39),
            Value::Int(40),
            Value::Int(-1),
            Value::Int(-6727),
            Value::Int(0x1_0000),
            Value::Int(i64::MIN),
            Value::Real(1.5),
            Value::str(""),
            Value::str(&long),
            Value::Data(vec![1; 33]),
            Value::Uuid([7; 16]),
            dict([("command", Value::str("configRequest"))]),
            Value::Array((0..20).map(Value::Int).collect()),
            Value::Dict((0..20).map(|n| (Value::Int(n), Value::Null)).collect()),
        ]);
        assert_eq!(decode(&encode(&v)), Some(v));
    }

    #[test]
    fn the_encoding_is_the_one_the_phone_writes() {
        assert_eq!(encode(&Value::Int(5)), [0x0d]);
        assert_eq!(encode(&Value::Int(-1)), [0x30, 0xff]);
        assert_eq!(encode(&Value::Int(300)), [0x31, 0x2c, 0x01]);
        assert_eq!(encode(&Value::str("ab")), [0x42, b'a', b'b']);
        assert_eq!(
            encode(&dict([("a", Value::Bool(true))])),
            [0xe1, 0x41, b'a', 0x01]
        );
        // An open array, ended.
        assert_eq!(
            decode(&[0xdf, 0x09, 0x0a, 0x03]),
            Some(Value::Array(vec![Value::Int(1), Value::Int(2)]))
        );
        // A back-reference to the first string.
        assert_eq!(
            decode(&[0xd2, 0x41, b'k', 0xa0]),
            Some(Value::Array(vec![Value::str("k"), Value::str("k")]))
        );
    }

    #[test]
    fn damaged_or_hostile_input_is_refused() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[0x45, b'a']), None, "a string cut short");
        assert_eq!(decode(&[0x61, 0xff]), None, "a length past the end");
        assert_eq!(decode(&[0x64, 0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(decode(&[0xa0]), None, "a reference to nothing");
        assert_eq!(decode(&[0xdf, 0x09]), None, "an open array never ended");
        assert_eq!(decode(&[0x09, 0x09]), None, "bytes after the value");
        assert_eq!(decode(&[0xff]), None);
        let deep = vec![0xd1; 100];
        assert_eq!(decode(&deep), None, "nesting is bounded");
        // A big string copied by many references runs out of budget.
        let mut bomb = vec![0xdf, 0x62, 0x00, 0x10];
        bomb.extend(std::iter::repeat_n(b'z', 0x1000));
        bomb.extend(std::iter::repeat_n(0xa0, 2000));
        bomb.push(0x03);
        assert_eq!(decode(&bomb), None);
    }
}
