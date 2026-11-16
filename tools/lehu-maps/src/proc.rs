// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Running the Python scripts: their output line by line as it comes, and
//! Cancel as SIGTERM (then a kill, when they do not stop).

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::util::{Cancel, Error, Result};

/// Where a line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

/// A command for the platform: no console window on Windows.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn lines(r: impl Read + Send + 'static, s: Stream, tx: mpsc::Sender<(Stream, String)>) {
    std::thread::spawn(move || {
        let mut r = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // pip draws its bars with \r: each its own line.
                    let text = String::from_utf8_lossy(&buf);
                    for part in text.split(['\r', '\n']).filter(|p| !p.trim().is_empty()) {
                        if tx.send((s, part.to_string())).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
}

/// Ask a child to stop, as Ctrl-C would; killed after `grace`.
fn stop(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: a signal to our own child, which has not been waited
            // for, so the pid is still its.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let t = Instant::now();
        while t.elapsed() < grace {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    #[cfg(not(unix))]
    let _ = grace;
    let _ = child.kill();
    let _ = child.wait();
}

/// Run `cmd` to its end, each line of its output to `on_line`. Cancel
/// stops it. An error when it cannot start or exits other than 0 (with
/// its last lines of stderr, or `on_line`'s error).
pub fn run(
    mut cmd: Command,
    cancel: &Cancel,
    on_line: &mut dyn FnMut(Stream, &str) -> Result<()>,
) -> Result<()> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env("PYTHONUNBUFFERED", "1");
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Msg(format!("could not start {name}: {e}")))?;
    let (tx, rx) = mpsc::channel();
    lines(child.stdout.take().expect("piped"), Stream::Out, tx.clone());
    lines(child.stderr.take().expect("piped"), Stream::Err, tx);
    let mut tail: Vec<String> = Vec::new();
    let mut failed: Option<Error> = None;
    loop {
        if cancel.is_set() {
            stop(&mut child, Duration::from_secs(10));
            return Err(Error::Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((s, line)) => {
                if s == Stream::Err {
                    tail.push(line.clone());
                    if tail.len() > 4 {
                        tail.remove(0);
                    }
                }
                if failed.is_none()
                    && let Err(e) = on_line(s, &line)
                {
                    failed = Some(e);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // Both pipes closed: it has ended, or soon will.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = loop {
        if cancel.is_set() {
            stop(&mut child, Duration::from_secs(10));
            return Err(Error::Cancelled);
        }
        if let Some(s) = child.try_wait().map_err(|e| Error::Msg(e.to_string()))? {
            break s;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if let Some(e) = failed {
        return Err(e);
    }
    if !status.success() {
        let why = if tail.is_empty() {
            String::new()
        } else {
            format!(": {}", tail.join(" / "))
        };
        return Err(Error::Msg(format!("{name} failed ({status}){why}")));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn lines_and_exit() {
        let mut c = command("sh");
        c.args(["-c", "echo one; echo two >&2; printf 'a\\rb\\n'"]);
        let mut seen = Vec::new();
        run(c, &Cancel::new(), &mut |s, l| {
            seen.push((s, l.to_string()));
            Ok(())
        })
        .unwrap();
        seen.sort_by_key(|(_, l)| l.clone());
        assert_eq!(
            seen,
            [
                (Stream::Out, "a".to_string()),
                (Stream::Out, "b".to_string()),
                (Stream::Out, "one".to_string()),
                (Stream::Err, "two".to_string())
            ]
        );
        let mut c = command("sh");
        c.args(["-c", "echo bad >&2; exit 3"]);
        let e = run(c, &Cancel::new(), &mut |_, _| Ok(())).unwrap_err();
        assert!(e.to_string().contains("bad"), "{e}");
    }

    #[test]
    fn cancel_stops_it() {
        let mut c = command("sh");
        c.args(["-c", "echo go; sleep 30"]);
        let cancel = Cancel::new();
        let k = cancel.clone();
        let t = Instant::now();
        let e = run(c, &cancel, &mut |_, _| {
            k.cancel();
            Ok(())
        })
        .unwrap_err();
        assert_eq!(e, Error::Cancelled);
        assert!(t.elapsed() < Duration::from_secs(10));
    }
}
