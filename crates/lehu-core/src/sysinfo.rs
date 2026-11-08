//! What the system is and what it does, from `/proc`: read when asked, for
//! the Info page, and never otherwise.

use std::fs;

/// The counters behind the Info page's memory and CPU lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Load {
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    /// This process's resident memory.
    pub own_rss_kb: u64,
    /// All CPUs' time so far, and the part of it idle or waiting, in ticks.
    pub cpu_total: u64,
    pub cpu_idle: u64,
    /// This process's CPU time so far, in ticks.
    pub own_ticks: u64,
}

/// `(total, available)` kB from `/proc/meminfo`.
fn parse_meminfo(text: &str) -> (u64, u64) {
    let (mut total, mut available) = (0, 0);
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let name = words.next();
        let value = words.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match name {
            Some("MemTotal:") => total = value,
            Some("MemAvailable:") => available = value,
            _ => {}
        }
    }
    (total, available)
}

/// `VmRSS` kB from `/proc/self/status`.
fn parse_rss(text: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// `(total, idle)` ticks from the first line of `/proc/stat`: idle counts
/// the time waiting for I/O too.
fn parse_stat(text: &str) -> (u64, u64) {
    let Some(line) = text.lines().find(|l| l.starts_with("cpu ")) else {
        return (0, 0);
    };
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
    (values.iter().sum(), idle)
}

/// User and system ticks from `/proc/self/stat`. The name in brackets may
/// hold anything, so the fields count from its closing bracket.
fn parse_self_stat(text: &str) -> u64 {
    let Some((_, rest)) = text.rsplit_once(')') else {
        return 0;
    };
    let mut fields = rest.split_whitespace().skip(11);
    let utime: u64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    utime + stime
}

/// `PRETTY_NAME`, else `NAME VERSION`, from `os-release`.
fn parse_os_release(text: &str) -> String {
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
            .filter(|v| !v.is_empty())
    };
    value("PRETTY_NAME").unwrap_or_else(|| {
        let name = value("NAME").unwrap_or_default();
        match value("VERSION") {
            Some(version) => format!("{name} {version}"),
            None => name,
        }
    })
}

/// The counters, now. What cannot be read counts as 0.
pub fn load() -> Load {
    let read = |path: &str| fs::read_to_string(path).unwrap_or_default();
    let (mem_total_kb, mem_available_kb) = parse_meminfo(&read("/proc/meminfo"));
    let (cpu_total, cpu_idle) = parse_stat(&read("/proc/stat"));
    Load {
        mem_total_kb,
        mem_available_kb,
        own_rss_kb: parse_rss(&read("/proc/self/status")),
        cpu_total,
        cpu_idle,
        own_ticks: parse_self_stat(&read("/proc/self/stat")),
    }
}

/// The kernel's release.
pub fn kernel() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The image's name.
pub fn os_name() -> String {
    fs::read_to_string("/etc/os-release")
        .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
        .map(|s| parse_os_release(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_and_rss() {
        let text = "MemTotal:       54000 kB\nMemFree:  1000 kB\nMemAvailable:   31000 kB\n";
        assert_eq!(parse_meminfo(text), (54000, 31000));
        assert_eq!(parse_meminfo(""), (0, 0));
        assert_eq!(parse_rss("Name:\tlehu\nVmRSS:\t   10272 kB\n"), 10272);
        assert_eq!(parse_rss("Name:\tlehu\n"), 0);
    }

    #[test]
    fn cpu_ticks() {
        let text = "cpu  10 1 5 80 4 0 0 0 0 0\ncpu0 10 1 5 80 4 0 0 0 0 0\n";
        assert_eq!(parse_stat(text), (100, 84));
        assert_eq!(parse_stat("intr 1\n"), (0, 0));
    }

    #[test]
    fn own_ticks_survive_a_strange_name() {
        let text = "269 (le) hu) S 1 269 269 0 -1 4 100 0 0 0 7 3 0 0 20 0 5 0 1 2 3";
        assert_eq!(parse_self_stat(text), 10);
        assert_eq!(parse_self_stat("garbage"), 0);
    }

    #[test]
    fn the_image_is_named() {
        assert_eq!(
            parse_os_release("NAME=Buildroot\nVERSION=2026.02\n"),
            "Buildroot 2026.02"
        );
        assert_eq!(
            parse_os_release("NAME=x\nPRETTY_NAME=\"LEHU OS 22\"\n"),
            "LEHU OS 22"
        );
        assert_eq!(parse_os_release("NAME=Buildroot\n"), "Buildroot");
        assert_eq!(parse_os_release(""), "");
    }

    #[test]
    fn this_machine_reads() {
        let l = load();
        assert!(l.mem_total_kb > 0 && l.cpu_total > 0 && l.own_rss_kb > 0);
        assert!(!kernel().is_empty());
    }
}
