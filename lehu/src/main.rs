// SPDX-License-Identifier: GPL-3.0-or-later
//! LEHU, a lightweight embedded head unit: the command line, then the main
//! loop in `run`.

mod art;
mod audio;
mod host;
mod input;
mod media;
mod mem;
mod run;
mod session;
mod status;

use std::path::PathBuf;
use std::process::ExitCode;

use lehu_core::cpu::Features;
use lehu_core::settings::{self, Paths};

const USAGE: &str = "\
lehu [--image FILE] [--user FILE]       run
lehu --check-config [--image FILE] [--user FILE]
lehu --print-config [--image FILE] [--user FILE]   the settings in force, as TOML
lehu --import PRESET [USERSETTINGS] [--out FILE]   FastCarPlay's files as a LEHU user file
lehu --display-test                    colour bars, a moving band, a window: the overlay path end to end
lehu --camera-test [SECONDS]           the camera on the video plane for a while, its inputs and levels listed
lehu --decoders [--image FILE] [--user FILE]   the video decoders found, and what each phone is asked for
lehu --cpu | --version | --help

Defaults: /etc/lehu/lehu.toml (optional) and /var/lib/lehu/settings.toml,
or ~/.lehu/settings.toml for a user who may not write /var/lib/lehu; the
user's file is made on the first run. LEHU_CONFIG and LEHU_USER_CONFIG
name others.";

