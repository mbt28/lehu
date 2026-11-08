//! Settings: TOML, in three layers.
//!
//! 1. The defaults compiled in (`Settings::default()`, documented in
//!    `config/lehu.toml`, which a test keeps identical).
//! 2. The image's file, `/etc/lehu/lehu.toml`: what the board maker set.
//! 3. The user's file on the data partition: only the keys the screens
//!    changed, so an image update never loses them.
//!
//! Each layer is a TOML table merged over the one below (tables deep, arrays
//! whole), and the merged table is read into the typed `Settings`. A file
//! with a syntax error, an unknown key or a value of the wrong type is an
//! error that names the file, the key and, for syntax, the line. A value out
//! of range is clamped and reported, so a head unit still boots.
//!
//! The user's file is written atomically (a temporary file, fsync, rename).
//! When to write is the caller's business: the screens change settings in
//! bursts, so the main loop saves a couple of seconds after the last change.

pub mod import;

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

// ── Schema ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Settings {
    pub display: Display,
    pub touch: Touch,
    pub carplay: CarPlay,
    pub android_auto: AndroidAuto,
    pub wifi: Wifi,
    pub bluetooth: Bluetooth,
    pub audio: Audio,
    pub usb: Usb,
    pub decode: Decode,
    pub camera: Camera,
    pub update: Update,
    pub system: System,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NightMode {
    Day,
    Night,
    Auto,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoPath {
    Auto,
    Hw,
    Headless,
}

/// The video codec asked of the phones. `Auto` takes what the system
/// decodes in hardware; a forced codec it cannot decode is refused at
/// start and `Auto` used instead.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoCodec {
    Auto,
    H264,
    Hevc,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Display {
    /// The picture asked of the phone; absent, the panel's own mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub fps: u32,
    pub video_path: VideoPath,
    pub codec: VideoCodec,
    pub night_mode: NightMode,
    pub left_hand_drive: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Touch {
    /// Empty: the first evdev node with absolute axes.
    pub device: String,
    pub swap_xy: bool,
    pub invert_x: bool,
    pub invert_y: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct CarPlay {
    pub mfi_i2c_bus: String,
    pub mfi_i2c_addr: u32,
    /// Offer CarPlay Ultra to a phone that runs it whenever the MFi chip
    /// allows (a 3.0): its mutual authentication, the instrument panel and
    /// the vehicle state. Another chip, or off: classic CarPlay.
    pub ultra: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct AndroidAuto {
    /// 1 = 800x480, 2 = 1280x720, 3 = 1920x1080.
    pub resolution: u32,
    pub fps: u32,
    pub dpi: u32,
    /// 0 = any phone that answers AOAP; set both to pin one.
    pub vendor_id: u32,
    pub product_id: u32,
    pub accessory_timeout_ms: u32,
    pub max_unacked: u32,
    /// Offer only ChaCha20-Poly1305 for the TLS session: far cheaper than
    /// AES-GCM on a CPU without AES instructions.
    pub force_chacha20: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Wifi {
    pub interface: String,
    /// The access point belongs to the system; ssid, passphrase and channel
    /// mirror its hostapd.conf, and the screens write changes back into it.
    pub hostapd_conf: String,
    pub ap_restart_command: String,
    pub ssid: String,
    pub passphrase: String,
    pub channel: u32,
    pub ap_ip: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Bluetooth {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Audio {
    /// The ALSA PCM opened directly, no plugin layer.
    pub device: String,
    /// 0..=100; absent until first set, when the card's level is taken over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u32>,
    /// "auto", "software", or an ALSA simple-mixer control name.
    pub volume_control: String,
    /// Empty: the card of `device`.
    pub volume_device: String,
    pub volume_step: u32,
    /// Music held before it plays, when it starts and after it runs dry.
    pub buffer_ms: u32,
    /// Voice segments held before a call's audio starts.
    pub call_prefill_segments: u32,
    /// Seconds of fade after a stream starts or ducks.
    pub fade: f32,
    /// Voice older than this is dropped rather than played late.
    pub voice_max_latency_ms: u32,
    /// The PCM is released after this much silence, powering the DAC down.
    pub idle_hold_ms: u32,
    /// Integer gain on the captured microphone, 1 = unchanged.
    pub mic_gain: u32,
    /// The capture PCM; empty: `device`.
    pub mic_device: String,
    /// Each stream's level under the volume, 0..=100 percent: music,
    /// navigation and the assistant's voice, alerts and chimes, calls.
    pub music_level: u32,
    pub navigation_level: u32,
    pub alert_level: u32,
    pub call_level: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Usb {
    pub async_calls: u32,
    pub transfer_size: u32,
    pub buffer: u32,
    pub write_timeout_ms: u32,
    pub write_retries: u32,
    pub write_retry_delay_ms: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Decode {
    pub low_delay: bool,
    pub fast: bool,
    pub rendering_buffer: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Standard {
    Auto,
    Pal,
    Ntsc,
}

/// One camera input's picture, as the camera view saves it. A level left
/// out is the driver's default for the standard.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct CameraInput {
    pub standard: Standard,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hue: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpness: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma_gain: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma_enhance: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Camera {
    /// Empty: the first V4L2 capture device found.
    pub device: String,
    /// The input last shown.
    pub input: u32,
    /// A keyboard key (Linux key code, 0 = none) that switches to the camera
    /// and back.
    pub key: u32,
    /// Keep the phone's decoder running while the camera shows, when the
    /// video memory holds both.
    pub fast_switch: bool,
    /// One entry per input; the array is replaced whole by a higher layer.
    pub inputs: Vec<CameraInput>,
}

/// Settings > Update: LEHU OS updates, carried by the phone's browser from
/// the update server to a page the head unit serves on its access point.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Update {
    /// The update server's manifest.json, as the phone fetches it; the
    /// updates' files are named relative to it. Empty: no Update row.
    pub manifest_url: String,
    /// The TCP port of the page on the access point's address.
    pub port: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Verbose,
    Protocol,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct System {
    pub log_level: LogLevel,
    /// The screens on the overlay plane while no video plays.
    pub ui: bool,
    /// Real-time priority for the USB read, audio and decoder threads.
    pub realtime_priority: bool,
    pub on_connect_script: String,
    pub on_disconnect_script: String,
    /// What Reboot and Power off run once LEHU has closed down.
    pub reboot_command: String,
    pub shutdown_command: String,
    /// Run with the phone's time when the system clock is far off it;
    /// `{unix}` stands for the seconds since the epoch. Empty: the clock
    /// is left alone.
    pub set_clock_command: String,
}

// ── Defaults ──────────────────────────────────────────────────────────────

impl Default for Display {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            fps: 60,
            video_path: VideoPath::Auto,
            codec: VideoCodec::Auto,
            night_mode: NightMode::Auto,
            left_hand_drive: true,
        }
    }
}

impl Default for CarPlay {
    fn default() -> Self {
        Self {
            mfi_i2c_bus: "auto".into(),
            mfi_i2c_addr: 0x10,
            ultra: true,
        }
    }
}

impl Default for AndroidAuto {
    fn default() -> Self {
        Self {
            resolution: 1,
            fps: 30,
            dpi: 120,
            vendor_id: 0,
            product_id: 0,
            accessory_timeout_ms: 5000,
            max_unacked: 16,
            force_chacha20: true,
        }
    }
}

impl Default for Wifi {
    fn default() -> Self {
        Self {
            interface: "wlan0".into(),
            hostapd_conf: "/etc/hostapd.conf".into(),
            ap_restart_command: "/etc/init.d/S44ap stop; /etc/init.d/S44ap start".into(),
            ssid: "LEHU".into(),
            passphrase: "lehu1234".into(),
            channel: 6,
            ap_ip: "192.168.53.1".into(),
        }
    }
}

impl Default for Bluetooth {
    fn default() -> Self {
        Self {
            name: "LEHU".into(),
        }
    }
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            device: "hw:0,0".into(),
            volume: None,
            volume_control: "auto".into(),
            volume_device: String::new(),
            volume_step: 5,
            buffer_ms: 500,
            call_prefill_segments: 6,
            fade: 0.3,
            voice_max_latency_ms: 300,
            idle_hold_ms: 1500,
            mic_gain: 1,
            mic_device: String::new(),
            music_level: 100,
            navigation_level: 100,
            alert_level: 100,
            call_level: 100,
        }
    }
}

impl Default for Usb {
    fn default() -> Self {
        Self {
            async_calls: 32,
            transfer_size: 2048,
            buffer: 128,
            write_timeout_ms: 300,
            write_retries: 6,
            write_retry_delay_ms: 2,
        }
    }
}

impl Default for Decode {
    fn default() -> Self {
        Self {
            low_delay: true,
            fast: true,
            rendering_buffer: 5,
        }
    }
}

impl Default for CameraInput {
    fn default() -> Self {
        Self {
            standard: Standard::Auto,
            brightness: None,
            contrast: None,
            saturation: None,
            hue: None,
            sharpness: None,
            chroma_gain: None,
            chroma_enhance: None,
        }
    }
}

impl Default for Update {
    fn default() -> Self {
        Self {
            manifest_url: "https://mbt28.github.io/lehu/os/manifest.json".into(),
            port: 8080,
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            ui: true,
            realtime_priority: true,
            on_connect_script: String::new(),
            on_disconnect_script: String::new(),
            reboot_command: "reboot".into(),
            shutdown_command: "poweroff".into(),
            set_clock_command: "date -u -s @{unix}".into(),
        }
    }
}

// ── Range checks ──────────────────────────────────────────────────────────

/// The limits the screens and the mixer work within.
pub const BUFFER_MAX_MS: u32 = 1500;
pub const VOLUME_MAX: u32 = 100;

impl Settings {
    /// Clamp what is out of range and say what was clamped, one line each.
    pub fn normalize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let mut clamp = |name: &str, value: &mut u32, lo: u32, hi: u32| {
            if *value < lo || *value > hi {
                notes.push(format!(
                    "{name} = {value} is outside {lo}..={hi}, using {}",
                    (*value).clamp(lo, hi)
                ));
                *value = (*value).clamp(lo, hi);
            }
        };
        clamp(
            "audio.buffer-ms",
            &mut self.audio.buffer_ms,
            0,
            BUFFER_MAX_MS,
        );
        if let Some(v) = self.audio.volume.as_mut() {
            clamp("audio.volume", v, 0, VOLUME_MAX);
        }
        clamp("audio.volume-step", &mut self.audio.volume_step, 1, 50);
        clamp("audio.mic-gain", &mut self.audio.mic_gain, 1, 16);
        clamp("audio.music-level", &mut self.audio.music_level, 0, 100);
        clamp(
            "audio.navigation-level",
            &mut self.audio.navigation_level,
            0,
            100,
        );
        clamp("audio.alert-level", &mut self.audio.alert_level, 0, 100);
        clamp("audio.call-level", &mut self.audio.call_level, 0, 100);
        clamp(
            "android-auto.resolution",
            &mut self.android_auto.resolution,
            1,
            3,
        );
        clamp("android-auto.fps", &mut self.android_auto.fps, 30, 60);
        clamp("display.fps", &mut self.display.fps, 24, 60);
        clamp("camera.input", &mut self.camera.input, 0, 3);
        clamp("update.port", &mut self.update.port, 1, 65535);
        if !(0.0..=10.0).contains(&self.audio.fade) {
            notes.push(format!(
                "audio.fade = {} is outside 0..=10, using 0.3",
                self.audio.fade
            ));
            self.audio.fade = 0.3;
        }
        let ssid = self.wifi.ssid.len();
        if ssid == 0 || ssid > 32 {
            notes.push(format!(
                "wifi.ssid is {ssid} bytes, not 1..=32, using the default"
            ));
            self.wifi.ssid = Wifi::default().ssid;
        }
        let pass = self.wifi.passphrase.len();
        if !(8..=63).contains(&pass) {
            notes.push(format!(
                "wifi.passphrase is {pass} bytes, not 8..=63, using the default"
            ));
            self.wifi.passphrase = Wifi::default().passphrase;
        }
        if self.camera.inputs.len() > 4 {
            notes.push(format!(
                "camera.inputs has {} entries, keeping 4",
                self.camera.inputs.len()
            ));
            self.camera.inputs.truncate(4);
        }
        notes
    }

    /// The defaults as a TOML table: the base every layer merges over.
    /// Optional settings that default to "absent" are not in it.
    pub fn defaults_table() -> Table {
        Table::try_from(Settings::default()).expect("the defaults serialize")
    }

    /// Every key there is, as a TOML table: the shape unknown keys are
    /// checked against. The optional settings are present with placeholder
    /// values, which is what tells this apart from `defaults_table`.
    pub fn schema_table() -> Table {
        let mut all = Settings::default();
        all.display.width = Some(0);
        all.display.height = Some(0);
        all.audio.volume = Some(0);
        Table::try_from(all).expect("the schema serializes")
    }
}

// ── Errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Error {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Syntax {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    UnknownKey {
        path: PathBuf,
        key: String,
    },
    Invalid {
        message: String,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Read { path, source } => write!(f, "{}: cannot read: {source}", path.display()),
            Error::Syntax {
                path,
                line,
                column,
                message,
            } => {
                write!(f, "{}:{line}:{column}: {message}", path.display())
            }
            Error::UnknownKey { path, key } => write!(f, "{}: unknown key `{key}`", path.display()),
            Error::Invalid { message } => write!(f, "settings: {message}"),
            Error::Write { path, source } => {
                write!(f, "{}: cannot write: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {}

// ── Loading ───────────────────────────────────────────────────────────────

/// Where the two files live. Both may be missing.
#[derive(Debug, Clone)]
pub struct Paths {
    pub image: PathBuf,
    pub user: PathBuf,
}

impl Paths {
    /// `/etc/lehu/lehu.toml` and the user's file (`user_default`), or the
    /// `LEHU_CONFIG` and `LEHU_USER_CONFIG` environment variables.
    pub fn system() -> Self {
        Self {
            image: std::env::var_os("LEHU_CONFIG")
                .map_or_else(|| PathBuf::from("/etc/lehu/lehu.toml"), PathBuf::from),
            user: std::env::var_os("LEHU_USER_CONFIG").map_or_else(user_default, PathBuf::from),
        }
    }
}

/// `/var/lib/lehu/settings.toml` for root and for a user who may write
/// there (the image's `lehu`); any other user keeps it in `~/.lehu`. The
/// protocols' state (pairings, the device registry) lives beside it.
fn user_default() -> PathBuf {
    let system = Path::new("/var/lib/lehu");
    match home_dir() {
        Some(home) if !crate::shell::is_root() && !writable(system) => {
            home.join(".lehu").join("settings.toml")
        }
        _ => system.join("settings.toml"),
    }
}

/// The home of the user this process runs as, from the password database:
/// `HOME` may still be the caller's (`start-stop-daemon -c`, `su` without
/// `-`). `HOME` only when the database has no entry.
fn home_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 1024];
    // SAFETY: passwd is plain data; getpwuid_r writes it and the strings it
    // points to into `buf`, which outlives every read of them below.
    let found = unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            libc::geteuid(),
            &mut pw,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut out,
        );
        if rc == 0 && !out.is_null() && !pw.pw_dir.is_null() {
            Some(std::ffi::CStr::from_ptr(pw.pw_dir).to_bytes().to_vec())
        } else {
            None
        }
    };
    found
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(std::ffi::OsStr::from_bytes(&d)))
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
        })
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access only reads the NUL-terminated path.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// The result of a load: the settings in force, the user's file to write
/// changes into, and what was clamped on the way.
#[derive(Debug)]
pub struct Loaded {
    pub settings: Settings,
    pub user: UserFile,
    pub notes: Vec<String>,
    /// The image's layer, kept so a value the user changes can be folded
    /// back into the settings in force without reading the files again.
    pub image: Table,
}

pub fn load(paths: &Paths) -> Result<Loaded, Error> {
    let schema = Settings::schema_table();
    let image_table = read_layer(&paths.image, &schema)?;
    let user_table = read_layer(&paths.user, &schema)?.unwrap_or_default();
    let image = image_table.unwrap_or_default();
    let mut merged = Settings::defaults_table();
    merge(&mut merged, image.clone());
    merge(&mut merged, user_table.clone());
    let mut settings: Settings =
        Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| Error::Invalid {
                message: e.message().to_string(),
            })?;
    let notes = settings.normalize();
    Ok(Loaded {
        settings,
        user: UserFile {
            path: paths.user.clone(),
            table: user_table,
            dirty: false,
        },
        notes,
        image,
    })
}

