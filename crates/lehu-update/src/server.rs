//! The page on the access point: a small HTTP/1.1 server, one connection
//! at a time, each answered and closed.
//!
//! - `GET /` the page, `GET /info` what the page needs to pick the update
//!   (the commit running, the manifest's URL, how much of an upload is
//!   here), `GET /state` what the head unit is doing with it.
//! - `POST /upload?file=&size=&sha256=&offset=[&notes=]` streams the body
//!   to `<cache>/<file>.part`, appended at `offset`, which must be the
//!   part's length (else 409 with it): a cut transfer resumes. When `size`
//!   bytes are there and their SHA-256 is the manifest's, the part becomes
//!   `<file>`, everything else in the folder goes, and `check` runs.
//!
//! The body is never held in memory: 16 KB at a time, straight to the card.
//! A connection that sends nothing for `read_timeout` is dropped, and one
//! with no request within `HEAD_TIMEOUT`: browsers open sockets ahead of
//! their requests, and one at a time, an idle one would keep the next
//! waiting. A head over 8 KB, a bad request line or bad parameters get a
//! 4xx.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::store::{self, MAX_SIZE, SPARE};
use crate::{Inner, Shared, Stage};

/// The page the phone opens.
const PAGE: &str = include_str!("page.html");

/// The page, as served.
pub fn page() -> &'static str {
    PAGE
}

/// A request's head may be this long.
const HEAD_MAX: usize = 8 * 1024;
/// A request's head arrives within this.
const HEAD_TIMEOUT: Duration = Duration::from_secs(3);
/// The body moves in pieces this big.
const CHUNK: usize = 16 * 1024;
/// The manifest's line about the update, as kept.
const NOTES_MAX: usize = 300;
/// After an early answer, this much of a body still coming is read and
/// dropped, so the phone gets the answer rather than a reset.
const LINGER_MAX: u64 = 4 << 20;

/// Bind on the access point's address and serve on a thread; the page's
/// URL.
pub(crate) fn start(shared: Arc<Shared>, ip: Ipv4Addr) -> io::Result<String> {
    let listener = TcpListener::bind((ip, shared.cfg.port))?;
    let addr = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    shared.lock().server_stop = Some(Arc::clone(&stop));
    let worker = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("lehu-update-http".into())
        .stack_size(128 * 1024)
        .spawn(move || serve(&worker, &listener, &stop));
    if let Err(e) = spawned {
        shared.lock().server_stop = None;
        return Err(e);
    }
    Ok(format!("http://{addr}/"))
}

/// Stop the server: within a second, or at once if a connection is cut.
pub(crate) fn stop(inner: &mut Inner) {
    if let Some(stop) = inner.server_stop.take() {
        stop.store(true, Ordering::Release);
        log::info!("update: the page stops");
    }
    if let Some(conn) = inner.conn.take() {
        let _ = conn.shutdown(Shutdown::Both);
    }
    inner.view.serving = false;
}

fn serve(shared: &Shared, listener: &TcpListener, stop: &Arc<AtomicBool>) {
    let mut chunk = vec![0u8; CHUNK];
    while !stop.load(Ordering::Acquire) {
        match readable(listener.as_raw_fd(), 1000) {
            Ok(true) => {}
            Ok(false) => {
                if idle(shared) {
                    log::info!("update: the page stops, unused");
                    break;
                }
                continue;
            }
            Err(e) => {
                log::warn!("update: the page's socket: {e}");
                break;
            }
        }
        let stream = match listener.accept() {
            Ok((s, peer)) => {
                log::debug!("update: a connection from {peer}");
                s
            }
            Err(e) => {
                log::debug!("update: accept: {e}");
                continue;
            }
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        shared.lock().conn = stream.try_clone().ok();
        if let Err(e) = handle(shared, stream, &mut chunk, stop) {
            log::debug!("update: a connection ended: {e}");
        }
        shared.lock().conn = None;
    }
    // Gone by itself (unused, or the socket failed): the view says so,
    // unless a newer server took over.
    shared.change(|inner| {
        if inner
            .server_stop
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, stop))
        {
            inner.server_stop = None;
            inner.view.serving = false;
        }
    });
}

