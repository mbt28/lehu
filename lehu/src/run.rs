//! The main loop: one `poll()` the whole program waits in.
//!
//! The loop owns the display, the screens and, in time, the session state.
//! Everything else runs on its own thread and reaches the loop through a
//! descriptor: the wake eventfd (input, and later the protocols), the
//! timerfd for the nearest deadline, the DRM card for flip completions. The
//! screens' own timers set the poll timeout. An idle loop sleeps in the
//! kernel.

use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use lehu_camera::Camera;
use lehu_core::cpu::Features;
use lehu_core::poll::{Interest, Poller, Timer, Wake};
use lehu_core::settings::{Loaded, VideoPath};
use lehu_drm::Display;
use lehu_input::{Config as InputConfig, Input, Nav, Phase, Route, TOUCH_SCALE};
use lehu_session::Link;
use lehu_ui::{CameraRequest, Host, Osd, OsdKey, Power, Rgb565Pixel, Surface};

use crate::audio::Audio;
use crate::host::AppHost;
use crate::input::{Event, Hub};
use crate::session::Driver;

/// Set by SIGINT and SIGTERM; the loop ends its current turn and leaves.
static STOP: AtomicBool = AtomicBool::new(false);
/// The wake eventfd, so a signal ends the poll at once instead of at its
/// next timeout. An eventfd write is async-signal-safe.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    let fd = WAKE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one: u64 = 1;
        // SAFETY: eventfd write of 8 bytes from a live u64; allowed in a
        // signal handler.
        unsafe {
            libc::write(fd, (&one as *const u64).cast(), 8);
        }
    }
}

fn install_signals(wake: &Wake) {
    WAKE_FD.store(wake.as_raw_fd(), Ordering::SeqCst);
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a zeroed sigaction filled in before use; the handler only
        // touches atomics and an eventfd.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

pub fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// The overlay plane as the screens' surface: changed rows go straight
/// into the plane's buffer, and after the tick the plane is shown over the
/// whole panel (a no-op when it already is).
struct Plane {
    display: Arc<Display>,
    width: u32,
    height: u32,
    written: bool,
}

impl Surface for Plane {
    fn write_rows(&mut self, y0: u32, y1: u32, pixels: &[Rgb565Pixel]) {
        if y1 <= y0 {
            return;
        }
        // SAFETY: Rgb565Pixel is repr(transparent) over u16, so the pixels
        // are their bytes in native order: the plane's RG16 on this target.
        let bytes =
            unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), pixels.len() * 2) };
        if self
            .display
            .ui_write_rect(0, y0, self.width, y1 - y0, bytes, self.width as usize * 2)
        {
            self.written = true;
        } else {
            log::warn!("ui: rows {y0}..{y1} refused by the plane");
        }
    }
}

impl Plane {
    fn show(&mut self) {
        if std::mem::take(&mut self.written) {
            self.display.ui_show(0, self.height as i32);
        }
    }
}

/// The settings as the screens hold them (the live, persisted copy), or the
/// defaults when there are no screens.
fn host_settings(screens: &Option<(Osd<AppHost>, Plane)>) -> &lehu_core::settings::Settings {
    static DEFAULT: std::sync::OnceLock<lehu_core::settings::Settings> = std::sync::OnceLock::new();
    match screens {
        Some((osd, _)) => osd.host().settings(),
        None => DEFAULT.get_or_init(lehu_core::settings::Settings::default),
    }
}

fn running_now(osd: &mut Osd<AppHost>) -> bool {
    osd.host_mut().camera_mut().is_some_and(|c| c.running())
}

fn no_camera() -> lehu_camera::Error {
    lehu_camera::Error {
        text: "No display for the camera".into(),
        no_memory: false,
    }
}

fn nav_key(nav: Option<Nav>) -> OsdKey {
    match nav {
        None => OsdKey::None,
        Some(Nav::Up) => OsdKey::Up,
        Some(Nav::Down) => OsdKey::Down,
        Some(Nav::Left) => OsdKey::Left,
        Some(Nav::Right) => OsdKey::Right,
        Some(Nav::Ok) => OsdKey::Ok,
        Some(Nav::Back) => OsdKey::Back,
        Some(Nav::Erase) => OsdKey::Erase,
        Some(Nav::Home) => OsdKey::Home,
        Some(Nav::Menu) => OsdKey::Menu,
        Some(Nav::Next) => OsdKey::Next,
        Some(Nav::Prev) => OsdKey::Prev,
    }
}