/// The settings in force after the user's table changed: the defaults, the
/// image's layer and the user's table merged again and normalized. The notes
/// are not repeated; `load` reported them once.
pub fn rebuild(image: &Table, user: &Table) -> Result<Settings, Error> {
    let mut merged = Settings::defaults_table();
    merge(&mut merged, image.clone());
    merge(&mut merged, user.clone());
    let mut settings: Settings =
        Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| Error::Invalid {
                message: e.message().to_string(),
            })?;
    settings.normalize();
    Ok(settings)
}

/// One file as a table, checked for syntax and for keys the schema does not
/// have. `None` when the file does not exist.
fn read_layer(path: &Path, schema: &Table) -> Result<Option<Table>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Read {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let table = parse(&text, path)?;
    if let Some(key) = unknown_key(&table, schema, "") {
        return Err(Error::UnknownKey {
            path: path.to_path_buf(),
            key,
        });
    }
    Ok(Some(table))
}

/// TOML text as a table; a syntax error carries its line and column.
pub fn parse(text: &str, path: &Path) -> Result<Table, Error> {
    toml::from_str::<Table>(text).map_err(|e| {
        let (line, column) = e
            .span()
            .map(|span| line_column(text, span.start))
            .unwrap_or((0, 0));
        Error::Syntax {
            path: path.to_path_buf(),
            line,
            column,
            message: e.message().to_string(),
        }
    })
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let head = &text[..offset.min(text.len())];
    let line = head.matches('\n').count() + 1;
    let column = head.rfind('\n').map_or(offset, |nl| offset - nl - 1) + 1;
    (line, column)
}