/// The Update pages were left long enough ago, and nothing is being
/// checked.
fn idle(shared: &Shared) -> bool {
    let inner = shared.lock();
    inner
        .left_at
        .is_some_and(|t| t.elapsed() >= shared.cfg.idle_stop)
        && inner.view.stage != Stage::Checking
}

/// `fd` has something to accept within `ms`.
fn readable(fd: i32, ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, alive across the call.
    let n = unsafe { libc::poll(&mut p, 1, ms) };
    match n {
        n if n > 0 => Ok(true),
        0 => Ok(false),
        _ => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// A request's head, as far as this server reads it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub content_length: Option<u64>,
    pub chunked: bool,
    pub expect_continue: bool,
}

impl Request {
    /// The head without its blank line; `None` when it is not HTTP/1.x.
    pub(crate) fn parse(head: &[u8]) -> Option<Self> {
        let head = std::str::from_utf8(head).ok()?;
        let mut lines = head.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let (method, target, version) = (first.next()?, first.next()?, first.next()?);
        if first.next().is_some()
            || !version.starts_with("HTTP/1.")
            || method.is_empty()
            || !target.starts_with('/')
        {
            return None;
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut r = Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            ..Self::default()
        };
        for line in lines.filter(|l| !l.is_empty()) {
            let (name, value) = line.split_once(':')?;
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                let n: u64 = value.parse().ok()?;
                // Two lengths that differ: which body is it?
                if r.content_length.is_some_and(|m| m != n) {
                    return None;
                }
                r.content_length = Some(n);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                r.chunked = true;
            } else if name.eq_ignore_ascii_case("expect") {
                r.expect_continue = value.eq_ignore_ascii_case("100-continue");
            }
        }
        Some(r)
    }
}

enum Head {
    /// The head ends at `.0`; `.1` bytes of the buffer are filled.
    Done(usize, usize),
    /// The peer closed before a whole head.
    Closed,
    TooLarge,
}

fn read_head(stream: &mut impl Read, buf: &mut [u8]) -> io::Result<Head> {
    let limit = HEAD_MAX.min(buf.len());
    let mut filled = 0;
    loop {
        if filled == limit {
            return Ok(Head::TooLarge);
        }
        let n = match stream.read(&mut buf[filled..limit]) {
            Ok(0) => return Ok(Head::Closed),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let from = filled.saturating_sub(3);
        filled += n;
        if let Some(at) = buf[from..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(Head::Done(from + at + 4, filled));
        }
    }
}

fn handle(
    shared: &Shared,
    mut stream: TcpStream,
    buf: &mut [u8],
    stop: &AtomicBool,
) -> io::Result<()> {
    let timeout = Some(shared.cfg.read_timeout);
    stream.set_read_timeout(Some(HEAD_TIMEOUT.min(shared.cfg.read_timeout)))?;
    stream.set_write_timeout(timeout)?;
    let _ = stream.set_nodelay(true);
    let head = read_head(&mut stream, buf)?;
    stream.set_read_timeout(timeout)?;
    let (end, filled) = match head {
        Head::Done(end, filled) => (end, filled),
        Head::Closed => return Ok(()),
        Head::TooLarge => {
            return respond(
                &mut stream,
                431,
                &error_json("head", "the request is too long"),
            );
        }
    };
    let Some(req) = Request::parse(&buf[..end - 4]) else {
        return respond(&mut stream, 400, &error_json("request", "not a request"));
    };
    log::debug!("update: {} {}", req.method, req.path);
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => respond_as(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        ("GET", "/info") => respond(&mut stream, 200, &info(shared)),
        ("GET", "/state") => respond(&mut stream, 200, &state(shared)),
        ("POST", "/upload") => {
            let prefix = buf[end..filled].to_vec();
            upload(shared, &mut stream, &req, &prefix, buf, stop)
        }
        (_, "/" | "/info" | "/state" | "/upload") => {
            respond(&mut stream, 405, &error_json("method", "not allowed"))
        }
        _ => respond_as(&mut stream, 404, "text/plain", b"not found\n"),
    }
}

fn status_line(code: u16) -> &'static str {
    match code {
        200 => "200 OK",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        409 => "409 Conflict",
        411 => "411 Length Required",
        422 => "422 Unprocessable Content",
        431 => "431 Request Header Fields Too Large",
        500 => "500 Internal Server Error",
        501 => "501 Not Implemented",
        507 => "507 Insufficient Storage",
        _ => "500 Internal Server Error",
    }
}

