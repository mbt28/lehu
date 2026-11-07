//! CarPlay Ultra on a head unit with one screen and no car behind it.
//!
//! What makes an iPhone run an Ultra session rather than classic CarPlay:
//!
//! 1. The CarPlayStartSession over iAP2 says the head unit supports mutual
//!    authentication (only with an MFi 3.0 coprocessor). The phone then
//!    authenticates on `/auth-setup` with MFi 4 (`x-apple-at: 0x20`,
//!    `mfi4`), runs the pair-setup through it and binds the pair-verify.
//! 2. `/info` lists `displayPanels`, and one of them carries a display
//!    plug-in: that is what makes the phone count the head unit as Ultra.
//!    A panel with a plug-in gets no picture of its own (a screen showing
//!    one turns black), so the plug-in sits on an extra panel that shows
//!    instruments and has no video stream: no second stream, no second
//!    decoder. The main screen's panel carries its stream and no plug-in.
//! 3. `/info` describes the vehicle state (`vehicle`), and the session
//!    SETUP enables `vehicleStateProtocol` when the phone offers it; the
//!    phone then opens its two data streams for it.
//! 4. Optionally a car-maker theme: named in the identification and the
//!    start, the main display named after the theme's centre display, and
//!    the UI sync channel (`ui_sync`) enabled. The phone fetches the
//!    theme's artwork itself; the head unit only needs the theme's ids,
//!    provisioned offline in a small file (no network on the board).
//!
//! Without a theme the phone runs Ultra in its own look.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use crate::av::stream_key;
use crate::cipher::FrameCipher;
use crate::plist::Value;
use crate::ui_sync::{self, Layout};
use crate::vehicle;

/// The theme file, in the CarPlay state directory.
pub const THEME_FILE: &str = "ultra-theme.txt";
/// The panel that carries the display plug-in, sized like the screen.
pub const INSTRUMENT_PANEL: &str = "b7e6c5a0-3333-4000-8000-000000000003";
/// The iAP2 tunnel's data stream, the one the session always had.
pub const IAP_DATASTREAM_UUID: &str = "E9459FD0-BCAD-4C45-820F-1E72447EF2F2";

const MAX_ID: usize = 64;

/// A car-maker theme from Apple's asset catalog, by its ids: the asset,
/// its version and the SDK it is built for go to the phone (which loads
/// the artwork), the centre display's id names our screen, a layout of
/// the theme's cluster is asked for over the UI sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub asset: String,
    pub version: u32,
    pub sdk: String,
    pub center: String,
    pub layout: Option<Layout>,
}

fn plain_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= MAX_ID
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Theme {
    /// `key=value` lines, `#` comments: asset, version, sdk, center, and
    /// optionally layout and palette.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut asset, mut version, mut sdk, mut center, mut layout, mut palette) =
            (None, None, None, None, None, None);
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {}: no key=value", n + 1));
            };
            let value = value.trim();
            if !plain_id(value) {
                return Err(format!("line {}: {value:?} is not a plain id", n + 1));
            }
            let slot = match key.trim() {
                "asset" => &mut asset,
                "version" => &mut version,
                "sdk" => &mut sdk,
                "center" => &mut center,
                "layout" => &mut layout,
                "palette" => &mut palette,
                other => return Err(format!("line {}: unknown key {other:?}", n + 1)),
            };
            *slot = Some(value.to_string());
        }
        let need = |v: Option<String>, what: &str| v.ok_or_else(|| format!("no {what}"));
        let version = need(version, "version")?;
        Ok(Self {
            asset: need(asset, "asset")?,
            version: version
                .parse()
                .map_err(|_| format!("version {version:?} is not a number"))?,
            sdk: need(sdk, "sdk")?,
            center: need(center, "center")?,
            layout: layout.map(|id| Layout {
                id,
                palette: palette.unwrap_or_default(),
            }),
        })
    }

    /// The theme provisioned in `dir`, if there is one and it reads.
    pub fn load(dir: &Path) -> Option<Self> {
        let file = dir.join(THEME_FILE);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("carplay: ultra: {}: {e}", file.display());
                return None;
            }
        };
        match Self::parse(&text) {
            Ok(theme) => {
                log::info!(
                    "carplay: ultra: theme {}:{}:{} on display {}",
                    theme.asset,
                    theme.version,
                    theme.sdk,
                    theme.center
                );
                Some(theme)
            }
            Err(e) => {
                log::warn!("carplay: ultra: {}: {e}; no theme", file.display());
                None
            }
        }
    }
}

