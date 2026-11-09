// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What `lehu-os-update` prints, and its progress file: `name=value`
//! lines. A command that failed exits 1 with `message=` on its last line.

/// The `name=value` pairs of `text`, in order; other lines are skipped.
pub fn fields(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|line| {
        let (name, value) = line.split_once('=')?;
        let name = name.trim();
        (!name.is_empty() && !name.contains(' ')).then_some((name, value.trim()))
    })
}

/// How an update installs, as `check` says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// A new LEHU, beside the running system: about 45 s, then a reboot.
    #[default]
    InPlace,
    /// The OS: a reboot into the update boot, about 4 minutes.
    UpdateBoot,
}

impl Mode {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "inplace" => Some(Mode::InPlace),
            "update-boot" => Some(Mode::UpdateBoot),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Mode::InPlace => "inplace",
            Mode::UpdateBoot => "update-boot",
        }
    }
}

/// `lehu-os-update status`: the last update and the version running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// installed, failed, update-boot, installing, checking; empty when no
    /// update was ever made on this board.
    pub state: String,
    /// The last update's version and commit.
    pub version: String,
    pub commit: String,
    pub message: String,
    /// When the state was written (`2026-11-08T10:14:40Z`).
    pub time: String,
    /// The commit running, and its version.
    pub booted: String,
    pub booted_version: String,
}

impl Status {
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (name, value) in fields(text) {
            let slot = match name {
                "state" => &mut s.state,
                "version" => &mut s.version,
                "commit" => &mut s.commit,
                "message" => &mut s.message,
                "time" => &mut s.time,
                "booted" => &mut s.booted,
                "booted_version" => &mut s.booted_version,
                _ => continue,
            };
            *slot = value.to_string();
        }
        s
    }
}

/// `lehu-os-update check`: what the update is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    pub version: String,
    pub commit: String,
    pub booted: String,
    pub size_kb: u64,
    pub mode: Mode,
}

impl Checked {
    /// `None` when a line the screens need is missing.
    pub fn parse(text: &str) -> Option<Self> {
        let mut c = Self::default();
        let mut mode = None;
        for (name, value) in fields(text) {
            match name {
                "version" => c.version = value.to_string(),
                "commit" => c.commit = value.to_string(),
                "booted" => c.booted = value.to_string(),
                "size_kb" => c.size_kb = value.parse().unwrap_or(0),
                "mode" => mode = Mode::parse(value),
                _ => {}
            }
        }
        c.mode = mode?;
        (!c.version.is_empty()).then_some(c)
    }
}

/// Where `apply` (or the update boot) is: `/run/lehu-os-update/progress`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// 0..=100.
    pub percent: u32,
    /// checking, update-boot, receiving, installing, installed, failed.
    pub phase: String,
    pub message: String,
    pub version: String,
}

impl Progress {
    pub fn parse(text: &str) -> Self {
        let mut p = Self::default();
        for (name, value) in fields(text) {
            match name {
                "percent" => p.percent = value.parse::<u32>().unwrap_or(0).min(100),
                "phase" => p.phase = value.to_string(),
                "message" => p.message = value.to_string(),
                "version" => p.version = value.to_string(),
                _ => {}
            }
        }
        p
    }
}

/// Why a command failed, for the screen: its last `message=`, else the
/// last line it wrote to stderr, else its exit; as a sentence.
pub fn failure(stdout: &str, stderr: &str, exit: Option<i32>) -> String {
    let message = fields(stdout)
        .filter(|(name, _)| *name == "message")
        .map(|(_, value)| value)
        .last()
        .filter(|m| !m.is_empty())
        .or_else(|| stderr.lines().map(str::trim).rfind(|l| !l.is_empty()));
    match message {
        Some(m) => sentence(m),
        None => match exit {
            Some(code) => format!("The installer stopped (exit {code})"),
            None => "The installer was stopped".to_string(),
        },
    }
}

/// `text` starting with a capital, as a screen shows it.
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_after_an_update_and_on_a_fresh_board() {
        let s = Status::parse(
            "state=installed\nmessage=version 22.0.1-2 is installed; it starts at the next boot\n\
             version=22.0.1-2\ncommit=82f6\ntime=2026-11-08T10:14:40Z\nbooted=82f6\n\
             booted_version=22.0.1-2\n",
        );
        assert_eq!(s.state, "installed");
        assert_eq!(s.version, "22.0.1-2");
        assert_eq!(s.commit, "82f6");
        assert_eq!(s.time, "2026-11-08T10:14:40Z");
        assert_eq!(s.booted, s.commit);
        assert_eq!(s.booted_version, "22.0.1-2");
        assert!(s.message.ends_with("next boot"));

        let fresh = Status::parse("booted=6888\nbooted_version=22.0.1-1\n");
        assert_eq!(fresh.state, "");
        assert_eq!(fresh.booted, "6888");
    }

    #[test]
    fn check_needs_a_version_and_a_mode() {
        let c = Checked::parse(
            "version=22.0.1-2\ncommit=82f6\nbooted=6888\nsize_kb=37000\nmode=update-boot\n",
        )
        .unwrap();
        assert_eq!(c.mode, Mode::UpdateBoot);
        assert_eq!(c.size_kb, 37000);
        assert_eq!(
            Checked::parse("version=1\nmode=inplace").unwrap().mode,
            Mode::InPlace
        );
        assert_eq!(Checked::parse("version=1\nmode=sideways"), None);
        assert_eq!(Checked::parse("mode=inplace"), None);
        assert_eq!(Checked::parse(""), None);
    }

    #[test]
    fn progress_is_clamped_and_tolerant() {
        let p = Progress::parse(
            "percent=40\nphase=receiving\nmessage=Receiving the update: 11 of 27 parts\nversion=22\n",
        );
        assert_eq!(p.percent, 40);
        assert_eq!(p.phase, "receiving");
        assert_eq!(p.message, "Receiving the update: 11 of 27 parts");
        assert_eq!(Progress::parse("percent=250").percent, 100);
        assert_eq!(Progress::parse("percent=x\ngarbage\n=1").percent, 0);
        // A value may hold '=' itself.
        assert_eq!(Progress::parse("message=a=b").message, "a=b");
    }

    #[test]
    fn a_failure_says_why_in_words() {
        assert_eq!(
            failure(
                "version=1\nmessage=the update is not signed with LEHU OS's key\n",
                "",
                Some(1)
            ),
            "The update is not signed with LEHU OS's key"
        );
        assert_eq!(
            failure("", "sudo: a password is required\n\n", Some(1)),
            "Sudo: a password is required"
        );
        assert_eq!(failure("", "", Some(2)), "The installer stopped (exit 2)");
        assert_eq!(failure("", "", None), "The installer was stopped");
    }
}