fn respond(stream: &mut TcpStream, code: u16, json: &str) -> io::Result<()> {
    respond_as(stream, code, "application/json", json.as_bytes())
}

fn respond_as(stream: &mut TcpStream, code: u16, kind: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        status_line(code),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Answered before the body was read: the rest of it is read and dropped
/// for a moment, so the phone sees the answer.
fn linger(stream: &mut TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = io::copy(&mut (&mut *stream).take(LINGER_MAX), &mut io::sink());
}

/// JSON, built by hand: the few fields the page reads.
#[derive(Default)]
pub(crate) struct Json(String);

impl Json {
    fn key(&mut self, key: &str) {
        self.0.push(if self.0.is_empty() { '{' } else { ',' });
        self.0.push('"');
        self.0.push_str(key);
        self.0.push_str("\":");
    }

    pub(crate) fn str(mut self, key: &str, value: &str) -> Self {
        self.key(key);
        self.0.push('"');
        for c in value.chars() {
            match c {
                '"' => self.0.push_str("\\\""),
                '\\' => self.0.push_str("\\\\"),
                // `<` too, so the text is safe wherever the page puts it.
                c if c < ' ' || c == '<' || c == '\u{2028}' || c == '\u{2029}' => {
                    self.0.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => self.0.push(c),
            }
        }
        self.0.push('"');
        self
    }

    pub(crate) fn num(mut self, key: &str, value: u64) -> Self {
        self.key(key);
        self.0.push_str(&value.to_string());
        self
    }

    pub(crate) fn bool(mut self, key: &str, value: bool) -> Self {
        self.key(key);
        self.0.push_str(if value { "true" } else { "false" });
        self
    }

    pub(crate) fn done(mut self) -> String {
        if self.0.is_empty() {
            self.0.push('{');
        }
        self.0.push('}');
        self.0
    }
}

fn error_json(error: &str, message: &str) -> String {
    Json::default()
        .str("error", error)
        .str("message", message)
        .done()
}

/// What the page asks first: the version running, where the updates are
/// listed, and what of an upload is here already.
pub(crate) fn info(shared: &Shared) -> String {
    let inner = shared.lock();
    let v = &inner.view;
    let (file, have, complete) = match v.stage {
        Stage::Checking | Stage::Ready | Stage::Installing | Stage::Restarting => {
            (v.file.clone(), v.size, true)
        }
        _ => store::current_part(&shared.cfg.cache)
            .map_or((String::new(), 0, false), |(f, n)| (f, n, false)),
    };
    Json::default()
        .str("booted", &v.booted)
        .str("booted_version", &v.booted_version)
        .str("manifest_url", &shared.cfg.manifest_url)
        .str("stage", v.stage.word())
        .str("file", &file)
        .num("have", have)
        .bool("complete", complete)
        .num("cancels", u64::from(v.cancels))
        .done()
}

/// What the head unit is doing with the update.
pub(crate) fn state(shared: &Shared) -> String {
    let inner = shared.lock();
    let v = &inner.view;
    Json::default()
        .str("stage", v.stage.word())
        .str("file", &v.file)
        .num("received", v.received)
        .num("size", v.size)
        .str("version", &v.version)
        .str("mode", v.mode.word())
        .num("percent", u64::from(v.percent))
        .str("message", &v.message)
        .done()
}

/// An upload's parameters, checked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Upload {
    pub file: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub offset: u64,
    pub notes: String,
}

impl Upload {
    pub(crate) fn parse(query: &str) -> Result<Self, &'static str> {
        let (mut file, mut size, mut sha256, mut offset, mut notes) = (None, None, None, 0, None);
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = decode(value).ok_or("a parameter is not encoded right")?;
            match name {
                "file" => file = Some(value),
                "size" => size = Some(value.parse::<u64>().map_err(|_| "size is not a number")?),
                "sha256" => {
                    sha256 = Some(store::parse_sha256(&value).ok_or("sha256 is not 64 hex digits")?)
                }
                "offset" => offset = value.parse().map_err(|_| "offset is not a number")?,
                "notes" => notes = Some(value),
                _ => {}
            }
        }
        let file = file.ok_or("no file")?;
        if !store::valid_name(&file) {
            return Err("not an update's file name");
        }
        let size = size.ok_or("no size")?;
        if size == 0 || size > MAX_SIZE {
            return Err("the size is out of range");
        }
        if offset > size {
            return Err("the offset is past the end");
        }
        let mut notes: String = notes
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if notes.len() > NOTES_MAX {
            let mut cut = NOTES_MAX;
            while !notes.is_char_boundary(cut) {
                cut -= 1;
            }
            notes.truncate(cut);
        }
        Ok(Self {
            file,
            size,
            sha256: sha256.ok_or("no sha256")?,
            offset,
            notes,
        })
    }
}

