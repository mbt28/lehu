// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Fuzz targets for the host, in plain `cargo test`: no libFuzzer, no
//! nightly. Every parser that reads bytes from a phone, a network, the
//! kernel or a file gets the seeds it normally sees, every truncation of
//! them, then mutations of them (bit flips, insertions, deletions, length
//! fields set to the values that break parsers, splices of two seeds) and
//! purely random buffers. A target fails on a panic, on an input that
//! takes longer than its time budget, and on a property it states (an
//! output bounded by the input, a round trip); the failure names the input
//! in hex so it can be replayed.
//!
//! The generator is a SplitMix64 seeded from the target's name, so every
//! run of a target sees the same inputs: a failure is reproducible from
//! the test name alone. `LEHU_FUZZ_SEED` changes the stream for a long run.
//!
//! How many inputs a target sees: `DEFAULT_ITERS` for a cheap parser,
//! divided by the target's cost for an expensive one (a pairing step does
//! big-number work). `LEHU_FUZZ_ITERS` replaces `DEFAULT_ITERS`, so
//! `LEHU_FUZZ_ITERS=400000` runs every target twenty times longer.

use std::fmt::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
mod aa;
#[cfg(test)]
mod bugs;
#[cfg(test)]
mod cp;
#[cfg(test)]
mod files;
#[cfg(test)]
mod media;
#[cfg(test)]
mod system;
#[cfg(test)]
mod ultra;

/// Inputs per cheap target in a default run.
pub const DEFAULT_ITERS: usize = 20_000;

/// The longest one input may take before it counts as a hang.
pub const BUDGET: Duration = Duration::from_millis(200);

/// An input still running after this is a real hang: the watchdog prints
/// it and aborts the test binary, since a looping thread cannot be stopped.
const HANG: Duration = Duration::from_secs(20);

/// SplitMix64: small, fast and good enough to drive a mutator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Seeded from a name (FNV-1a), and `LEHU_FUZZ_SEED` when it is set.
    pub fn named(name: &str) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in name.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        if let Some(extra) = std::env::var("LEHU_FUZZ_SEED")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            h ^= extra.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        Self(h)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    /// True `num` times in `den`.
    pub fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }

    pub fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Values that sit on the edges parsers get wrong.
const INTERESTING: [u64; 18] = [
    0,
    1,
    2,
    3,
    4,
    7,
    8,
    0x10,
    0x7f,
    0x80,
    0xff,
    0x100,
    0x7fff,
    0x8000,
    0xffff,
    0x7fff_ffff,
    0xffff_ffff,
    u64::MAX,
];

/// The input under way and when it started, for the watchdog.
type Running = Option<(Instant, Vec<u8>)>;

/// What a target is fed: its seeds, the tokens of its format (tags,
/// header names, keys) for insertion, and the longest input it gets.
pub struct Target {
    pub name: &'static str,
    pub seeds: Vec<Vec<u8>>,
    pub dict: Vec<Vec<u8>>,
    pub max_len: usize,
    /// How many times dearer one input is than a cheap parser's.
    pub cost: usize,
    /// Panics already reported (a test in `bugs` shows each): an input
    /// that ends in one is counted, not failed, so the target goes on
    /// looking for something new.
    pub known: Vec<Known>,
}

/// A panic already found: the source file it is raised in (a path
/// suffix) and a piece of its message. Not the line: the crates under
/// test move.
#[derive(Clone, Copy, Debug)]
pub struct Known {
    pub file: &'static str,
    pub message: &'static str,
}

thread_local! {
    /// The panic sites this thread's target already knows.
    static KNOWN: std::cell::RefCell<Vec<Known>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Where this thread's last panic was raised, and whether it was known.
    static LAST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The panic hook: a known panic is noted and kept quiet (a long run hits
/// it thousands of times); any other goes to the default hook, which
/// prints where it was raised.
fn install_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let text = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_default();
            let file = info.location().map(|l| l.file()).unwrap_or("");
            let known = KNOWN.with(|k| {
                k.borrow()
                    .iter()
                    .any(|k| file.ends_with(k.file) && text.contains(k.message))
            });
            LAST.with(|l| l.set(known));
            if !known {
                default(info);
            }
        }));
    });
}

