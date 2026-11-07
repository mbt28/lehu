//! Finds the three C libraries this crate speaks to and compiles the AAC
//! helper. Everything comes from pkg-config: the Buildroot wrapper for the
//! board (`scripts/env.sh` sets `PKG_CONFIG`), the system one for the host.
//! A host that has a library's runtime file but no development package
//! (`libasound.so.2` without `alsa.pc`, say) is linked to that file by name,
//! so the tests that need it still run; a host with nothing gets a stub
//! that fails to open (`lehu_audio_no_alsa`, `lehu_audio_no_opus`,
//! `lehu_audio_no_aac`). For the board nothing is optional.

use std::env;
use std::path::Path;
use std::process::Command;

struct Flags {
    includes: Vec<String>,
    other: Vec<String>,
    search: Vec<String>,
    libs: Vec<String>,
}

/// pkg-config's answer for the packages, None when it has none.
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

fn link(flags: &Flags) {
    for dir in &flags.search {
        println!("cargo:rustc-link-search=native={dir}");
    }
    for lib in &flags.libs {
        println!("cargo:rustc-link-lib={lib}");
    }
}

/// The host's runtime copy of a library (`libopus.so.0`), when there is no
/// development package: linked by its file name.
fn runtime_library(target: &str, file: &str) -> bool {
    let arch = target.split('-').next().unwrap_or_default();
    let multiarch = format!("{arch}-linux-gnu");
    let dirs = [
        format!("/usr/lib/{multiarch}"),
        format!("/lib/{multiarch}"),
        "/usr/lib64".to_string(),
        "/usr/lib".to_string(),
        "/usr/local/lib".to_string(),
    ];
    for dir in dirs {
        if Path::new(&dir).join(file).exists() {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=dylib:+verbatim={file}");
            return true;
        }
    }
    false
}

fn main() {
    for cfg in [
        "lehu_audio_no_alsa",
        "lehu_audio_no_opus",
        "lehu_audio_no_aac",
    ] {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    println!("cargo:rerun-if-changed=csrc/aac.c");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=LEHU_AUDIO_STUB");

    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let native = host == target;

    // With env.sh sourced PKG_CONFIG is the Buildroot wrapper, whose answer
    // is the ARM sysroot: right for the board, wrong for the host's own test
    // binary, which takes the system pkg-config instead.
    let tool = if native {
        if Path::new("/usr/bin/pkg-config").exists() {
            "/usr/bin/pkg-config".to_string()
        } else {
            "pkg-config".to_string()
        }
    } else {
        env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_string())
    };
    let forced_stub = env::var("LEHU_AUDIO_STUB").is_ok_and(|v| v == "1");
    let probe = |packages: &[&str]| {
        if forced_stub {
            None
        } else {
            pkg_config(&tool, packages)
        }
    };
    let missing = |what: &str| {
        assert!(
            native,
            "lehu-audio: {tool} found no {what} for {target}; source scripts/env.sh"
        );
        println!("cargo:warning=lehu-audio: no {what} on this host: a stub here");
    };

    match probe(&["alsa"]) {
        Some(f) => link(&f),
        None if !forced_stub && native && runtime_library(&target, "libasound.so.2") => {}
        None => {
            missing("ALSA (pkg-config alsa)");
            println!("cargo:rustc-cfg=lehu_audio_no_alsa");
        }
    }
    match probe(&["opus"]) {
        Some(f) => link(&f),
        None if !forced_stub && native && runtime_library(&target, "libopus.so.0") => {}
        None => {
            missing("libopus (pkg-config opus)");
            println!("cargo:rustc-cfg=lehu_audio_no_opus");
        }
    }
    match probe(&["libavcodec", "libavutil"]) {
        Some(f) => {
            let mut build = cc::Build::new();
            build.file("csrc/aac.c").warnings(true);
            for dir in &f.includes {
                build.include(dir);
            }
            for flag in &f.other {
                build.flag(flag);
            }
            build.compile("lehu_audio_aac");
            link(&f);
        }
        None => {
            missing("ffmpeg headers (pkg-config libavcodec libavutil)");
            println!("cargo:rustc-cfg=lehu_audio_no_aac");
        }
    }
}
