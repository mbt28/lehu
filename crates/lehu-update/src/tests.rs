// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The update against a stand-in installer (a shell script) and a folder of
//! its own, the page's server over loopback.

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use super::server::{Received, Refused, Request, Upload, begin, receive};
use super::*;

fn no_unit() -> bool {
    false
}

fn a_unit() -> bool {
    true
}

/// A folder with a cache, and an installer that answers from files there:
/// `status` prints `status`, `check` prints `check` (exit 1 when
/// `check-fails` is there), `apply` writes the progress and prints
/// `apply`.
fn setup(name: &str) -> (PathBuf, Config) {
    let root = store::tests::dir(name);
    let cache = root.join("cache");
    fs::create_dir(&cache).unwrap();
    let script = root.join("lehu-os-update");
    let r = root.display();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             case $1 in\n\
             status) cat {r}/status 2>/dev/null; echo booted=c1; echo booted_version=1.0 ;;\n\
             check) echo \"$2\" >> {r}/checked; [ -f {r}/check-fails ] && \
             {{ echo 'message=the update is not signed with LEHU OS'\"'\"'s key'; exit 1; }}; \
             echo version=2.0; echo commit=c2; echo booted=c1; echo size_kb=40; echo mode=inplace ;;\n\
             apply) printf 'percent=60\\nphase=installing\\nmessage=Installing the update\\n' > {r}/progress; \
             sleep 1; [ -f {r}/apply-fails ] && {{ echo 'message=the pull failed'; exit 1; }}; \
             echo message=version 2.0 is installed ;;\n\
             esac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = Config {
        installer: script,
        progress: root.join("progress"),
        cache,
        shown: root.join("shown"),
        sudo: false,
        manifest_url: "https://example.org/os/manifest.json".into(),
        port: 0,
        unit_running: no_unit,
        idle_stop: Duration::from_secs(600),
        restart_wait: Duration::from_secs(600),
        read_timeout: Duration::from_secs(2),
    };
    (root, cfg)
}