fn main() -> ExitCode {
    mem::tune();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut paths = Paths::system();
    let mut command = None;
    let mut positional = Vec::new();
    let mut out = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--image" => match it.next() {
                Some(p) => paths.image = PathBuf::from(p),
                None => return usage("--image needs a file"),
            },
            "--user" => match it.next() {
                Some(p) => paths.user = PathBuf::from(p),
                None => return usage("--user needs a file"),
            },
            "--out" => match it.next() {
                Some(p) => out = Some(PathBuf::from(p)),
                None => return usage("--out needs a file"),
            },
            "--version" => {
                println!("lehu {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            flag if flag.starts_with("--") => {
                if command.replace(flag.to_string()).is_some() {
                    return usage("one command at a time");
                }
            }
            other => positional.push(other.to_string()),
        }
    }

    match command.as_deref() {
        Some("--cpu") => {
            println!("{}", Features::detect());
            ExitCode::SUCCESS
        }
        Some("--decoders") => decoders(&paths),
        Some("--check-config") => match settings::load(&paths) {
            Ok(loaded) => {
                for note in &loaded.notes {
                    println!("note: {note}");
                }
                println!("ok: {} and {}", paths.image.display(), paths.user.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Some("--print-config") => match settings::load(&paths) {
            Ok(loaded) => {
                print!("{}", settings::to_toml(&loaded.settings));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Some("--import") => import(&positional, out),
        Some("--display-test") => display_test(),
        Some("--camera-test") => match settings::load(&paths) {
            Ok(loaded) => camera_test(&loaded.settings, &positional),
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Some(other) => usage(&format!("unknown option {other}")),
        None => {
            let features = Features::detect();
            match settings::load(&paths) {
                Ok(loaded) => {
                    let system = loaded.settings.system.clone();
                    match run::run(loaded, features) {
                        run::Exit::Done(code) => code,
                        run::Exit::Power(action) => power_down(action, &system),
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// `--decoders`: the probe's full report, then what each phone would be
/// asked for with the settings in force.
fn decoders(paths: &Paths) -> ExitCode {
    let caps = lehu_video::caps();
    print!("{}", caps.report());
    println!(
        "software: not used; the video plane shows the decoder's own buffers ({})",
        Features::detect()
    );
    let s = match settings::load(paths) {
        Ok(loaded) => loaded.settings,
        Err(e) => {
            eprintln!("{e}; with the defaults:");
            settings::Settings::default()
        }
    };
    let wanted = s.display.codec;
    let forced = match wanted {
        settings::VideoCodec::Auto => None,
        settings::VideoCodec::H264 => Some(lehu_video::Codec::H264),
        settings::VideoCodec::Hevc => Some(lehu_video::Codec::Hevc),
    };
    match forced {
        Some(c) if !caps.has(c) => {
            println!("display.codec = {c}: refused, nothing decodes it; auto")
        }
        Some(c) => println!("display.codec = {c}"),
        None => println!("display.codec = auto"),
    }
    let cp = lehu_video::carplay_codec(caps, wanted);
    let size = caps
        .max(cp)
        .map_or(String::new(), |(w, h)| format!(", within {w}x{h}"));
    println!("carplay: {cp} at the panel's size{size}");
    let mut offer = lehu_aa::Config::from_settings(&s).offer;
    offer.fit(caps, wanted);
    let (w, h) = offer.size();
    let codecs: Vec<&str> = offer.codecs().map(lehu_video::Codec::name).collect();
    println!(
        "android auto: {} at {w}x{h} (android-auto.resolution = {})",
        codecs.join(" then "),
        s.android_auto.resolution
    );
    ExitCode::SUCCESS
}

/// Start over as the same program with the same arguments: the init
/// script does not respawn LEHU, and a head unit with nothing on its
/// screen is worse than one starting again.
fn restart_self() -> ExitCode {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!("lehu: starting again");
    let err = std::process::Command::new(&exe).args(&args).exec();
    eprintln!("lehu: cannot start again ({}): {err}", exe.display());
    ExitCode::FAILURE
}

/// Settings > System: hand the head unit to the system's own reboot or
/// poweroff once the loop has closed everything down (the phone session,
/// the camera, the saved settings). init then stops every service, this
/// process included. Should the command fail (not permitted, not there)
/// or the system not go down, start again.
fn power_down(action: lehu_ui::Power, system: &settings::System) -> ExitCode {
    use std::time::Duration;
    let (what, command) = match action {
        lehu_ui::Power::Reboot => ("Rebooting", &system.reboot_command),
        lehu_ui::Power::Shutdown => ("Shutting down", &system.shutdown_command),
    };
    eprintln!("lehu: {what}: {command}");
    // SAFETY: sync takes nothing and cannot fail; init's TERM must end this
    // process now rather than ask it for a clean quit, so the handlers go
    // back to the defaults.
    unsafe {
        libc::sync();
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    let status = lehu_core::shell::privileged(command).status();
    match status {
        Ok(st) if st.success() => {
            std::thread::sleep(Duration::from_secs(60));
            eprintln!("lehu: still running 60 s after '{command}'");
        }
        Ok(st) => eprintln!("lehu: '{command}' failed ({st})"),
        Err(e) => eprintln!("lehu: '{command}': {e}"),
    }
    restart_self()
}

fn usage(why: &str) -> ExitCode {
    eprintln!("lehu: {why}\n{USAGE}");
    ExitCode::FAILURE
}

/// `--import PRESET [USERSETTINGS] [--out FILE]`: the old files become one
/// LEHU user file, written to `--out` or printed.
fn import(files: &[String], out: Option<PathBuf>) -> ExitCode {
    if files.is_empty() || files.len() > 2 {
        return usage("--import takes the preset and, optionally, usersettings.txt");
    }
    let mut texts = Vec::new();
    for (i, path) in files.iter().enumerate() {
        match std::fs::read_to_string(path) {
            Ok(t) => texts.push((path.as_str(), t)),
            // No user file yet is the usual case on a fresh unit.
            Err(e) if i == 1 && e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("note: {path} does not exist; importing the preset alone");
            }
            Err(e) => {
                eprintln!("{path}: cannot read: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let pairs: Vec<(&str, &str)> = texts.iter().map(|(n, t)| (*n, t.as_str())).collect();
    let result = settings::import::from_fastcarplay(&pairs);
    for w in &result.warnings {
        eprintln!("warning: {w}");
    }
    // The result must load as settings before it is called an import.
    let mut merged = settings::Settings::defaults_table();
    settings::merge(&mut merged, result.table.clone());
    if let Err(e) = toml::Value::Table(merged).try_into::<settings::Settings>() {
        eprintln!("the imported settings do not load: {}", e.message());
        return ExitCode::FAILURE;
    }
    let text = match toml::to_string_pretty(&result.table) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot write TOML: {e}");
            return ExitCode::FAILURE;
        }
    };
    match out {
        Some(path) => match std::fs::write(&path, text) {
            Ok(()) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}: cannot write: {e}", path.display());
                ExitCode::FAILURE
            }
        },
        None => {
            print!("{text}");
            ExitCode::SUCCESS
        }
    }
}

/// `--display-test`: take the display, paint colour bars on the overlay,
/// move a band across them with partial pushes, then show a window of the
/// overlay the way the volume bar is shown over video. Exercises the DRM
/// crate end to end on real hardware, which no host test can.
/// `--camera-test [SECONDS]`: take the display, start the camera as the
/// view would, show it for a while and say what it found: the inputs, the
/// standard, the levels, whether a picture came. The DRM and V4L2 paths
/// together on real hardware, which no host test can.
fn camera_test(settings: &settings::Settings, args: &[String]) -> ExitCode {
    use std::sync::Arc;
    use std::thread::sleep;
    use std::time::Duration;

    lehu_core::log::init(settings::LogLevel::Debug);
    let seconds: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(5);
    let display = match lehu_drm::Display::open_any() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("display: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "camera present: {}",
        lehu_camera::present(&settings.camera.device)
    );
    let mut camera = lehu_camera::Camera::new(display);
    if let Err(e) = camera.start(&settings.camera) {
        eprintln!("camera: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "inputs {:?}, input {}, standard {} ({}), signal {}",
        camera.inputs(),
        camera.input(),
        camera.standard_label(),
        if camera.has_standard() {
            "selectable"
        } else {
            "fixed"
        },
        if camera.no_signal() { "none" } else { "yes" }
    );
    for level in camera.levels() {
        println!("  {:<12} {}", level.label, level.value);
    }
    let mut first_at = None;
    for i in 0..seconds * 10 {
        sleep(Duration::from_millis(100));
        if let Some(save) = camera.tick() {
            println!(
                "save asked: input {} with {} profiles",
                save.input,
                save.inputs.len()
            );
        }
        if first_at.is_none() && camera.picture_ready() {
            first_at = Some(i * 100);
        }
        if camera.failed() {
            eprintln!("camera failed: {}", camera.error());
            return ExitCode::FAILURE;
        }
    }
    println!(
        "picture ready after {} ms, signal {}, generation {}",
        first_at.map_or("never".to_string(), |ms| ms.to_string()),
        if camera.no_signal() { "none" } else { "yes" },
        camera.generation()
    );
    camera.stop();
    println!("stopped; CmaFree should be back: {}", cma_free());
    ExitCode::SUCCESS
}

fn cma_free() -> String {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("CmaFree"))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn display_test() -> ExitCode {
    use lehu_core::poll::{Interest, Poller};
    use std::os::fd::AsRawFd;
    use std::thread::sleep;
    use std::time::Duration;

    let display = match lehu_drm::Display::open_any() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("display: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (w, h) = (display.width() as usize, display.height() as usize);
    println!(
        "display {w}x{h}, ui {:?} {} bytes/px pitch {}, panel {:?} mm",
        display.ui_format(),
        display.ui_bpp(),
        display.ui_pitch(),
        display.size_mm()
    );
    if !display.has_ui() || display.ui_bpp() != 2 {
        eprintln!("the test paints RGB565 only");
        return ExitCode::FAILURE;
    }
    let rgb565 =
        |r: u32, g: u32, b: u32| -> u16 { (((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)) as u16 };
    const BARS: [(u32, u32, u32); 8] = [
        (235, 235, 235),
        (235, 235, 16),
        (16, 235, 235),
        (16, 235, 16),
        (235, 16, 235),
        (235, 16, 16),
        (16, 16, 235),
        (16, 16, 16),
    ];
    let mut frame = vec![0u8; w * h * 2];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = BARS[x * 8 / w];
            let i = (y * w + x) * 2;
            frame[i..i + 2].copy_from_slice(&rgb565(r, g, b).to_le_bytes());
        }
    }
    let t0 = std::time::Instant::now();
    display.ui_write_rect(0, 0, w as u32, h as u32, &frame, w * 2);
    let shown = display.ui_show(0, h as i32);
    println!("bars: full push {:?}, shown {shown}", t0.elapsed());
    sleep(Duration::from_millis(1500));

    // A 40-row amber band sweeping down: each step writes only the rows that
    // changed (the old band's rows restored, the new band's rows painted).
    let band = 40usize;
    let amber: Vec<u8> = (0..w * band)
        .flat_map(|_| rgb565(242, 162, 28).to_le_bytes())
        .collect();
    let mut last = None;
    let t0 = std::time::Instant::now();
    let steps = (h - band) / 8;
    for step in 0..steps {
        let y = step * 8;
        if let Some(old) = last {
            display.ui_write_rect(
                0,
                old as u32,
                w as u32,
                band as u32,
                &frame[old * w * 2..],
                w * 2,
            );
        }
        display.ui_write_rect(0, y as u32, w as u32, band as u32, &amber, w * 2);
        let y0 = last.unwrap_or(y).min(y);
        let y1 = last.unwrap_or(y).max(y) + band;
        display.ui_show(y0 as i32, y1 as i32);
        last = Some(y);
        sleep(Duration::from_millis(16));
    }
    println!("band: {steps} partial pushes in {:?}", t0.elapsed());
    sleep(Duration::from_millis(500));

    // The window over video: hide, then a 480x64 window at the foot, as the
    // volume bar, with a non-blocking commit and the flip event drained.
    display.ui_hide(true);
    sleep(Duration::from_millis(400));
    let mut poller = Poller::with_capacity(1);
    let idx = poller
        .add(display.fd().as_raw_fd(), Interest::Read)
        .unwrap_or(0);
    let (wx, wy) = ((w as i32 - 480) / 2, h as i32 - 64 - 16);
    let mut shown = display.ui_show_window(wx, wy, 480, 64, false);
    let mut events = 0;
    for _ in 0..10 {
        if poller.wait(Some(Duration::from_millis(50))).unwrap_or(0) > 0 && poller.readable(idx) {
            events += display.handle_events();
        }
        if !shown {
            shown = display.ui_show_window(wx, wy, 480, 64, false);
        }
    }
    println!("window: shown {shown}, flip events {events}");
    sleep(Duration::from_millis(1200));
    display.ui_hide(true);
    println!("display test done");
    ExitCode::SUCCESS
}