/// The first key in `table` that the schema does not have, as `a.b.c`.
/// Arrays are not entered: their elements are checked by the typed read.
fn unknown_key(table: &Table, schema: &Table, prefix: &str) -> Option<String> {
    for (key, value) in table {
        let full = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match schema.get(key) {
            None => return Some(full),
            Some(Value::Table(d)) => {
                if let Value::Table(t) = value
                    && let Some(k) = unknown_key(t, d, &full)
                {
                    return Some(k);
                }
            }
            Some(_) => {}
        }
    }
    None
}

/// `over` onto `base`: tables recurse, everything else replaces.
pub fn merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        if let Value::Table(o) = value {
            if let Some(Value::Table(b)) = base.get_mut(&key) {
                merge(b, o);
                continue;
            }
            base.insert(key, Value::Table(o));
        } else {
            base.insert(key, value);
        }
    }
}

// ── The user's file ───────────────────────────────────────────────────────

/// The keys the user changed, and nothing else. Set what changed, then save
/// when the burst of changes is over.
#[derive(Debug)]
pub struct UserFile {
    path: PathBuf,
    table: Table,
    dirty: bool,
}

impl UserFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// `key` is dotted, `audio.buffer-ms`; the value is any serializable
    /// setting, a whole `Vec<CameraInput>` included.
    pub fn set<T: Serialize>(&mut self, key: &str, value: T) -> Result<(), Error> {
        let value = Value::try_from(value).map_err(|e| Error::Invalid {
            message: format!("{key}: {e}"),
        })?;
        let mut parts = key.split('.').peekable();
        let mut table = &mut self.table;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                if table.get(part) != Some(&value) {
                    table.insert(part.to_string(), value);
                    self.dirty = true;
                }
                return Ok(());
            }
            let entry = table
                .entry(part.to_string())
                .or_insert_with(|| Value::Table(Table::new()));
            if !entry.is_table() {
                *entry = Value::Table(Table::new());
            }
            table = entry.as_table_mut().expect("just made a table");
        }
        Ok(())
    }

    /// Back to the layer below for this key. True when something was removed.
    pub fn remove(&mut self, key: &str) -> bool {
        let Some((head, last)) = key.rsplit_once('.') else {
            let removed = self.table.remove(key).is_some();
            self.dirty |= removed;
            return removed;
        };
        let mut table = &mut self.table;
        for part in head.split('.') {
            match table.get_mut(part) {
                Some(Value::Table(t)) => table = t,
                _ => return false,
            }
        }
        let removed = table.remove(last).is_some();
        self.dirty |= removed;
        removed
    }

    /// Made, holding no keys yet, on the first run, so the place the
    /// screens' changes go exists from the start. True when it was made.
    pub fn create_if_missing(&mut self) -> Result<bool, Error> {
        if self.path.exists() {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// Written atomically: a temporary file beside it, fsync, rename. A
    /// missing directory is made private (0700): the pairings live there.
    pub fn save(&mut self) -> Result<(), Error> {
        let write = |path: &Path| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            let tmp = path.with_extension("toml.tmp");
            let text = toml::to_string_pretty(&self.table)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            {
                let mut f = fs::File::create(&tmp)?;
                f.write_all(b"# LEHU user settings: the keys changed on the head unit.\n")?;
                f.write_all(text.as_bytes())?;
                f.sync_all()?;
            }
            fs::rename(&tmp, path)
        };
        write(&self.path).map_err(|source| Error::Write {
            path: self.path.clone(),
            source,
        })?;
        self.dirty = false;
        Ok(())
    }

    pub fn save_if_dirty(&mut self) -> Result<bool, Error> {
        if !self.dirty {
            return Ok(false);
        }
        self.save().map(|()| true)
    }
}