/// `%XX` decoded; `+` stays itself (the page encodes with
/// encodeURIComponent).
fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Why an upload is refused before its body is read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Checking, installing: not now.
    Busy(Stage),
    /// The offset is not the part's length: the part's length.
    Offset(u64),
    /// The body runs past `size`.
    TooLong,
    NoSpace,
    NoFolder,
}

/// Whether to take an upload of `len` bytes; if so, the view says it is
/// being received and other updates in the folder are gone.
pub(crate) fn begin(shared: &Shared, up: &Upload, len: u64) -> Result<(), Refused> {
    let dir = &shared.cfg.cache;
    shared.change(|inner| {
        let stage = inner.view.stage;
        if matches!(
            stage,
            Stage::Checking | Stage::Installing | Stage::Restarting
        ) {
            return Err(Refused::Busy(stage));
        }
        if !dir.is_dir() {
            return Err(Refused::NoFolder);
        }
        let have = store::part_len(dir, &up.file);
        if have > up.size {
            // Not this update's part after all.
            let _ = fs::remove_file(store::part_path(dir, &up.file));
            return Err(Refused::Offset(0));
        }
        if up.offset != have {
            return Err(Refused::Offset(have));
        }
        if len > up.size - up.offset {
            return Err(Refused::TooLong);
        }
        let free = store::free_bytes(dir).unwrap_or(0);
        if free < (up.size - up.offset).saturating_add(SPARE) {
            return Err(Refused::NoSpace);
        }
        if up.offset == 0 {
            store::remove_others(dir, &[&format!("{}.part", up.file)]);
        }
        let v = &mut inner.view;
        v.stage = Stage::Receiving;
        v.file = up.file.clone();
        v.size = up.size;
        v.received = up.offset;
        v.notes = up.notes.clone();
        v.version.clear();
        v.message.clear();
        Ok(())
    })
}

/// How an upload's body went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Received {
    /// All of it is here, intact: checked next.
    Complete,
    /// The body ended as sent, short of `size`: so much is here.
    Partial(u64),
    /// The connection ended before the body did: so much is here.
    Cut(u64),
    /// The whole file is not what the manifest said: removed.
    Damaged,
    /// It could not be written.
    Failed(String),
}