impl Target {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            seeds: Vec::new(),
            dict: Vec::new(),
            max_len: 4096,
            cost: 1,
            known: Vec::new(),
        }
    }

    /// A panic in `file` whose message holds `message` is a known bug.
    pub fn known(mut self, file: &'static str, message: &'static str) -> Self {
        self.known.push(Known { file, message });
        self
    }

    pub fn seed(mut self, s: impl Into<Vec<u8>>) -> Self {
        self.seeds.push(s.into());
        self
    }

    pub fn seeds<I: IntoIterator<Item = Vec<u8>>>(mut self, s: I) -> Self {
        self.seeds.extend(s);
        self
    }

    pub fn dict(mut self, words: &[&str]) -> Self {
        self.dict
            .extend(words.iter().map(|w| w.as_bytes().to_vec()));
        self
    }

    /// Tokens that are not text (a compression pointer, a marker).
    pub fn dict_bytes(mut self, words: &[&[u8]]) -> Self {
        self.dict.extend(words.iter().map(|w| w.to_vec()));
        self
    }

    pub fn max_len(mut self, n: usize) -> Self {
        self.max_len = n;
        self
    }

    pub fn cost(mut self, c: usize) -> Self {
        self.cost = c.max(1);
        self
    }

    /// The random inputs this target sees in this run.
    pub fn iterations(&self) -> usize {
        let base = std::env::var("LEHU_FUZZ_ITERS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_ITERS);
        (base / self.cost).max(1)
    }

    /// One input: a mutated seed most of the time, a random buffer (with
    /// tokens of the format in it, when there are any) otherwise.
    pub fn generate(&self, rng: &mut Rng, out: &mut Vec<u8>) {
        out.clear();
        if self.seeds.is_empty() || rng.chance(1, 12) {
            let n = rng.below(self.max_len.min(512) + 1);
            while out.len() < n {
                if !self.dict.is_empty() && rng.chance(1, 4) {
                    out.extend_from_slice(rng.pick(&self.dict));
                } else {
                    out.push(rng.byte());
                }
            }
            return;
        }
        out.extend_from_slice(rng.pick(&self.seeds));
        let rounds = 1 + rng.below(4);
        for _ in 0..rounds {
            self.mutate(rng, out);
        }
        out.truncate(self.max_len);
    }

    fn mutate(&self, rng: &mut Rng, b: &mut Vec<u8>) {
        let len = b.len();
        match rng.below(12) {
            // A bit.
            0 if len > 0 => {
                let i = rng.below(len);
                b[i] ^= 1 << rng.below(8);
            }
            // A byte set to an edge value or to anything.
            1 if len > 0 => {
                let i = rng.below(len);
                b[i] = *rng.pick(&INTERESTING) as u8;
            }
            2 if len > 0 => {
                let i = rng.below(len);
                b[i] = rng.byte();
            }
            // A byte nudged: off-by-one lengths and counts.
            3 if len > 0 => {
                let i = rng.below(len);
                let d = 1 + rng.below(4) as u8;
                b[i] = if rng.chance(1, 2) {
                    b[i].wrapping_add(d)
                } else {
                    b[i].wrapping_sub(d)
                };
            }
            // Bytes inserted.
            4 => {
                let at = rng.below(len + 1);
                let n = 1 + rng.below(16);
                let ins = rng.bytes(n);
                b.splice(at..at, ins);
            }
            // A range deleted.
            5 if len > 0 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(32));
                b.drain(at..at + n);
            }
            // A range repeated: repeated items, nested structures.
            6 if len > 0 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(64));
                let chunk = b[at..at + n].to_vec();
                let to = rng.below(len + 1);
                b.splice(to..to, chunk);
            }
            // A length field: 1, 2, 4 or 8 bytes, either order, set to an
            // edge value or to the distance to the end.
            7 if len > 0 => {
                let width = *rng.pick(&[1usize, 2, 4, 8]);
                let at = rng.below(len);
                let value = match rng.below(4) {
                    0 => (len - at) as u64,
                    1 => (len - at) as u64 + 1,
                    2 => (len - at).saturating_sub(1) as u64,
                    _ => *rng.pick(&INTERESTING),
                };
                let bytes = if rng.chance(1, 2) {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                };
                let src = if rng.chance(1, 2) {
                    &bytes[8 - width..]
                } else {
                    &bytes[..width]
                };
                for (k, &v) in src.iter().enumerate() {
                    if let Some(slot) = b.get_mut(at + k) {
                        *slot = v;
                    }
                }
            }
            // Cut short.
            8 if len > 0 => b.truncate(rng.below(len)),
            // Spliced with another seed.
            9 if !self.seeds.is_empty() => {
                let other = rng.pick(&self.seeds);
                let cut = rng.below(len + 1);
                let from = rng.below(other.len() + 1);
                b.truncate(cut);
                b.extend_from_slice(&other[from..]);
            }
            // A token of the format.
            10 if !self.dict.is_empty() => {
                let at = rng.below(len + 1);
                let word = rng.pick(&self.dict).clone();
                b.splice(at..at, word);
            }
            // Bytes overwritten in place by a run of one value.
            _ if len > 0 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(16));
                let any = rng.byte();
                let v = *rng.pick(&[0u8, 0xff, any]);
                b[at..at + n].fill(v);
            }
            _ => b.push(rng.byte()),
        }
    }

    /// Run `check` on every input; a panic, an input over the budget or a
    /// returned error fails the test with the input in hex.
    pub fn run<F: FnMut(&[u8]) -> Result<(), String>>(self, mut check: F) {
        let current: Arc<Mutex<Running>> = Arc::new(Mutex::new(None));
        let done = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let current = Arc::clone(&current);
            let done = Arc::clone(&done);
            let name = self.name;
            thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(100));
                    let slot = current.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some((start, input)) = slot.as_ref()
                        && start.elapsed() > HANG
                    {
                        eprintln!(
                            "{name}: input still running after {:?}, a hang ({} bytes): {}",
                            start.elapsed(),
                            input.len(),
                            hex(input)
                        );
                        std::process::abort();
                    }
                }
            })
        };

        install_hook();
        KNOWN.with(|k| *k.borrow_mut() = self.known.clone());
        let mut rng = Rng::named(self.name);
        let mut input = Vec::with_capacity(self.max_len);
        let mut index = 0usize;
        let mut slowest = Duration::ZERO;
        let mut known_hits = 0usize;
        let mut one = |input: &[u8], index: usize| {
            *current.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), input.to_vec()));
            let start = Instant::now();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| check(input)));
            let took = start.elapsed();
            *current.lock().unwrap_or_else(|e| e.into_inner()) = None;
            slowest = slowest.max(took);
            let why = match outcome {
                Err(_) if LAST.with(|l| l.replace(false)) => {
                    known_hits += 1;
                    None
                }
                Err(p) => Some(format!("panicked ({})", panic_text(&p))),
                Ok(Err(e)) => Some(e),
                Ok(Ok(())) if took > BUDGET => Some(format!("took {took:?}, over {BUDGET:?}")),
                Ok(Ok(())) => None,
            };
            if let Some(why) = why {
                done.store(true, Ordering::Release);
                panic!(
                    "{}: {why} on input #{index} ({} bytes): {}",
                    self.name,
                    input.len(),
                    hex(input)
                );
            }
        };

        // The seeds as they are, then every truncation of them.
        for s in &self.seeds {
            one(s, index);
            index += 1;
        }
        for s in &self.seeds {
            let step = (s.len() / 256).max(1);
            for cut in (0..s.len()).step_by(step) {
                one(&s[..cut], index);
                index += 1;
            }
        }
        let iterations = self.iterations();
        for _ in 0..iterations {
            self.generate(&mut rng, &mut input);
            one(&input, index);
            index += 1;
        }
        done.store(true, Ordering::Release);
        let _ = watchdog.join();
        KNOWN.with(|k| k.borrow_mut().clear());
        eprintln!(
            "{}: {index} inputs, slowest {slowest:?}, {known_hits} on known bugs",
            self.name
        );
    }
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "a non-text payload".into()
    }
}