/// The settings in force, as TOML text (for `lehu --print-config`).
pub fn to_toml(settings: &Settings) -> String {
    toml::to_string_pretty(settings).expect("settings serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lehu-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let table = Settings::defaults_table();
        let back: Settings = Value::Table(table).try_into().unwrap();
        assert_eq!(back, Settings::default());
    }

    #[test]
    fn the_codec_names() {
        for (name, codec) in [
            ("auto", VideoCodec::Auto),
            ("h264", VideoCodec::H264),
            ("hevc", VideoCodec::Hevc),
        ] {
            let s: Settings = toml::from_str(&format!("[display]\ncodec = \"{name}\"")).unwrap();
            assert_eq!(s.display.codec, codec);
        }
        assert!(toml::from_str::<Settings>("[display]\ncodec = \"vp9\"").is_err());
    }

    #[test]
    fn the_shipped_file_is_the_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/lehu.toml");
        let text = fs::read_to_string(&path).unwrap();
        let table = parse(&text, &path).unwrap();
        assert_eq!(unknown_key(&table, &Settings::schema_table(), ""), None);
        let mut merged = Settings::defaults_table();
        merge(&mut merged, table);
        let shipped: Settings = Value::Table(merged).try_into().unwrap();
        assert_eq!(
            shipped,
            Settings::default(),
            "config/lehu.toml differs from Settings::default()"
        );
    }

    #[test]
    fn carplay_ultra_is_on_unless_turned_off() {
        assert!(Settings::default().carplay.ultra);
        let d = dir("ultra");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("user.toml"),
        };
        fs::write(&paths.image, "[carplay]\nultra = true\n").unwrap();
        let loaded = load(&paths).unwrap();
        assert!(loaded.settings.carplay.ultra);
        assert_eq!(
            loaded.settings.carplay.mfi_i2c_bus, "auto",
            "the rest stays"
        );
        fs::write(&paths.user, "[carplay]\nultra = false\n").unwrap();
        assert!(
            !load(&paths).unwrap().settings.carplay.ultra,
            "the user's word last"
        );
        fs::write(&paths.user, "[carplay]\nultra = \"yes\"\n").unwrap();
        assert!(matches!(load(&paths), Err(Error::Invalid { .. })));
    }

    #[test]
    fn layers_merge_in_order() {
        let d = dir("layers");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("user.toml"),
        };
        fs::write(
            &paths.image,
            "[display]\nwidth = 800\nheight = 480\n[audio]\nbuffer-ms = 300\n",
        )
        .unwrap();
        fs::write(&paths.user, "[audio]\nbuffer-ms = 800\nvolume = 70\n").unwrap();
        let loaded = load(&paths).unwrap();
        assert_eq!(loaded.settings.display.width, Some(800));
        assert_eq!(loaded.settings.audio.buffer_ms, 800);
        assert_eq!(loaded.settings.audio.volume, Some(70));
        assert_eq!(loaded.settings.audio.volume_step, 5);
        assert!(loaded.notes.is_empty());
    }

    #[test]
    fn missing_files_are_the_defaults() {
        let d = dir("missing");
        let loaded = load(&Paths {
            image: d.join("none.toml"),
            user: d.join("none2.toml"),
        })
        .unwrap();
        assert_eq!(loaded.settings, Settings::default());
    }

    #[test]
    fn unknown_keys_are_named() {
        let d = dir("unknown");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("user.toml"),
        };
        fs::write(&paths.image, "[audio]\nbuffer_ms = 300\n").unwrap();
        match load(&paths) {
            Err(Error::UnknownKey { key, .. }) => assert_eq!(key, "audio.buffer_ms"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn syntax_errors_carry_the_line() {
        let d = dir("syntax");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("user.toml"),
        };
        fs::write(&paths.image, "[audio]\nbuffer-ms = 300\nvolume = \n").unwrap();
        match load(&paths) {
            Err(Error::Syntax { line, .. }) => assert_eq!(line, 3),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wrong_types_are_errors_and_ranges_are_clamped() {
        let d = dir("types");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("user.toml"),
        };
        fs::write(&paths.image, "[audio]\nbuffer-ms = \"loud\"\n").unwrap();
        assert!(matches!(load(&paths), Err(Error::Invalid { .. })));
        fs::write(
            &paths.image,
            "[audio]\nbuffer-ms = 9000\n[wifi]\npassphrase = \"short\"\n",
        )
        .unwrap();
        let loaded = load(&paths).unwrap();
        assert_eq!(loaded.settings.audio.buffer_ms, BUFFER_MAX_MS);
        assert_eq!(loaded.settings.wifi.passphrase, "lehu1234");
        assert_eq!(loaded.notes.len(), 2);
    }

    #[test]
    fn stream_levels_and_the_devices_reload_and_clamp() {
        let d = dir("streams");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("user.toml"),
        };
        fs::write(&paths.image, "[audio]\nalert-level = 140\n").unwrap();
        let mut loaded = load(&paths).unwrap();
        assert_eq!(loaded.settings.audio.alert_level, 100);
        assert_eq!(loaded.notes.len(), 1);
        assert_eq!(loaded.settings.audio.mic_device, "");
        loaded.user.set("audio.music-level", 70u32).unwrap();
        loaded.user.set("audio.call-level", 0u32).unwrap();
        loaded
            .user
            .set("audio.device", "hw:CARD=Device,DEV=0")
            .unwrap();
        loaded.user.set("audio.mic-device", "hw:0,0").unwrap();
        loaded.user.save().unwrap();
        let again = load(&paths).unwrap();
        let audio = &again.settings.audio;
        assert_eq!((audio.music_level, audio.call_level), (70, 0));
        assert_eq!((audio.navigation_level, audio.alert_level), (100, 100));
        assert_eq!(audio.device, "hw:CARD=Device,DEV=0");
        assert_eq!(audio.mic_device, "hw:0,0");
        let rebuilt = rebuild(&again.image, again.user.table()).unwrap();
        assert_eq!(rebuilt, again.settings);
    }

    #[test]
    fn user_file_sets_saves_and_reloads() {
        let d = dir("user");
        let paths = Paths {
            image: d.join("lehu.toml"),
            user: d.join("state/user.toml"),
        };
        let mut loaded = load(&paths).unwrap();
        assert!(!loaded.user.is_dirty());
        loaded.user.set("audio.buffer-ms", 750u32).unwrap();
        loaded
            .user
            .set("display.night-mode", NightMode::Night)
            .unwrap();
        loaded
            .user
            .set(
                "camera.inputs",
                vec![CameraInput {
                    standard: Standard::Pal,
                    brightness: Some(40),
                    ..Default::default()
                }],
            )
            .unwrap();
        assert!(loaded.user.is_dirty());
        assert!(loaded.user.save_if_dirty().unwrap());
        assert!(!loaded.user.save_if_dirty().unwrap());
        let again = load(&paths).unwrap();
        assert_eq!(again.settings.audio.buffer_ms, 750);
        assert_eq!(again.settings.display.night_mode, NightMode::Night);
        assert_eq!(again.settings.camera.inputs[0].brightness, Some(40));
        assert_eq!(again.settings.camera.inputs[0].contrast, None);
        // Setting the same value again is not a change.
        let mut user = again.user;
        user.set("audio.buffer-ms", 750u32).unwrap();
        assert!(!user.is_dirty());
        assert!(user.remove("audio.buffer-ms"));
        assert!(!user.remove("audio.buffer-ms"));
        user.save().unwrap();
        assert_eq!(load(&paths).unwrap().settings.audio.buffer_ms, 500);
    }
}