/// The body (`len` bytes from `body`) appended to the part, hashed on the
/// way when it starts at 0, and the file checked when whole.
pub(crate) fn receive(
    shared: &Shared,
    up: &Upload,
    len: u64,
    body: &mut impl Read,
    chunk: &mut [u8],
    stop: &AtomicBool,
) -> Received {
    let dir = &shared.cfg.cache;
    let part = store::part_path(dir, &up.file);
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(&part)?;
        let mut hasher = (up.offset == 0).then(openssl::sha::Sha256::new);
        let mut got = 0u64;
        while got < len {
            if stop.load(Ordering::Acquire) {
                return Ok(Received::Cut(up.offset + got));
            }
            let want = chunk.len().min((len - got) as usize);
            let n = match body.read(&mut chunk[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::info!("update: the transfer stopped: {e}");
                    break;
                }
            };
            f.write_all(&chunk[..n])?;
            if let Some(h) = hasher.as_mut() {
                h.update(&chunk[..n]);
            }
            got += n as u64;
            shared.progress(|v| v.received = up.offset + got);
        }
        let total = up.offset + got;
        if got < len {
            f.sync_data()?;
            return Ok(Received::Cut(total));
        }
        if total < up.size {
            f.sync_data()?;
            return Ok(Received::Partial(total));
        }
        f.sync_all()?;
        drop(f);
        let digest = match hasher {
            Some(h) => h.finish(),
            // Resumed: the parts sent before are hashed from the card.
            None => store::sha256_file(&part)?,
        };
        if digest != up.sha256 {
            log::warn!("update: {} arrived damaged", up.file);
            fs::remove_file(&part)?;
            return Ok(Received::Damaged);
        }
        fs::rename(&part, dir.join(&up.file))?;
        store::remove_others(dir, &[&up.file]);
        log::info!("update: {} received, {} bytes", up.file, up.size);
        Ok(Received::Complete)
    })();
    let result = result.unwrap_or_else(|e: io::Error| Received::Failed(e.to_string()));
    shared.change(|inner| {
        // Cancelled on the head unit meanwhile: however it ended, it goes.
        if std::mem::take(&mut inner.cancel_upload) {
            let whole = result == Received::Complete;
            shared.drop_update(inner, whole);
            return;
        }
        match &result {
            Received::Complete => shared.received(inner, &up.file),
            Received::Partial(n) | Received::Cut(n) => {
                inner.view.received = *n;
                if inner.view.stage == Stage::Receiving {
                    inner.view.stage = Stage::Idle;
                }
            }
            Received::Damaged => {
                inner.view.stage = Stage::Failed;
                inner.view.file.clear();
                inner.view.received = 0;
                inner.view.message = "The update arrived damaged. Send it again".into();
            }
            Received::Failed(why) => {
                log::warn!("update: cannot save {}: {why}", up.file);
                inner.view.stage = Stage::Failed;
                inner.view.message = "The update cannot be saved".into();
            }
        }
    });
    result
}

fn upload(
    shared: &Shared,
    stream: &mut TcpStream,
    req: &Request,
    prefix: &[u8],
    chunk: &mut [u8],
    stop: &AtomicBool,
) -> io::Result<()> {
    let up = match Upload::parse(&req.query) {
        Ok(up) => up,
        Err(why) => {
            respond(stream, 400, &error_json("parameters", why))?;
            linger(stream);
            return Ok(());
        }
    };
    if req.chunked {
        respond(stream, 501, &error_json("chunked", "send a length"))?;
        linger(stream);
        return Ok(());
    }
    let Some(len) = req.content_length else {
        respond(stream, 411, &error_json("length", "send a length"))?;
        linger(stream);
        return Ok(());
    };
    if let Err(refused) = begin(shared, &up, len) {
        let (code, json) = match refused {
            Refused::Busy(stage) => (
                409,
                Json::default()
                    .str("error", "busy")
                    .str("stage", stage.word())
                    .done(),
            ),
            Refused::Offset(have) => (
                409,
                Json::default()
                    .str("error", "offset")
                    .num("have", have)
                    .done(),
            ),
            Refused::TooLong => (400, error_json("length", "longer than the file")),
            Refused::NoSpace => (507, error_json("space", "no room on the head unit")),
            Refused::NoFolder => (500, error_json("folder", "no folder for updates")),
        };
        log::info!("update: upload of {} refused: {refused:?}", up.file);
        respond(stream, code, &json)?;
        linger(stream);
        return Ok(());
    }
    if req.expect_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let ahead = prefix.len().min(usize::try_from(len).unwrap_or(usize::MAX));
    let mut body = (&prefix[..ahead]).chain((&mut *stream).take(len - ahead as u64));
    let (code, json) = match receive(shared, &up, len, &mut body, chunk, stop) {
        Received::Complete => (
            200,
            Json::default()
                .bool("complete", true)
                .num("have", up.size)
                .done(),
        ),
        Received::Partial(n) => (
            200,
            Json::default()
                .bool("complete", false)
                .num("have", n)
                .done(),
        ),
        Received::Cut(n) => (
            400,
            Json::default().str("error", "cut").num("have", n).done(),
        ),
        Received::Damaged => (422, error_json("sha256", "the file arrived damaged")),
        Received::Failed(_) => (500, error_json("save", "the file cannot be saved")),
    };
    respond(stream, code, &json)
}