/// How the loop ended, for main to act on once everything is closed.
#[derive(Clone, Copy, Debug)]
pub enum Exit {
    /// A signal, or an error that leaves nothing to restart into.
    Done(ExitCode),
    /// Reboot or power off, through the system's own command.
    Power(Power),
}

/// Run until SIGTERM or SIGINT, or until the screens ask to restart or
/// power down.
pub fn run(loaded: Loaded, features: Features) -> Exit {
    let Loaded {
        settings,
        mut user,
        notes,
        image,
    } = loaded;
    lehu_core::log::init(settings.system.log_level);
    log::info!(
        "lehu {} on {}: {features}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    );
    match user.create_if_missing() {
        Ok(true) => log::info!("settings: made {}", user.path().display()),
        Ok(false) => log::info!("settings: {}", user.path().display()),
        Err(e) => log::warn!("settings: {e}; changes will not be kept"),
    }
    for note in &notes {
        log::warn!("settings: {note}");
    }

    let display = match settings.display.video_path {
        VideoPath::Headless => None,
        _ => match Display::open_any() {
            Ok(d) => {
                log::info!(
                    "display {}x{}, ui {:?} on the overlay: {}",
                    d.width(),
                    d.height(),
                    d.ui_format(),
                    d.has_ui()
                );
                Some(Arc::new(d))
            }
            // Something else owns the display (a compositor, another LEHU):
            // run without one rather than fight it, as FastCarPlay did.
            Err(lehu_drm::Error::NotMaster(why)) => {
                log::warn!("display: not the master ({why}); running headless");
                None
            }
            Err(e) if settings.display.video_path == VideoPath::Auto => {
                log::warn!("display: {e}; running headless");
                None
            }
            Err(e) => {
                log::error!("display: {e}");
                return Exit::Done(ExitCode::FAILURE);
            }
        },
    };

    let (wake, timer) = match (Wake::new(), Timer::new()) {
        (Ok(w), Ok(t)) => (w, t),
        (Err(e), _) | (_, Err(e)) => {
            log::error!("cannot create the loop's descriptors: {e}");
            return Exit::Done(ExitCode::FAILURE);
        }
    };
    let wake = Arc::new(wake);
    install_signals(&wake);

    let mut poller = Poller::with_capacity(16);
    let wake_idx = poller.add(wake.as_raw_fd(), Interest::Read);
    let timer_idx = poller.add(timer.as_raw_fd(), Interest::Read);
    let drm_idx = display
        .as_ref()
        .and_then(|d| poller.add(d.fd().as_raw_fd(), Interest::Read));
    if wake_idx.is_none() || timer_idx.is_none() {
        log::error!("the poller is too small");
        return Exit::Done(ExitCode::FAILURE);
    }

    // The screens: on the overlay plane when there is one. The controller
    // lives on this thread; the input thread reaches it through the hub.
    let (width, height) = display
        .as_ref()
        .map_or((800, 480), |d| (d.width(), d.height()));
    let input_config = InputConfig::from_settings(&settings);
    let mut camera_key = settings.camera.key;
    // The audio runs for the program's lifetime, shared by the screens and
    // the sessions. The protocols' state (pairings, identities) lives
    // beside the user's settings file.
    let audio = Arc::new(Audio::start(&settings.audio));
    let state_dir = user.path().parent().map_or_else(
        || std::path::PathBuf::from("/var/lib/lehu"),
        std::path::Path::to_path_buf,
    );
    // Input: the thread calls the hub, the hub queues and wakes the loop.
    let hub = Hub::new(Arc::clone(&wake));
    // Bluetooth runs for the program's life, in every source: the HID
    // devices of the screens need it, and the wireless phones. Without
    // bluetoothd the screens show Bluetooth as off.
    let bt = {
        let hub = hub.clone();
        let wake = Arc::clone(&wake);
        match lehu_bt::Manager::start(&settings.bluetooth, move |event| match event {
            lehu_bt::BtEvent::PhoneConnected(a) => hub.bt_phone(a, true),
            lehu_bt::BtEvent::PhoneDisconnected(a) => hub.bt_phone(a, false),
            lehu_bt::BtEvent::Changed => wake.wake(),
        }) {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                log::warn!("bluetooth: {e}; running without it");
                None
            }
        }
    };
    let host = AppHost::new(
        settings,
        user,
        image,
        display.clone(),
        Arc::clone(&audio),
        bt.clone(),
    );
    let mut screens = match &display {
        Some(d) if d.has_ui() => match Osd::new(host, width, height) {
            Ok(osd) => Some((
                osd,
                Plane {
                    display: Arc::clone(d),
                    width,
                    height,
                    written: false,
                },
            )),
            Err(e) => {
                log::error!("screens: {e}");
                return Exit::Done(ExitCode::FAILURE);
            }
        },
        _ => {
            log::warn!("no overlay plane: running without the screens");
            None
        }
    };

    hub.set_phone_takes(false); // no session yet: buttons stay on the screens
    let input = match Input::start(input_config, hub.clone()) {
        Ok(i) => {
            i.route(Route::Screens);
            Some(i)
        }
        Err(e) => {
            log::warn!("input: {e}; running without touch and keys");
            None
        }
    };

    // The phone session: none yet, the driver holds the decoder and the
    // front once a protocol starts one. USB attach and detach reach the
    // loop through the hub, like input.
    let mut driver = Driver::new(
        Arc::clone(&wake),
        display.clone(),
        host_settings(&screens),
        Arc::clone(&audio),
        bt.clone(),
        state_dir,
    );
    let mut bt_session_active = false;
    let mut status_file = crate::status::StatusFile::new("/run/lehu/status.json");
    let watcher = {
        let hub = hub.clone();
        match lehu_usb::Watcher::start(move |event| hub.usb(event)) {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("usb: {e}; no hotplug");
                None
            }
        }
    };
    // Who has the panel: the screens whole, or a window of them over the
    // phone's video or the camera's picture.
    let mut over_video = false;
    // The camera view ended or failed while it is still up: not again
    // until it is left.
    let mut camera_done = false;
    // The settings' generation the program last followed.
    let mut settings_seen = 0;
    let mut mem_watch = crate::mem::Watch::new();

    log::info!("running");
    let mut exit = Exit::Done(ExitCode::SUCCESS);
    while !stopping() {
        // The screens' timers and animations bound the wait, and their own
        // moments (the volume bar or the navigation card going, the Media
        // page's position line); otherwise the loop sleeps until a
        // descriptor is ready.
        let timeout = screens.as_ref().and_then(|(osd, _)| {
            match (lehu_ui::next_timer_due(), osd.next_due()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        });
        match poller.wait(timeout) {
            Ok(_) => {}
            Err(e) => {
                log::error!("poll: {e}");
                break;
            }
        }
        if wake_idx.is_some_and(|i| poller.readable(i)) {
            wake.drain();
            while let Some(event) = hub.pop() {
                match (&mut screens, event) {
                    (
                        Some((osd, _)),
                        Event::Touch {
                            route: Route::Screens,
                            count,
                            contacts,
                        },
                    ) => {
                        if count > 0 {
                            let c = contacts[0];
                            osd.pointer(
                                (u32::from(c.x) * width / u32::from(TOUCH_SCALE)) as i32,
                                (u32::from(c.y) * height / u32::from(TOUCH_SCALE)) as i32,
                                c.phase != Phase::Up,
                            );
                        }
                    }
                    (Some((osd, _)), Event::Key { nav, ch, value }) => {
                        osd.key(nav_key(nav), ch, value as i32);
                    }
                    (Some((osd, _)), Event::Volume(step)) => {
                        osd.host_mut().volume_step(i32::from(step))
                    }
                    (Some((osd, _)), Event::Mute) => osd.host_mut().toggle_mute(),
                    (Some((osd, _)), Event::CameraToggle) => {
                        osd.host_mut().request_camera(CameraRequest::Toggle);
                    }
                    (Some((osd, _)), Event::KeyLearned(code)) => osd.host_mut().key_learned(code),
                    (_, Event::Phone { button, value }) => driver.button(button, value),
                    (
                        _,
                        Event::Touch {
                            route: Route::Phone,
                            count,
                            contacts,
                        },
                    ) => driver.touch(count, contacts),
                    (_, Event::Usb(event)) => driver.usb(event),
                    (_, Event::BtPhone(address, connected)) => driver.bt_phone(address, connected),
                    (None, other) => log::debug!("input without screens: {other:?}"),
                }
            }
        }
        if timer_idx.is_some_and(|i| poller.readable(i)) {
            timer.read();
        }
        if let (Some(d), Some(i)) = (&display, drm_idx)
            && poller.readable(i)
        {
            d.handle_events();
            driver.on_flip();
        }

        driver.drain();
        // A setting changed on the screens applies at once: the output and
        // the microphone, the Bluetooth name, Android Auto's frame rate.
        if let Some((osd, _)) = &screens
            && osd.host().settings_generation() != settings_seen
        {
            let host = osd.host();
            settings_seen = host.settings_generation();
            let s = host.settings();
            audio.set_devices(&s.audio);
            driver.set_android_fps(s.android_auto.fps);
            if let Some(bt) = &bt {
                bt.set_name(&s.bluetooth.name);
            }
        }
        let due = driver.next_due();
        status_file.update(&driver.status_fields());
        mem_watch.tick(lehu_core::clock::now_ms());
        if let Err(e) = timer.arm(due) {
            log::warn!("timer: {e}");
        }
        if let Some(bt) = &bt {
            let active = driver.wireless_active();
            if active != bt_session_active {
                bt_session_active = active;
                bt.session_active(active);
            }
        }
        if let Some((osd, _)) = &mut screens {
            let host = osd.host_mut();
            if host.take_resume() {
                driver.resume();
            }
            if host.take_ap_changed() {
                driver.access_point_changed();
            }
            // The links as the header shows them: the one the session runs
            // over, not a phone merely plugged in.
            let backgrounded = driver.active() && !driver.phone_front();
            let usb = driver.active() && driver.link() == Some(Link::Usb);
            let wifi = driver.active() && driver.link() == Some(Link::Wifi);
            host.set_session(driver.take_status(), backgrounded, usb, wifi);
            if host.devices_seen() != driver.registry().generation() {
                host.set_devices(driver.registry());
            }
            host.set_switch_question(driver.switch_question());
            // Now playing and the route guidance, when the session's moved.
            if host.media_seen() != driver.now_playing().0 {
                host.set_media(driver.now_playing());
            }
            host.set_zone(driver.zone_min());
            if host.nav_seen() != driver.guidance().0 {
                host.set_guidance(driver.guidance(), driver.zone_min());
            }
            if let Some(art) = driver.take_art() {
                host.set_art(art);
            }
            if let Some(key) = host.take_media_key() {
                driver.media_key(match key {
                    lehu_ui::MediaKey::Previous => lehu_input::PhoneButton::PrevTrack,
                    lehu_ui::MediaKey::PlayPause => lehu_input::PhoneButton::PlayPause,
                    lehu_ui::MediaKey::Next => lehu_input::PhoneButton::NextTrack,
                });
            }
            match host.take_device_action() {
                Some(crate::host::DeviceAction::Connect(id)) => driver.connect_device(&id),
                Some(crate::host::DeviceAction::Stop(id)) => driver.stop_device(&id),
                Some(crate::host::DeviceAction::Forget(id)) => driver.forget_device(&id),
                Some(crate::host::DeviceAction::Switch(yes)) => driver.answer_switch(yes),
                None => {}
            }
        }

        // Settings > Video > Camera: the camera owns the panel until its
        // view is closed, whatever the phone does meanwhile. It needs the
        // video memory the phone's decoder holds (~19 of 24 MiB): the
        // decoder is parked while the camera shows, and the phone, if it
        // had the screen, told so; both come back after. With fast switch
        // the decoder stays when the memory holds both.
        let mut camera_running = false;
        let mut camera_waiting = false;
        if let Some((osd, plane)) = &mut screens {
            osd.camera_requests(); // the camera key, from any screen or video
            let wanted = osd.host().camera_wanted();
            if !wanted {
                camera_done = false;
            }
            let (running, failed, no_memory, error) =
                osd.host_mut()
                    .camera_mut()
                    .map_or((false, false, false, String::new()), |c| {
                        (
                            c.running(),
                            c.failed(),
                            c.out_of_memory(),
                            c.error().to_string(),
                        )
                    });
            if running && (!wanted || failed) {
                // A restart that found no memory next to the decoder (fast
                // switch) starts again below, the decoder parked.
                let again = wanted && no_memory && driver.video_running();
                if failed && !again {
                    osd.host_mut().camera_failed(&error);
                }
                // Back to the screens: they cover the picture before it
                // goes, or the plane would show the phone's frames (fast
                // switch) or nothing while they are drawn.
                let covered = !wanted && !driver.phone_had_screen();
                if covered {
                    osd.invalidate();
                    if osd.tick(false, plane) {
                        plane.show();
                    }
                    over_video = false;
                }
                let save = osd.host_mut().camera_mut().and_then(Camera::stop);
                if let Some(save) = save {
                    osd.host_mut().persist_camera(save);
                }
                if again {
                    driver.park(true);
                } else {
                    camera_done = wanted;
                    driver.unpark();
                }
                crate::mem::trim();
            }
            if wanted && !camera_done && !running_now(osd) {
                // Fast switch: the camera first tries next to the decoder.
                let together = osd.host().settings().camera.fast_switch && driver.video_running();
                driver.park(!together);
                let settings = osd.host().settings().camera.clone();
                let mut started = osd
                    .host_mut()
                    .camera_mut()
                    .map_or(Err(no_camera()), |c| c.start(&settings));
                if let Err(e) = &started
                    && together
                    && e.no_memory
                {
                    log::info!("camera: no memory next to the phone's video, parking its decoder");
                    driver.park(true);
                    started = osd
                        .host_mut()
                        .camera_mut()
                        .map_or(Err(no_camera()), |c| c.start(&settings));
                }
                if let Err(e) = started {
                    osd.host_mut().camera_failed(&e.text);
                    camera_done = true;
                    driver.unpark();
                }
            }
            if let Some(camera) = osd.host_mut().camera_mut()
                && camera.running()
            {
                camera_running = true;
                // Not before its first frame is there: until then the plane
                // has nothing, or the phone's last frame (fast switch), so
                // the screens stay as they were (held by the view).
                camera_waiting = !over_video && !camera.picture_ready();
                if let Some(save) = camera.tick() {
                    osd.host_mut().persist_camera(save);
                }
            }
        }

        // Who has the panel. The phone's video (or the camera) is on the
        // primary plane; the screens' plane shows whole while they are in
        // front, and only the window the screens ask for (the volume bar,
        // the camera column) over the picture. The camera's column is the
        // screens' own, so the phone takes nothing while it runs.
        let phone_front = driver.phone_front() && !camera_running;
        let over_now = phone_front || (camera_running && !camera_waiting);
        hub.set_phone_takes(phone_front);
        if let Some(input) = &input {
            input.route(if phone_front {
                Route::Phone
            } else {
                Route::Screens
            });
        }
        if over_now != over_video {
            over_video = over_now;
            if let (Some((osd, _)), Some(d)) = (&mut screens, &display) {
                if over_now {
                    if phone_front {
                        osd.hidden();
                    }
                    d.ui_hide(false);
                } else {
                    osd.invalidate();
                }
            }
        }

        if let Some((osd, plane)) = &mut screens {
            if over_video {
                if camera_running {
                    osd.release_held();
                }
                if osd.tick(true, plane) {
                    plane.written = false; // only the window below goes up
                }
                if let Some(d) = &display {
                    match osd.over_video() {
                        Some(r) => {
                            d.ui_show_window(r.x as i32, r.y as i32, r.width, r.height, false);
                        }
                        None => {
                            d.ui_hide(false);
                        }
                    }
                }
            } else if osd.tick(camera_waiting, plane) {
                plane.show();
            }
            let host = osd.host_mut();
            host.tick();
            if let (Some(input), Some(on)) = (&input, host.take_learning()) {
                input.set_learning(on);
            }
            if host.camera_key() != camera_key {
                camera_key = host.camera_key();
                if let Some(input) = &input {
                    input.set_camera_key(camera_key as u16);
                }
            }
            if let Some(action) = host.take_power() {
                log::info!("power: {action:?} requested; closing down first");
                exit = Exit::Power(action);
                break;
            }
        }
    }

    log::info!("stopping; {} input events were dropped", hub.dropped());
    drop(watcher);
    if let Some((osd, _)) = &mut screens
        && let Some(save) = osd.host_mut().camera_mut().and_then(Camera::stop)
    {
        osd.host_mut().persist_camera(save);
    }
    driver.stop(); // the phone is told goodbye, the decoder's pool freed
    drop(bt); // the agent and the profiles unregistered
    drop(input); // releases the grabbed keyboards
    match screens.take() {
        Some((mut osd, _)) => osd.host_mut().flush(), // the audio goes with it
        None => {
            audio.stop();
        }
    }
    drop(display); // releases the planes and DRM master last
    exit
}
