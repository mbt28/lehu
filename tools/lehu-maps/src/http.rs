// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Downloads: small texts, and large files to the cache with resume (a
//! `.part` file continued with a Range request) and an md5 check.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use md5::{Digest, Md5};
use ureq::ResponseExt;

use crate::util::{Cancel, Error, Result, hex, io_err};

/// The User-Agent: the program and its version, nothing about the user.
pub const USER_AGENT: &str = concat!("lehu-maps/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

/// What a HEAD request found.
pub struct Head {
    /// Where the redirects ended.
    pub url: String,
    pub size: Option<u64>,
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(None)
            .timeout_per_read(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }

    /// A small text (an index, an md5 file).
    pub fn text(&self, url: &str) -> Result<String> {
        let mut res = self
            .agent
            .get(url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("{url}: HTTP {}", res.status()).into());
        }
        res.body_mut()
            .with_config()
            .limit(32 << 20)
            .read_to_string()
            .map_err(|e| format!("{url}: {e}").into())
    }

    /// Where `url` leads, and how large it is.
    pub fn head(&self, url: &str) -> Result<Head> {
        let res = self
            .agent
            .head(url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("{url}: HTTP {}", res.status()).into());
        }
        let size = res
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        Ok(Head {
            url: res.get_uri().to_string(),
            size,
        })
    }

    /// `url` into `dest`, through `dest.part`: a part there from before is
    /// continued (a Range request), unless the server sends the whole file
    /// again. With `md5`, the file is checked against it; a file that does
    /// not match is removed, and an error returned. `progress(done, total)`
    /// as it comes. Cancelled, the part stays for the next time.
    pub fn download(
        &self,
        url: &str,
        dest: &Path,
        md5: Option<&str>,
        cancel: &Cancel,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<()> {
        let part = part_path(dest);
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).map_err(|e| io_err(dir.display(), e))?;
        }
        let have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let mut req = self.agent.get(url);
        if have > 0 {
            req = req.header("Range", &format!("bytes={have}-"));
        }
        let res = req.call().map_err(|e| format!("{url}: {e}"))?;
        let status = res.status().as_u16();
        let length: Option<u64> = res
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let (mut file, mut done, total) = match status {
            206 => {
                let start = res
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(range_start);
                if start != Some(have) {
                    return Err(
                        format!("{url}: the server resumed at {start:?}, not {have}").into(),
                    );
                }
                let f = OpenOptions::new()
                    .append(true)
                    .open(&part)
                    .map_err(|e| io_err(part.display(), e))?;
                (f, have, length.map(|l| l + have))
            }
            200 => {
                let f = File::create(&part).map_err(|e| io_err(part.display(), e))?;
                (f, 0, length)
            }
            // The part is already whole: nothing after it.
            416 if have > 0 => {
                drop(res);
                return finish(&part, dest, md5);
            }
            s => return Err(format!("{url}: HTTP {s}").into()),
        };
        let mut body = res.into_body().into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        progress(done, total);
        loop {
            cancel.check()?;
            let n = body.read(&mut buf).map_err(|e| {
                Error::Msg(format!(
                    "{url}: {e} (the download goes on from here next time)"
                ))
            })?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| io_err(part.display(), e))?;
            done += n as u64;
            progress(done, total);
        }
        file.sync_all().map_err(|e| io_err(part.display(), e))?;
        drop(file);
        if let Some(t) = total
            && done != t
        {
            return Err(format!("{url}: {done} of {t} bytes came; try again").into());
        }
        finish(&part, dest, md5)
    }
}