/// What the session SETUP takes of the features the phone offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Enabled {
    pub vehicle_state: bool,
    pub ui_sync: bool,
}

impl Enabled {
    pub fn from_offer(offered: &[&str], themed: bool) -> Self {
        Self {
            vehicle_state: offered.contains(&vehicle::FEATURE),
            ui_sync: themed && offered.contains(&ui_sync::FEATURE),
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        [
            (self.vehicle_state, vehicle::FEATURE),
            (self.ui_sync, ui_sync::FEATURE),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
    }
}

/// What `/info` says beyond classic CarPlay: the display's Ultra
/// properties, the panels, the vehicle state, and the UI sync once it is
/// enabled. With a theme the screen's panel is named as the theme's
/// display is.
pub fn add_info(info: &mut Value, vehicle: &vehicle::VehicleState, themed: bool, ui_sync_on: bool) {
    let mut panels = Vec::new();
    if let Some(Value::Array(displays)) = info.get("displays").cloned() {
        let mut patched = Vec::with_capacity(displays.len());
        for mut d in displays {
            let ui = d.int_or("uiAppearanceMode", 0);
            let setting = d.int_or("uiAppearanceSetting", 0);
            d.set("cornerMasks", Value::Bool(false));
            d.set("adjacentViewAreas", Value::Array(Vec::new()));
            d.set("mapAppearanceMode", Value::Int(ui));
            d.set("mapAppearanceSetting", Value::Int(setting));
            if let Some(Value::Array(areas)) = d.get("viewAreas").cloned() {
                let areas = areas
                    .into_iter()
                    .map(|a| {
                        a.with("viewAreaTransitionControl", Value::Bool(false))
                            .with("viewAreaSupportsFocusTransfer", Value::Bool(false))
                    })
                    .collect();
                d.set("viewAreas", Value::Array(areas));
            }
            if let Some(uuid) = d.get("uuid").and_then(Value::as_str) {
                let uid = if themed {
                    uuid.to_string()
                } else {
                    format!("{uuid}-panel")
                };
                panels.push(panel(&d, &uid, Some(uuid), false));
            }
            patched.push(d);
        }
        if let Some(first) = patched.first() {
            panels.push(panel(
                first,
                &format!("{INSTRUMENT_PANEL}-panel"),
                None,
                true,
            ));
        }
        info.set("displays", Value::Array(patched));
    }
    info.set("displayPanels", Value::Array(panels));
    info.set(vehicle::INFO_KEY, vehicle.info_entry());
    if ui_sync_on {
        info.set(ui_sync::INFO_KEY, Value::dict());
    }
}

/// A panel the size of display `d`: with the display's stream, or empty
/// with the plug-in that shows the instruments.
fn panel(d: &Value, uid: &str, stream: Option<&str>, instruments: bool) -> Value {
    let size = |k: &str| d.get(k).cloned().unwrap_or(Value::Int(0));
    let streams = stream
        .map(|uuid| {
            Value::dict()
                .with("uuid", Value::str(uuid))
                .with("originXPixels", Value::Int(0))
                .with("originYPixels", Value::Int(0))
                .with("zIndex", Value::Int(0))
        })
        .into_iter()
        .collect();
    let properties = if instruments {
        vec![Value::str("showsInstruments")]
    } else {
        Vec::new()
    };
    let mut p = Value::dict()
        .with("uid", Value::str(uid))
        .with("widthPixels", size("widthPixels"))
        .with("heightPixels", size("heightPixels"))
        .with("widthPhysical", size("widthPhysical"))
        .with("heightPhysical", size("heightPhysical"))
        .with("properties", Value::Array(properties))
        .with("initialVideoStreams", Value::Array(streams));
    if instruments {
        let plugin = Value::dict()
            .with("uid", Value::str(&format!("{INSTRUMENT_PANEL}-plugin")))
            .with("videoStreams", Value::Array(Vec::new()));
        p.set("displayPlugins", Value::Array(vec![plugin]));
    }
    p
}

/// Which Ultra channel a data stream's client type names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    VehicleState { high_priority: bool },
    UiSync,
}

impl Channel {
    pub fn of(client_type: &str, enabled: Enabled) -> Option<Self> {
        let uuid = client_type.to_ascii_uppercase();
        match uuid.as_str() {
            vehicle::HIGH_PRIORITY_DATASTREAM_UUID if enabled.vehicle_state => {
                Some(Self::VehicleState {
                    high_priority: true,
                })
            }
            vehicle::DATASTREAM_UUID if enabled.vehicle_state => Some(Self::VehicleState {
                high_priority: false,
            }),
            ui_sync::DATASTREAM_UUID if enabled.ui_sync => Some(Self::UiSync),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::VehicleState {
                high_priority: true,
            } => "vehicle state (high priority)",
            Self::VehicleState { .. } => "vehicle state",
            Self::UiSync => "ui sync",
        }
    }
}

// ── The data streams' framing ─────────────────────────────────────────────

const PKG_HEADER: usize = 32;
const MAX_PACKAGE: usize = 256 * 1024;
const PACKAGE_ASYNC: u32 = 0x6173_796e;
const PACKAGE_SYNC: u32 = 0x7379_6e63;
const PACKAGE_REPLY: u32 = 0x7270_6c79;
const MSG_TYPE_COMMAND: u32 = 0x636d_6e64;

/// One package of a data stream: its kind (async, sync, reply), the reply
/// token a sync one wants back, and the message.
#[derive(Debug, PartialEq, Eq)]
pub struct Package {
    pub kind: u32,
    pub reply_token: u64,
    pub body: Vec<u8>,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The next whole package off the front of `plain`; `Err` with the size
/// when it cannot be one.
pub fn next_package(plain: &mut Vec<u8>) -> Result<Option<Package>, usize> {
    let Some(size) = be32(plain, 0) else {
        return Ok(None);
    };
    let size = size as usize;
    if !(PKG_HEADER..=MAX_PACKAGE).contains(&size) {
        return Err(size);
    }
    let (Some(kind), Some(token), Some(body)) = (
        be32(plain, 4),
        plain
            .get(20..28)
            .and_then(|t| <[u8; 8]>::try_from(t).ok())
            .map(u64::from_be_bytes),
        plain.get(PKG_HEADER..size),
    ) else {
        return Ok(None);
    };
    let package = Package {
        kind,
        reply_token: token,
        body: body.to_vec(),
    };
    plain.drain(..size);
    Ok(Some(package))
}

pub fn package(kind: u32, message_type: u32, reply_token: u64, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(PKG_HEADER + body.len());
    p.extend_from_slice(&((PKG_HEADER + body.len()) as u32).to_be_bytes());
    p.extend_from_slice(&kind.to_be_bytes());
    p.extend_from_slice(&0u64.to_be_bytes());
    p.extend_from_slice(&message_type.to_be_bytes());
    p.extend_from_slice(&reply_token.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(body);
    p
}

/// Serve one Ultra data stream until it ends: sealed like the control
/// channel under the stream's keys, packages in; a sync package is
/// acknowledged, every message handed to `handle`, its answers sent as
/// commands.
pub fn serve(
    mut stream: TcpStream,
    secret: &[u8; 32],
    seed: i64,
    name: &str,
    alive: &dyn Fn() -> bool,
    handle: &mut dyn FnMut(&[u8]) -> Vec<Vec<u8>>,
) {
    let read_key = stream_key(secret, seed, "DataStream-Output-Encryption-Key");
    let write_key = stream_key(secret, seed, "DataStream-Input-Encryption-Key");
    let mut cipher = FrameCipher::new(&read_key, &write_key);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let mut sealed = Vec::new();
    let mut plain = Vec::new();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while alive() {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        sealed.extend_from_slice(&buf[..n]);
        if !cipher.decrypt(&mut sealed, &mut plain) {
            log::warn!("cp-av: {name} decrypt failed");
            break;
        }
        out.clear();
        loop {
            match next_package(&mut plain) {
                Ok(Some(p)) if p.kind == PACKAGE_REPLY => {}
                Ok(Some(p)) => {
                    if p.kind == PACKAGE_SYNC {
                        let ack = package(PACKAGE_REPLY, 0, p.reply_token, &[]);
                        cipher.encrypt_into(&ack, &mut out);
                    }
                    for answer in handle(&p.body) {
                        let p = package(PACKAGE_ASYNC, MSG_TYPE_COMMAND, 0, &answer);
                        cipher.encrypt_into(&p, &mut out);
                    }
                }
                Ok(None) => break,
                Err(size) => {
                    log::warn!("cp-av: {name}: a package of {size} bytes");
                    return;
                }
            }
        }
        if !out.is_empty() && stream.write_all(&out).is_err() {
            break;
        }
    }
    log::info!("cp-av: {name} closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info;
    use crate::{Config, plist};
    use lehu_core::settings::NightMode;
    use lehu_video::Codec;

    #[test]
    fn a_theme_file_reads_and_a_bad_one_is_refused() {
        let theme = Theme::parse(
            "# made on a PC\nasset=44C9FA31EC\nversion = 985\nsdk=19.0\ncenter=Center_Display\n\
             layout=0D38B526\npalette=blue\n",
        )
        .unwrap();
        assert_eq!(theme.asset, "44C9FA31EC");
        assert_eq!(theme.version, 985);
        assert_eq!(theme.sdk, "19.0");
        assert_eq!(theme.center, "Center_Display");
        assert_eq!(
            theme.layout,
            Some(Layout {
                id: "0D38B526".into(),
                palette: "blue".into()
            })
        );
        // What scripts/ultra-theme.py writes.
        let made = Theme::parse(
            "# CarPlay Ultra theme 44C9FA31EC:985:19.0, from Apple's catalog\nasset=44C9FA31EC\n\
             version=985\nsdk=19.0\ncenter=Center_Display\n\
             layout=6C45CC47-EC01-527A-B40D-1316AA9C936F\npalette=custom\n",
        )
        .unwrap();
        assert_eq!(
            made.layout.map(|l| l.id),
            Some("6C45CC47-EC01-527A-B40D-1316AA9C936F".to_string())
        );
        let plain = Theme::parse("asset=A\nversion=1\nsdk=1\ncenter=C\n").unwrap();
        assert_eq!(plain.layout, None);
        for bad in [
            "",
            "asset=A\nversion=x\nsdk=1\ncenter=C",
            "asset=A\nversion=1\nsdk=1",
            "asset=A B\nversion=1\nsdk=1\ncenter=C",
            "asset=A\nversion=1\nsdk=1\ncenter=C\ncolour=red",
            "asset A",
            &format!("asset={}\nversion=1\nsdk=1\ncenter=C", "x".repeat(65)),
        ] {
            assert!(Theme::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Theme::load(Path::new("/nonexistent")), None);
    }

    #[test]
    fn features_are_taken_only_when_offered() {
        let both = Enabled::from_offer(&["hevc", "vehicleStateProtocol", "uiSync"], true);
        assert_eq!(
            both.names().collect::<Vec<_>>(),
            ["vehicleStateProtocol", "uiSync"]
        );
        let unthemed = Enabled::from_offer(&["vehicleStateProtocol", "uiSync"], false);
        assert!(unthemed.vehicle_state && !unthemed.ui_sync);
        assert_eq!(Enabled::from_offer(&["hevc"], true), Enabled::default());
        assert_eq!(
            Channel::of(&vehicle::DATASTREAM_UUID.to_lowercase(), both),
            Some(Channel::VehicleState {
                high_priority: false
            })
        );
        assert_eq!(
            Channel::of(vehicle::HIGH_PRIORITY_DATASTREAM_UUID, both),
            Some(Channel::VehicleState {
                high_priority: true
            })
        );
        assert_eq!(
            Channel::of(ui_sync::DATASTREAM_UUID, both),
            Some(Channel::UiSync)
        );
        assert_eq!(Channel::of(ui_sync::DATASTREAM_UUID, unthemed), None);
        assert_eq!(Channel::of(IAP_DATASTREAM_UUID, both), None);
        assert_eq!(Channel::of("", Enabled::default()), None);
    }

    #[test]
    fn the_info_carries_the_panels_and_no_second_screen() {
        let cfg = Config::for_test(Codec::H264);
        let mut v = info::info_value(&cfg, NightMode::Auto);
        let vehicle = vehicle::VehicleState::new("LEHU", "1");
        add_info(&mut v, &vehicle, false, false);
        let bytes = plist::encode(&v);
        let v = plist::decode(&bytes).unwrap();
        let displays = v.get("displays").and_then(Value::as_array).unwrap();
        assert_eq!(displays.len(), 1, "one screen");
        assert!(
            displays
                .iter()
                .all(|d| d.int_or("type", 0) == info::STREAM_MAIN_SCREEN)
        );
        assert_eq!(displays[0].bool_of("cornerMasks"), Some(false));
        let area = &displays[0]
            .get("viewAreas")
            .and_then(Value::as_array)
            .unwrap()[0];
        assert_eq!(area.bool_of("viewAreaTransitionControl"), Some(false));
        let panels = v.get("displayPanels").and_then(Value::as_array).unwrap();
        assert_eq!(panels.len(), 2);
        let main = &panels[0];
        assert_eq!(
            main.str_or("uid", ""),
            format!("{}-panel", info::MAIN_DISPLAY_UUID)
        );
        assert!(
            main.get("displayPlugins").is_none(),
            "the screen keeps its picture"
        );
        let streams = main
            .get("initialVideoStreams")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(streams[0].str_or("uuid", ""), info::MAIN_DISPLAY_UUID);
        let instruments = &panels[1];
        assert_eq!(instruments.int_or("widthPixels", 0), 800);
        assert_eq!(
            instruments.get("initialVideoStreams"),
            Some(&Value::Array(Vec::new())),
            "no second stream"
        );
        let plugins = instruments
            .get("displayPlugins")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(
            plugins[0].get("videoStreams"),
            Some(&Value::Array(Vec::new()))
        );
        assert_eq!(
            instruments.get("properties"),
            Some(&Value::Array(vec![Value::str("showsInstruments")]))
        );
        assert!(v.get(vehicle::INFO_KEY).is_some());
        assert!(v.get(ui_sync::INFO_KEY).is_none());

        // With a theme: the screen and its panel bear the theme's name.
        let mut cfg = Config::for_test(Codec::H264);
        cfg.ultra = true;
        cfg.theme =
            Some(Theme::parse("asset=A\nversion=1\nsdk=19.0\ncenter=Center_Display").unwrap());
        let mut v = info::info_value(&cfg, NightMode::Auto);
        add_info(&mut v, &vehicle, true, true);
        let panels = v.get("displayPanels").and_then(Value::as_array).unwrap();
        assert_eq!(panels[0].str_or("uid", ""), "Center_Display");
        let streams = panels[0]
            .get("initialVideoStreams")
            .and_then(Value::as_array);
        assert_eq!(streams.unwrap()[0].str_or("uuid", ""), "Center_Display");
        assert!(v.get(ui_sync::INFO_KEY).is_some());
    }

    #[test]
    fn packages_frame_and_unframe() {
        let mut plain = [
            package(PACKAGE_SYNC, MSG_TYPE_COMMAND, 9, b"one"),
            package(PACKAGE_ASYNC, 0, 0, b""),
        ]
        .concat();
        plain.extend_from_slice(&package(PACKAGE_ASYNC, 0, 0, b"three")[..20]);
        assert_eq!(
            next_package(&mut plain),
            Ok(Some(Package {
                kind: PACKAGE_SYNC,
                reply_token: 9,
                body: b"one".to_vec()
            }))
        );
        assert_eq!(next_package(&mut plain).unwrap().unwrap().body, b"");
        assert_eq!(next_package(&mut plain), Ok(None), "cut short, wait");
        let mut bad = 8u32.to_be_bytes().to_vec();
        assert_eq!(next_package(&mut bad), Err(8));
        let mut huge = u32::MAX.to_be_bytes().to_vec();
        assert!(next_package(&mut huge).is_err());
        assert_eq!(next_package(&mut vec![0, 0]), Ok(None));
    }
}
