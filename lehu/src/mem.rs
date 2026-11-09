// SPDX-License-Identifier: GPL-3.0-or-later
//! The program's heap on a 64 MB board meant to run without swap: glibc's
//! allocator set up before any thread starts, given back what it holds
//! free when a session's video or the camera ends, and a line in the log
//! when the anonymous memory moves.

/// Set before any thread starts. At most two malloc arenas: each thread
/// would get its own (up to eight on this CPU), and what one frees the
/// others do not use. A fixed mmap threshold: glibc raises it to the size
/// of any mapped block that is freed, after which the 512 KB video units
/// came from the heap, zeroed whole (resident in full) and kept there once
/// freed; with 64 KB fixed they are mapped, cost the pages written, and go
/// back when freed. A fixed trim threshold: the heap's free top goes back
/// past 256 KB.
pub fn tune() {
    #[cfg(target_env = "gnu")]
    // SAFETY: plain parameter settings, made before any other thread runs.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
        libc::mallopt(libc::M_MMAP_THRESHOLD, 64 * 1024);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 256 * 1024);
    }
}

/// What the heap holds free goes back to the system.
pub fn trim() {
    #[cfg(target_env = "gnu")]
    // SAFETY: no arguments to get wrong; glibc locks its arenas itself.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// How often the anonymous memory is looked at.
const WATCH_MS: u64 = 10_000;
/// A move this big is worth a line.
const WATCH_STEP_KB: u64 = 256;

/// Logs the anonymous memory when it moved by `WATCH_STEP_KB` or more
/// since the last line: with what the heap holds in use and free, and what
/// is mapped outside it.
pub struct Watch {
    last_kb: u64,
    next_ms: u64,
}

impl Watch {
    pub fn new() -> Self {
        Self {
            last_kb: 0,
            next_ms: 0,
        }
    }

    pub fn tick(&mut self, now: u64) {
        if now < self.next_ms {
            return;
        }
        self.next_ms = now + WATCH_MS;
        let Some(anon) = anon_kb() else {
            return;
        };
        if anon.abs_diff(self.last_kb) < WATCH_STEP_KB {
            return;
        }
        self.last_kb = anon;
        log::info!("[Mem] anon {anon} kB{}", heap());
    }
}

/// `RssAnon` of this process.
fn anon_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("RssAnon:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_env = "gnu")]
fn heap() -> String {
    // SAFETY: returns a struct by value; glibc locks its arenas itself.
    let m = unsafe { libc::mallinfo2() };
    format!(
        ", heap {} kB in use + {} kB free, mapped {} kB",
        m.uordblks / 1024,
        m.fordblks / 1024,
        m.hblkhd / 1024
    )
}

#[cfg(not(target_env = "gnu"))]
fn heap() -> String {
    String::new()
}