/// The part checked and renamed into place.
fn finish(part: &Path, dest: &Path, md5: Option<&str>) -> Result<()> {
    if let Some(want) = md5 {
        let got = md5_file(part)?;
        if !got.eq_ignore_ascii_case(want.trim()) {
            let _ = fs::remove_file(part);
            return Err(format!(
                "{} did not download whole (md5 {got}, not {want}); it is removed, try again",
                dest.file_name().unwrap_or_default().to_string_lossy()
            )
            .into());
        }
    }
    fs::rename(part, dest).map_err(|e| io_err(dest.display(), e))
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// `bytes 100-199/200` → 100.
fn range_start(v: &str) -> Option<u64> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

/// The md5 of a file, hex.
pub fn md5_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| io_err(path.display(), e))?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| io_err(path.display(), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// The first word of an `.md5` file (`<hex>  <name>`), when it is an md5.
pub fn parse_md5(text: &str) -> Option<String> {
    let w = text.split_whitespace().next()?;
    (w.len() == 32 && w.bytes().all(|b| b.is_ascii_hexdigit())).then(|| w.to_ascii_lowercase())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A tiny HTTP server on 127.0.0.1 serving `body` at every path, with
    /// Range requests answered when `ranges`, else the whole body. Its base
    /// URL, and how many requests it had.
    pub fn serve(body: Vec<u8>, ranges: bool) -> (String, Arc<AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                c.fetch_add(1, Ordering::SeqCst);
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut range = None;
                let mut head = false;
                let mut line = String::new();
                while r.read_line(&mut line).unwrap_or(0) > 0 {
                    let l = line.trim_end().to_string();
                    line.clear();
                    if l.starts_with("HEAD ") {
                        head = true;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = v.trim_end_matches('-').parse::<usize>().ok();
                    }
                    if l.is_empty() {
                        break;
                    }
                }
                let (status, part, extra) = match range {
                    Some(start) if ranges && start >= body.len() => {
                        ("416 Range Not Satisfiable", &body[0..0], String::new())
                    }
                    Some(start) if ranges => (
                        "206 Partial Content",
                        &body[start..],
                        format!(
                            "Content-Range: bytes {start}-{}/{}\r\n",
                            body.len() - 1,
                            body.len()
                        ),
                    ),
                    _ => ("200 OK", &body[..], String::new()),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    part.len()
                );
                if !head {
                    let _ = s.write_all(part);
                }
            }
        });
        (url, count)
    }

    fn body() -> Vec<u8> {
        (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn md5_of(b: &[u8]) -> String {
        hex(&Md5::digest(b))
    }

    #[test]
    fn resumes_a_part() {
        let data = body();
        let (url, _) = serve(data.clone(), true);
        let dir = crate::testutil::TempDir::new("resume");
        let dest = dir.path().join("x.osm.pbf");
        fs::write(part_path(&dest), &data[..123_456]).unwrap();
        let mut first = None;
        Http::new()
            .download(
                &format!("{url}/x"),
                &dest,
                Some(&md5_of(&data)),
                &Cancel::new(),
                &mut |d, t| {
                    first.get_or_insert((d, t));
                },
            )
            .unwrap();
        assert_eq!(first, Some((123_456, Some(data.len() as u64))));
        assert_eq!(fs::read(&dest).unwrap(), data);
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn starts_again_without_ranges() {
        let data = body();
        let (url, _) = serve(data.clone(), false);
        let dir = crate::testutil::TempDir::new("noranges");
        let dest = dir.path().join("x.osm.pbf");
        fs::write(part_path(&dest), b"garbage that is not the start").unwrap();
        Http::new()
            .download(
                &format!("{url}/x"),
                &dest,
                Some(&md5_of(&data)),
                &Cancel::new(),
                &mut |_, _| {},
            )
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn whole_part_and_bad_md5() {
        let data = body();
        let (url, _) = serve(data.clone(), true);
        let dir = crate::testutil::TempDir::new("whole");
        let dest = dir.path().join("x.osm.pbf");
        // The part is complete: the server says 416, the md5 decides.
        fs::write(part_path(&dest), &data).unwrap();
        Http::new()
            .download(
                &format!("{url}/x"),
                &dest,
                Some(&md5_of(&data)),
                &Cancel::new(),
                &mut |_, _| {},
            )
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
        // A wrong md5: the file is removed and it says so.
        let dest2 = dir.path().join("y.osm.pbf");
        let e = Http::new()
            .download(
                &format!("{url}/y"),
                &dest2,
                Some(&"0".repeat(32)),
                &Cancel::new(),
                &mut |_, _| {},
            )
            .unwrap_err();
        assert!(e.to_string().contains("did not download whole"), "{e}");
        assert!(!dest2.exists() && !part_path(&dest2).exists());
    }

    #[test]
    fn cancel_keeps_the_part() {
        let data = body();
        let (url, _) = serve(data, true);
        let dir = crate::testutil::TempDir::new("cancel");
        let dest = dir.path().join("x.osm.pbf");
        let cancel = Cancel::new();
        let c = cancel.clone();
        let e = Http::new()
            .download(&format!("{url}/x"), &dest, None, &cancel, &mut |d, _| {
                if d > 0 {
                    c.cancel();
                }
            })
            .unwrap_err();
        assert_eq!(e, Error::Cancelled);
        assert!(part_path(&dest).exists() && !dest.exists());
    }

    #[test]
    fn head_and_text() {
        let md5 = b"0793fa28e938308574f9fc0dcc40f0ba  luxembourg-261110.osm.pbf\n";
        let (url, _) = serve(md5.to_vec(), false);
        let h = Http::new().head(&format!("{url}/a.md5")).unwrap();
        assert_eq!(h.size, Some(md5.len() as u64));
        let t = Http::new().text(&format!("{url}/a.md5")).unwrap();
        assert_eq!(
            parse_md5(&t).as_deref(),
            Some("0793fa28e938308574f9fc0dcc40f0ba")
        );
        assert_eq!(parse_md5("<html>"), None);
    }

    #[test]
    fn content_range() {
        assert_eq!(range_start("bytes 100-199/200"), Some(100));
        assert_eq!(range_start("bytes */200"), None);
    }
}
