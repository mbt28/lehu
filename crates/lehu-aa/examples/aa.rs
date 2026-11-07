//! Wired Android Auto on the board without the rest of LEHU: switches the
//! phone that is plugged in, runs the session, logs what it reports and
//! counts the video and audio, until Ctrl-C. `aa` alone, or `aa chacha`
//! to force ChaCha20-Poly1305.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_int(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

use lehu_core::queue::Queue;
use lehu_session::{AudioFormat, AudioKind, Event, MicFormat, Phase, Ports, Session as _, Sinks};

struct Count {
    pcm: AtomicU32,
}

impl Sinks for Count {
    fn audio_pcm(&mut self, _: AudioKind, _: u32, _: u8, pcm: &[i16]) {
        self.pcm.fetch_add(pcm.len() as u32, Ordering::Relaxed);
    }
    fn audio_packet(&mut self, _: AudioKind, _: AudioFormat, _: &[u8]) {}
    fn mic(&mut self, wanted: Option<MicFormat>) {
        println!("mic: {wanted:?}");
    }
}

fn main() {
    let mut settings = lehu_core::settings::Settings::default();
    settings.system.log_level = lehu_core::settings::LogLevel::Debug;
    if std::env::args().any(|a| a == "chacha") {
        settings.android_auto.force_chacha20 = true;
    }
    lehu_core::log::init(settings.system.log_level);
    let config = lehu_aa::Config::from_settings(&settings);
    let wake = Arc::new(lehu_core::poll::Wake::new().expect("eventfd"));
    let video = Arc::new(Queue::new(8));
    let recycle = Arc::new(Queue::new(8));
    let ports = Ports::new(wake, Arc::clone(&video), Arc::clone(&recycle));
    let session: Arc<Mutex<Option<lehu_aa::Session>>> = Arc::new(Mutex::new(None));
    // SAFETY: the handler only stores a flag.
    unsafe {
        libc::signal(libc::SIGINT, on_int as *const () as usize);
    }
    let watcher = {
        let session = Arc::clone(&session);
        let ports = ports.clone();
        let config = config.clone();
        lehu_usb::Watcher::start(move |event| match event {
            lehu_usb::Event::Attached(d) => match d.role() {
                lehu_usb::Role::Other if config.accepts(d.id) => {
                    match lehu_aa::switch_to_accessory(&d) {
                        Ok(s) => println!("{d}: {s:?}"),
                        Err(e) => println!("{d}: {e}"),
                    }
                }
                lehu_usb::Role::Accessory => {
                    let sinks = Box::new(Count {
                        pcm: AtomicU32::new(0),
                    });
                    match lehu_aa::Session::start(config.clone(), &d, sinks, ports.clone()) {
                        Ok(s) => {
                            if let Some(old) = session.lock().unwrap().replace(s) {
                                println!("superseding the old session");
                                Box::new(old).stop();
                            }
                        }
                        Err(e) => println!("{d}: {e}"),
                    }
                }
                _ => {}
            },
            lehu_usb::Event::Detached(d) => println!("detached {d}"),
        })
        .expect("uevent socket")
    };
    let mut units = 0u32;
    let mut bytes = 0u64;
    while !STOP.load(Ordering::Relaxed) {
        while let Some(e) = ports.events.pop() {
            println!("event: {e:?}");
            if let Event::Phase(Phase::Closed) = e
                && let Some(s) = session.lock().unwrap().take()
            {
                Box::new(s).stop();
            }
        }
        while let Some(u) = video.pop_wait(Duration::from_millis(100)) {
            units += 1;
            bytes += u.len() as u64;
            if units.is_multiple_of(300) {
                println!("video: {units} units, {bytes} bytes");
            }
            let mut u = u;
            u.clear();
            recycle.push_discard(u);
        }
    }
    if let Some(s) = session.lock().unwrap().take() {
        Box::new(s).stop();
    }
    watcher.stop();
}
