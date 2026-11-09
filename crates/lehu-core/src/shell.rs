// SPDX-License-Identifier: GPL-3.0-or-later
//! The system's commands LEHU runs: the user's hooks as LEHU itself, and
//! the ones that need root (Reboot, Power off, the clock, the access
//! point's restart) through `sudo -n` when LEHU is not root.

use std::process::Command;

/// Whether this process runs as root.
pub fn is_root() -> bool {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `/bin/sh -c command`.
pub fn shell(command: &str) -> Command {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(command);
    c
}

/// A command that needs root. As root, or when it asks for sudo itself
/// (the image's rules name each command, `sudo -n /sbin/reboot`), the
/// shell runs it as it stands; otherwise the whole shell runs under
/// `sudo -n`, which fails at once rather than wait for a password.
pub fn privileged(command: &str) -> Command {
    privileged_as(command, is_root())
}

fn privileged_as(command: &str, root: bool) -> Command {
    if root || command.split_whitespace().next() == Some("sudo") {
        return shell(command);
    }
    let mut c = Command::new("sudo");
    c.args(["-n", "/bin/sh", "-c", command]);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(c: &Command) -> Vec<String> {
        std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn root_and_sudo_commands_run_as_they_stand() {
        assert_eq!(
            line(&privileged_as("reboot", true)),
            ["/bin/sh", "-c", "reboot"]
        );
        assert_eq!(
            line(&privileged_as("sudo -n /sbin/reboot", false)),
            ["/bin/sh", "-c", "sudo -n /sbin/reboot"]
        );
    }

    #[test]
    fn a_user_runs_the_whole_shell_under_sudo() {
        assert_eq!(
            line(&privileged_as("a stop; a start", false)),
            ["sudo", "-n", "/bin/sh", "-c", "a stop; a start"]
        );
        // A word that only starts with sudo is not it.
        assert_eq!(line(&privileged_as("sudoku", false))[0], "sudo");
    }
}