fn wait_for(updater: &Updater, what: &str, ok: impl Fn(&View) -> bool) -> View {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let v = updater.view();
        if ok(&v) {
            return v;
        }
        assert!(Instant::now() < until, "waited for {what}: {v:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sha_hex(data: &[u8]) -> String {
    openssl::sha::sha256(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn upload(file: &str, data: &[u8], offset: u64) -> Upload {
    Upload::parse(&format!(
        "file={file}&size={}&sha256={}&offset={offset}&notes=Faster%20boot",
        data.len(),
        sha_hex(data)
    ))
    .unwrap()
}

fn shared(cfg: Config) -> Arc<Shared> {
    Shared::new(cfg, Box::new(|| {}))
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

// ── the upload's parameters ──────────────────────────────────────────────

#[test]
fn upload_parameters_are_checked() {
    let sha = "a".repeat(64);
    let ok = Upload::parse(&format!(
        "file=lehu-os-2-from-1.tar&size=100&sha256={sha}&offset=40&notes=Back%20to%20%2Bnormal&x=1"
    ))
    .unwrap();
    assert_eq!(ok.file, "lehu-os-2-from-1.tar");
    assert_eq!((ok.size, ok.offset), (100, 40));
    assert_eq!(ok.notes, "Back to +normal");
    // The offset defaults to the start; notes are optional and kept short.
    let long = Upload::parse(&format!(
        "file=a.tar&size=1&sha256={sha}&notes={}",
        "%C3%A9".repeat(400)
    ))
    .unwrap();
    assert_eq!(long.offset, 0);
    assert!(long.notes.len() <= 300 && long.notes.starts_with('é'));
    for bad in [
        format!("size=1&sha256={sha}"),
        format!("file=..%2Fetc%2Fx.tar&size=1&sha256={sha}"),
        format!("file=a.tgz&size=1&sha256={sha}"),
        format!("file=a.tar&sha256={sha}"),
        format!("file=a.tar&size=0&sha256={sha}"),
        format!("file=a.tar&size={}&sha256={sha}", MAX_SIZE + 1),
        format!("file=a.tar&size=x&sha256={sha}"),
        "file=a.tar&size=1&sha256=abc".to_string(),
        "file=a.tar&size=1".to_string(),
        format!("file=a.tar&size=5&offset=6&sha256={sha}"),
        format!("file=a%2.tar&size=1&sha256={sha}"),
        format!("file=a.tar&size=1&sha256={sha}&notes=%FF"),
    ] {
        assert!(Upload::parse(&bad).is_err(), "{bad}");
    }
}

#[test]
fn request_heads() {
    let r = Request::parse(
        b"POST /upload?file=a.tar HTTP/1.1\r\nHost: x\r\ncontent-length: 12\r\nExpect: 100-continue",
    )
    .unwrap();
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/upload");
    assert_eq!(r.query, "file=a.tar");
    assert_eq!(r.content_length, Some(12));
    assert!(r.expect_continue && !r.chunked);
    assert!(
        Request::parse(b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked")
            .unwrap()
            .chunked
    );
    for bad in [
        &b"GET /"[..],
        b"GET / SPDY/3",
        b"GET x HTTP/1.1",
        b"GET / HTTP/1.1 extra",
        b"GET / HTTP/1.1\r\nno colon here",
        b"POST / HTTP/1.1\r\nContent-Length: -1",
        b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2",
        b"\xff\xfe / HTTP/1.1",
    ] {
        assert_eq!(
            Request::parse(bad),
            None,
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn json_is_escaped() {
    let j = server::Json::default()
        .str("a", "say \"hi\"\\\n<b>")
        .num("n", 7)
        .bool("t", true)
        .done();
    assert_eq!(j, r#"{"a":"say \"hi\"\\\u000a\u003cb>","n":7,"t":true}"#);
    assert_eq!(server::Json::default().done(), "{}");
}

// ── receiving, without a socket ──────────────────────────────────────────

#[test]
fn a_whole_upload_is_checked_renamed_and_the_rest_cleaned() {
    let (_root, cfg) = setup("whole");
    let dir = cfg.cache.clone();
    fs::write(dir.join("old.tar"), b"old").unwrap();
    fs::write(dir.join("other.tar.part"), b"part").unwrap();
    let sh = shared(cfg);
    let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7) as u8).collect();
    let up = upload("new.tar", &data, 0);
    begin(&sh, &up, data.len() as u64).unwrap();
    // A new upload clears out what came before.
    assert_eq!(names(&dir), Vec::<String>::new());
    assert_eq!(sh.lock().view.stage, Stage::Receiving);
    assert_eq!(sh.lock().view.notes, "Faster boot");
    let mut chunk = vec![0; 4096];
    let got = receive(
        &sh,
        &up,
        data.len() as u64,
        &mut &data[..],
        &mut chunk,
        &AtomicBool::new(false),
    );
    assert_eq!(got, Received::Complete);
    assert_eq!(names(&dir), ["new.tar"]);
    assert_eq!(fs::read(dir.join("new.tar")).unwrap(), data);
    let inner = sh.lock();
    assert_eq!(inner.view.stage, Stage::Checking);
    assert_eq!(inner.view.received, data.len() as u64);
    assert!(matches!(
        inner.jobs.front(),
        Some(Job::Check { quiet: false, .. })
    ));
}

#[test]
fn a_cut_upload_resumes_at_its_length() {
    let (_root, cfg) = setup("resume");
    let dir = cfg.cache.clone();
    let sh = shared(cfg);
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let stop = AtomicBool::new(false);
    let mut chunk = vec![0; 1000];
    let first = upload("u.tar", &data, 0);
    begin(&sh, &first, data.len() as u64).unwrap();
    // The phone went away after 12345 bytes of the body.
    let got = receive(
        &sh,
        &first,
        data.len() as u64,
        &mut &data[..12_345],
        &mut chunk,
        &stop,
    );
    assert_eq!(got, Received::Cut(12_345));
    assert_eq!(store::part_len(&dir, "u.tar"), 12_345);
    assert_eq!(sh.lock().view.stage, Stage::Idle);
    assert_eq!(sh.lock().view.received, 12_345);
    // From anywhere else: refused with the length there is.
    let wrong = upload("u.tar", &data, 0);
    assert_eq!(
        begin(&sh, &wrong, data.len() as u64),
        Err(Refused::Offset(12_345))
    );
    let rest = upload("u.tar", &data, 12_345);
    let len = (data.len() - 12_345) as u64;
    assert_eq!(begin(&sh, &rest, len + 1), Err(Refused::TooLong));
    begin(&sh, &rest, len).unwrap();
    // In two bodies: the first a part of the rest, sent as such.
    let half = upload("u.tar", &data, 12_345);
    let got = receive(
        &sh,
        &half,
        10_000,
        &mut &data[12_345..22_345],
        &mut chunk,
        &stop,
    );
    assert_eq!(got, Received::Partial(22_345));
    let last = upload("u.tar", &data, 22_345);
    begin(&sh, &last, (data.len() - 22_345) as u64).unwrap();
    let got = receive(
        &sh,
        &last,
        (data.len() - 22_345) as u64,
        &mut &data[22_345..],
        &mut chunk,
        &stop,
    );
    // Hashed again from the card: the parts before were not seen now.
    assert_eq!(got, Received::Complete);
    assert_eq!(fs::read(dir.join("u.tar")).unwrap(), data);
}

#[test]
fn a_damaged_upload_is_removed_and_said() {
    let (_root, cfg) = setup("damaged");
    let dir = cfg.cache.clone();
    let sh = shared(cfg);
    let data = vec![1u8; 5000];
    let mut up = upload("d.tar", &data, 0);
    up.sha256[0] ^= 1;
    begin(&sh, &up, 5000).unwrap();
    let got = receive(
        &sh,
        &up,
        5000,
        &mut &data[..],
        &mut [0; 512],
        &AtomicBool::new(false),
    );
    assert_eq!(got, Received::Damaged);
    assert_eq!(names(&dir), Vec::<String>::new());
    let v = sh.lock().view.clone();
    assert_eq!(v.stage, Stage::Failed);
    assert!(v.message.contains("damaged"));
    // Sent again from the start, it is taken.
    begin(&sh, &upload("d.tar", &data, 0), 5000).unwrap();
}

#[test]
fn uploads_wait_while_checking_and_installing() {
    let (_root, cfg) = setup("busy");
    let sh = shared(cfg);
    let data = [7u8; 10];
    for stage in [Stage::Checking, Stage::Installing, Stage::Restarting] {
        sh.lock().view.stage = stage;
        assert_eq!(
            begin(&sh, &upload("b.tar", &data, 0), 10),
            Err(Refused::Busy(stage))
        );
    }
    // A checked update is replaced by a new one.
    sh.lock().view.stage = Stage::Ready;
    begin(&sh, &upload("b.tar", &data, 0), 10).unwrap();
    // A part longer than the update is not its part.
    fs::write(store::part_path(&sh.cfg.cache, "c.tar"), [0u8; 20]).unwrap();
    sh.lock().view.stage = Stage::Idle;
    let long_part = Upload {
        offset: 10,
        ..upload("c.tar", &data, 0)
    };
    assert_eq!(begin(&sh, &long_part, 0), Err(Refused::Offset(0)));
    assert_eq!(store::part_len(&sh.cfg.cache, "c.tar"), 0);
}

#[test]
fn no_folder_no_upload() {
    let (_root, mut cfg) = setup("nofolder");
    cfg.cache = cfg.cache.join("missing");
    let sh = shared(cfg);
    assert_eq!(
        begin(&sh, &upload("n.tar", &[1], 0), 1),
        Err(Refused::NoFolder)
    );
}

// ── the outcome after a restart ──────────────────────────────────────────

fn status(state: &str, booted: &str) -> Status {
    Status {
        state: state.into(),
        version: "2.0".into(),
        commit: "c2".into(),
        message: "the pull failed".into(),
        time: "2026-11-08T10:14:40Z".into(),
        booted: booted.into(),
        booted_version: "x".into(),
    }
}

#[test]
fn the_last_outcome_is_news_once() {
    let none = (String::new(), String::new());
    assert_eq!(
        outcome_of(&status("installed", "c2"), &none, no_unit),
        Some(Outcome::Updated("2.0".into()))
    );
    assert!(matches!(
        outcome_of(&status("installed", "c1"), &none, no_unit),
        Some(Outcome::Failed(_))
    ));
    assert_eq!(
        outcome_of(&status("failed", "c1"), &none, no_unit),
        Some(Outcome::Failed("The pull failed".into()))
    );
    assert_eq!(
        outcome_of(&status("installing", "c1"), &none, no_unit),
        Some(Outcome::Interrupted)
    );
    // Still running (LEHU restarted while it works): nothing to say.
    assert_eq!(outcome_of(&status("installing", "c1"), &none, a_unit), None);
    assert_eq!(
        outcome_of(&status("update-boot", "c1"), &none, no_unit),
        None
    );
    assert_eq!(outcome_of(&status("", "c1"), &none, no_unit), None);
    // Shown before: not again.
    let shown = ("2026-11-08T10:14:40Z".to_string(), "c2".to_string());
    assert_eq!(
        outcome_of(&status("installed", "c2"), &shown, no_unit),
        None
    );
}

#[test]
fn an_update_shows_once_and_its_leftovers_go() {
    let (root, cfg) = setup("outcome");
    fs::write(
        root.join("status"),
        "state=installed\nversion=2.0\ncommit=c1\ntime=T1\n",
    )
    .unwrap();
    fs::write(cfg.cache.join("x.tar.part"), b"left").unwrap();
    let u = Updater::new(cfg.clone(), || {});
    let v = wait_for(&u, "the outcome", |v| v.outcome.is_some());
    assert_eq!(v.outcome, Some(Outcome::Updated("2.0".into())));
    assert_eq!(v.booted_version, "1.0");
    assert_eq!(names(&cfg.cache), Vec::<String>::new());
    u.outcome_seen();
    assert_eq!(u.view().outcome, None);
    assert_eq!(
        fs::read_to_string(&cfg.shown).unwrap(),
        "time=T1\ncommit=c1\n"
    );
    drop(u);
    // The next start says nothing.
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the status", |v| !v.booted.is_empty());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(u.view().outcome, None);
}

#[test]
fn an_update_received_before_is_checked_again_at_start() {
    let (root, cfg) = setup("leftover");
    // check unpacked it to a folder of the tar's name.
    fs::create_dir(cfg.cache.join("lehu-os-2.0-from-c1")).unwrap();
    let u = Updater::new(cfg.clone(), || {});
    let v = wait_for(&u, "the check", |v| v.stage == Stage::Ready);
    assert_eq!(v.file, "lehu-os-2.0-from-c1.tar");
    assert_eq!(v.version, "2.0");
    let checked = fs::read_to_string(root.join("checked")).unwrap();
    assert_eq!(
        checked.trim(),
        cfg.cache
            .join("lehu-os-2.0-from-c1.tar")
            .display()
            .to_string()
    );
    // One the installer refuses at start is not news.
    drop(u);
    fs::write(root.join("check-fails"), "").unwrap();
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the check", |v| {
        v.stage == Stage::Idle
            && fs::read_to_string(root.join("checked"))
                .unwrap()
                .lines()
                .count()
                == 2
    });
    assert_eq!(u.view().message, "");
}

// ── the whole way, over loopback ─────────────────────────────────────────

fn http(url: &str, request: &[u8], body: &[u8]) -> (u16, String) {
    let addr = url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(request).unwrap();
    // A peer that answered early may close before the body is all sent.
    let _ = s.write_all(body);
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let code = out
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (code, body)
}

fn post(url: &str, query: &str, body: &[u8]) -> (u16, String) {
    let head = format!(
        "POST /upload?{query} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    http(url, head.as_bytes(), body)
}

#[test]
fn the_page_takes_an_update_and_it_installs() {
    let (root, cfg) = setup("http");
    let cache = cfg.cache.clone();
    let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w = Arc::clone(&wakes);
    let u = Updater::new(cfg, move || {
        w.fetch_add(1, Ordering::Relaxed);
    });
    wait_for(&u, "the status", |v| v.booted == "c1");
    u.screens(true);
    u.serve(Some(Ipv4Addr::LOCALHOST));
    let v = u.view();
    assert!(v.serving, "{v:?}");
    let url = v.url.clone();
    assert!(url.starts_with("http://127.0.0.1:"));

    let (code, page) = http(&url, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n", b"");
    assert_eq!(code, 200);
    assert!(page.contains("<title>LEHU update</title>"));
    let (code, info) = http(&url, b"GET /info HTTP/1.1\r\n\r\n", b"");
    assert_eq!(code, 200);
    assert!(info.contains(r#""booted":"c1""#), "{info}");
    assert!(info.contains(r#""manifest_url":"https://example.org/os/manifest.json""#));
    assert!(info.contains(r#""complete":false"#));
    // Garbage and the unknown.
    assert_eq!(http(&url, b"BLAH\r\n\r\n", b"").0, 400);
    assert_eq!(http(&url, b"GET /etc/passwd HTTP/1.1\r\n\r\n", b"").0, 404);
    assert_eq!(http(&url, b"DELETE /info HTTP/1.1\r\n\r\n", b"").0, 405);
    let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(9000));
    assert_eq!(http(&url, huge.as_bytes(), b"").0, 431);
    assert_eq!(
        http(&url, b"POST /upload?file=a.tar HTTP/1.1\r\n\r\n", b"").0,
        400
    );

    let data: Vec<u8> = (0..300_000u32).map(|i| (i ^ (i >> 8)) as u8).collect();
    let q = |offset: usize| {
        format!(
            "file=lehu-os-2.0-from-c1.tar&size={}&sha256={}&offset={offset}",
            data.len(),
            sha_hex(&data)
        )
    };
    // No length: refused.
    let head = format!("POST /upload?{} HTTP/1.1\r\n\r\n", q(0));
    assert_eq!(http(&url, head.as_bytes(), b"").0, 411);
    // The first 100 000 bytes, then the rest from where it stopped.
    let (code, body) = post(&url, &q(0), &data[..100_000]);
    assert_eq!(
        (code, body.as_str()),
        (200, r#"{"complete":false,"have":100000}"#)
    );
    let (_, info) = http(&url, b"GET /info HTTP/1.1\r\n\r\n", b"");
    assert!(
        info.contains(r#""file":"lehu-os-2.0-from-c1.tar","have":100000"#),
        "{info}"
    );
    let (code, body) = post(&url, &q(0), &data);
    assert_eq!(code, 409);
    assert!(body.contains(r#""have":100000"#));
    let (code, body) = post(&url, &q(100_000), &data[100_000..]);
    assert_eq!(code, 200, "{body}");
    assert!(body.contains(r#""complete":true"#));

    let v = wait_for(&u, "the check", |v| v.stage == Stage::Ready);
    assert_eq!(v.version, "2.0");
    assert_eq!(v.mode, Mode::InPlace);
    let (_, state) = http(&url, b"GET /state HTTP/1.1\r\n\r\n", b"");
    assert!(state.contains(r#""stage":"ready""#), "{state}");
    assert_eq!(names(&cache), ["lehu-os-2.0-from-c1.tar"]);

    // Install: the page stops, apply's progress shows, and it ends with
    // the head unit restarting by itself.
    assert!(u.install());
    assert!(!u.install(), "once");
    assert!(u.busy());
    assert!(!u.view().serving);
    let v = wait_for(&u, "the progress", |v| v.percent == 60);
    assert_eq!(v.message, "Installing the update");
    let v = wait_for(&u, "the end", |v| v.stage == Stage::Restarting);
    assert_eq!(v.percent, 100);
    assert!(u.busy());
    assert!(wakes.load(Ordering::Relaxed) > 5);
    let _ = root;
}

#[test]
fn a_socket_that_says_nothing_does_not_hold_up_the_next() {
    let (_root, cfg) = setup("idlesocket");
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the status", |v| !v.booted.is_empty());
    u.screens(true);
    u.serve(Some(Ipv4Addr::LOCALHOST));
    let url = u.view().url;
    // As a browser opens one ahead of its requests.
    let silent = TcpStream::connect(url.trim_start_matches("http://").trim_end_matches('/'));
    assert!(silent.is_ok());
    let t0 = Instant::now();
    assert_eq!(http(&url, b"GET /state HTTP/1.1\r\n\r\n", b"").0, 200);
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
}

#[test]
fn a_failed_install_says_why_and_frees_the_phone() {
    let (root, cfg) = setup("applyfail");
    fs::write(cfg.cache.join("u.tar"), b"x").unwrap();
    fs::write(root.join("apply-fails"), "").unwrap();
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the check", |v| v.stage == Stage::Ready);
    assert!(u.install());
    let v = wait_for(&u, "the failure", |v| v.stage == Stage::Failed);
    assert_eq!(v.message, "The pull failed");
    assert!(!u.busy());
}

#[test]
fn cancel_removes_what_arrived_and_tells_the_page() {
    let (_root, cfg) = setup("cancel");
    let cache = cfg.cache.clone();
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the status", |v| !v.booted.is_empty());
    assert!(!u.cancel(), "nothing to cancel");

    // Part of one, waiting for the phone: the part goes.
    u.shared.change(|inner| {
        let v = &mut inner.view;
        v.file = "p.tar".into();
        v.size = 100;
        v.received = 10;
    });
    fs::write(cache.join("p.tar.part"), [0u8; 10]).unwrap();
    assert!(u.cancel());
    assert!(!cache.join("p.tar.part").exists());
    let v = u.view();
    assert_eq!((v.stage, v.received, v.cancels), (Stage::Idle, 0, 1));
    assert!(!v.can_cancel());

    // Checked: the installer's discard removes it.
    fs::write(cache.join("c.tar"), b"x").unwrap();
    u.shared.change(|inner| {
        let v = &mut inner.view;
        v.stage = Stage::Ready;
        v.file = "c.tar".into();
    });
    assert!(u.cancel());
    assert_eq!(u.view().stage, Stage::Idle);
    assert_eq!(u.view().cancels, 2);
}

#[test]
fn nothing_cancels_a_check_or_an_install() {
    let mut v = View::default();
    for (stage, can) in [
        (Stage::Idle, false),
        (Stage::Receiving, true),
        (Stage::Checking, false),
        (Stage::Ready, true),
        (Stage::Failed, false),
        (Stage::Installing, false),
        (Stage::Restarting, false),
    ] {
        v.stage = stage;
        assert_eq!(v.can_cancel(), can, "{stage:?}");
    }
    v.stage = Stage::Failed;
    v.file = "u.tar".into();
    assert!(v.can_cancel(), "a failed install left it there");
}

#[test]
fn a_transfer_cancelled_midway_leaves_nothing() {
    let (_root, cfg) = setup("cancelmid");
    let cache = cfg.cache.clone();
    let sh = shared(cfg);
    let data = [7u8; 64];
    let up = upload("m.tar", &data, 0);
    begin(&sh, &up, 64).unwrap();
    // As `Updater::cancel` leaves it; the connection then ends early.
    sh.lock().cancel_upload = true;
    let mut chunk = [0u8; 16];
    let stop = AtomicBool::new(false);
    let got = receive(&sh, &up, 64, &mut &data[..10], &mut chunk, &stop);
    assert_eq!(got, Received::Cut(10));
    assert!(!cache.join("m.tar.part").exists());
    let v = sh.lock().view.clone();
    assert_eq!((v.stage, v.received, v.cancels), (Stage::Idle, 0, 1));
}

#[test]
fn a_restart_that_never_comes_frees_the_phone() {
    let (_root, mut cfg) = setup("norestart");
    cfg.restart_wait = Duration::from_millis(200);
    fs::write(cfg.cache.join("u.tar"), b"x").unwrap();
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the check", |v| v.stage == Stage::Ready);
    assert!(u.install());
    wait_for(&u, "the end", |v| v.stage == Stage::Restarting);
    assert!(u.busy());
    let v = wait_for(&u, "the wait", |v| v.stage == Stage::Failed);
    assert!(
        v.message.starts_with("The head unit did not restart"),
        "{v:?}"
    );
    assert!(!u.busy());
}

#[test]
fn a_failed_check_says_why() {
    let (root, cfg) = setup("checkfail");
    fs::write(root.join("check-fails"), "").unwrap();
    let data = [3u8; 64];
    let sh = shared(cfg);
    let up = upload("f.tar", &data, 0);
    begin(&sh, &up, 64).unwrap();
    assert_eq!(
        receive(
            &sh,
            &up,
            64,
            &mut &data[..],
            &mut [0; 64],
            &AtomicBool::new(false)
        ),
        Received::Complete
    );
    // Run the queued check as the worker would.
    let job = sh.lock().jobs.pop_front();
    match job {
        Some(Job::Check { tar, quiet }) => run_check(&sh, &tar, quiet),
        _ => panic!("no check queued"),
    }
    let v = sh.lock().view.clone();
    assert_eq!(v.stage, Stage::Failed);
    assert_eq!(v.message, "The update is not signed with LEHU OS's key");
}

#[test]
fn the_page_stops_when_left_alone() {
    let (_root, mut cfg) = setup("idle");
    cfg.idle_stop = Duration::from_millis(200);
    let u = Updater::new(cfg, || {});
    wait_for(&u, "the status", |v| !v.booted.is_empty());
    u.screens(true);
    u.serve(Some(Ipv4Addr::LOCALHOST));
    assert!(u.view().serving);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(u.view().serving, "not while the pages show");
    u.screens(false);
    wait_for(&u, "the stop", |v| !v.serving);
    // Opened again, it serves again.
    u.screens(true);
    u.serve(Some(Ipv4Addr::LOCALHOST));
    assert!(u.view().serving);
}

#[test]
fn no_installer_nothing_to_do() {
    let (_root, mut cfg) = setup("none");
    cfg.installer = cfg.installer.with_file_name("missing");
    let u = Updater::new(cfg, || {});
    let v = u.view();
    assert!(!v.available);
    u.serve(Some(Ipv4Addr::LOCALHOST));
    assert!(!u.view().serving);
    assert!(!u.install());
    // No access point address: said.
    let (_root, cfg) = setup("noap");
    let u = Updater::new(cfg, || {});
    u.serve(None);
    assert_eq!(u.view().serve_error, "The access point is not running");
}

#[test]
fn the_installers_lock_tells_an_apply_runs() {
    let root = store::tests::dir("lock");
    let lock = root.join("lock");
    // No lock file: nothing ever ran.
    assert!(!lock_held(&lock));
    // As `apply` leaves it: there, and nobody holds it.
    fs::write(&lock, b"").unwrap();
    assert!(!lock_held(&lock));
    // `apply` runs: it holds the lock exclusively.
    let apply = fs::File::create(&lock).unwrap();
    apply.lock().unwrap();
    assert!(lock_held(&lock));
    // Asking took nothing from it.
    assert!(lock_held(&lock));
    drop(apply);
    assert!(!lock_held(&lock));
}
