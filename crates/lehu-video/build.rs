//! Compiles the C helper that drives ffmpeg's v4l2-request hwaccel. The
//! headers and libraries come from pkg-config: the Buildroot wrapper for the
//! board (`scripts/env.sh` sets `PKG_CONFIG`), the system one for the host.
//! A host without ffmpeg's headers still builds and tests: the helper is
//! then a stub that reports the decoder unavailable (`lehu_video_stub`).
//! For the board the helper is not optional.

use std::env;
use std::process::Command;

struct Flags {
    includes: Vec<String>,
    other: Vec<String>,
    search: Vec<String>,
    libs: Vec<String>,
}

/// pkg-config's answer for the libraries, None when it has none.
fn pkg_config(tool: &str, packages: &[&str]) -> Option<Flags> {
    let run = |arg: &str| -> Option<String> {
        let out = Command::new(tool).arg(arg).args(packages).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let cflags = run("--cflags")?;
    let libs = run("--libs")?;
    let mut f = Flags {
        includes: Vec::new(),
        other: Vec::new(),
        search: Vec::new(),
        libs: Vec::new(),
    };
    for w in cflags.split_whitespace() {
        match w.strip_prefix("-I") {
            Some(dir) => f.includes.push(dir.to_string()),
            None => f.other.push(w.to_string()),
        }
    }
    for w in libs.split_whitespace() {
        if let Some(dir) = w.strip_prefix("-L") {
            f.search.push(dir.to_string());
        } else if let Some(lib) = w.strip_prefix("-l") {
            f.libs.push(lib.to_string());
        }
    }
    Some(f)
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(lehu_video_stub)");
    println!("cargo:rerun-if-changed=csrc/decoder.c");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=LEHU_VIDEO_STUB");

    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let native = host == target;

    // With env.sh sourced PKG_CONFIG is the Buildroot wrapper, whose answer
    // is the ARM sysroot: right for the board, wrong for the host's own test
    // binary, which takes the system pkg-config instead.
    let tool = if native {
        if std::path::Path::new("/usr/bin/pkg-config").exists() {
            "/usr/bin/pkg-config".to_string()
        } else {
            "pkg-config".to_string()
        }
    } else {
        env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_string())
    };

    let forced_stub = env::var("LEHU_VIDEO_STUB").is_ok_and(|v| v == "1");
    let flags = if forced_stub {
        None
    } else {
        pkg_config(&tool, &["libavcodec", "libavutil"])
    };

    let Some(flags) = flags else {
        assert!(
            native,
            "lehu-video: {tool} found no libavcodec/libavutil for {target}; source scripts/env.sh"
        );
        println!(
            "cargo:warning=lehu-video: no ffmpeg headers on this host (pkg-config libavcodec libavutil): the decoder is a stub here"
        );
        println!("cargo:rustc-cfg=lehu_video_stub");
        return;
    };

    let mut build = cc::Build::new();
    build.file("csrc/decoder.c").warnings(true);
    for dir in &flags.includes {
        build.include(dir);
    }
    for flag in &flags.other {
        build.flag(flag);
    }
    build.compile("lehu_video_dec");
    for dir in &flags.search {
        println!("cargo:rustc-link-search=native={dir}");
    }
    for lib in &flags.libs {
        println!("cargo:rustc-link-lib={lib}");
    }
}