/// Bytes as lower-case hex, the form a failure reports its input in.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Hex back to bytes, for replaying a reported input. Spaces are ignored.
pub fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .map(|b| (b as char).to_digit(16).unwrap_or(0) as u8)
        .collect();
    digits
        .chunks(2)
        .map(|p| (p[0] << 4) | p.get(1).copied().unwrap_or(0))
        .collect()
}

/// Fail unless `got` is at most `factor` times `input` plus `slack`: the
/// allocation bound of a parser that takes lengths from the wire.
pub fn bounded(
    what: &str,
    got: usize,
    input: usize,
    factor: usize,
    slack: usize,
) -> Result<(), String> {
    let limit = input.saturating_mul(factor).saturating_add(slack);
    if got > limit {
        return Err(format!(
            "{what}: {got} from {input} bytes of input, over {factor}x + {slack}"
        ));
    }
    Ok(())
}

/// Fail with `what` unless `ok`.
pub fn ensure(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what()) }
}

/// A scratch directory under the system's temporary one, removed on drop.
pub struct TempDir(std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lehu-fuzz-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A stream for the protocol loops: reads hand out `data` in chunks whose
/// sizes come from `rng` (a torn read is how a socket delivers), then end
/// of file; writes are kept.
pub struct Pipe {
    data: Vec<u8>,
    at: usize,
    rng: Rng,
    pub written: Vec<u8>,
}

impl Pipe {
    pub fn new(data: Vec<u8>, seed: u64) -> Self {
        Self {
            data,
            at: 0,
            rng: Rng::new(seed),
            written: Vec::new(),
        }
    }
}

impl std::io::Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.data.len() - self.at;
        if left == 0 {
            return Ok(0);
        }
        let n = (1 + self.rng.below(64)).min(left).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl std::io::Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A 64-bit fingerprint of an input, to derive a per-input stream (chunk
/// sizes) that stays the same when the input is replayed.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generator_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(a.bytes(64), b.bytes(64));
        assert_eq!(unhex(&hex(&[0, 1, 0xab, 0xff])), [0, 1, 0xab, 0xff]);
    }

    #[test]
    fn a_panic_is_reported_with_its_input() {
        let r = panic::catch_unwind(|| {
            Target::new("selftest")
                .seed(vec![1u8, 2, 3])
                .cost(DEFAULT_ITERS)
                .run(|b| {
                    if b == [1, 2, 3] {
                        panic!("found it");
                    }
                    Ok(())
                })
        });
        let text = panic_text(&r.expect_err("the target fails"));
        assert!(
            text.contains("found it") && text.ends_with("010203"),
            "{text}"
        );
    }
}
