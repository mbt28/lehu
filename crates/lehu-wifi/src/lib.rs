// SPDX-License-Identifier: GPL-3.0-or-later
//! The head unit's Wi-Fi access point, as configured by the system: we read
//! it, we do not run it.
//!
//! The AP is started by init (hostapd from its config file) and stays up
//! whether or not LEHU runs, which is what makes it usable for debugging
//! and deployment. The phones are handed its credentials over their
//! Bluetooth bootstraps, so what we tell them must describe a network that
//! exists: that is guaranteed by reading the EFFECTIVE config, the file
//! hostapd was started from, rather than by owning the process. A change
//! from the screens rewrites that file atomically and restarts the AP
//! through the command the image provides (LEHU is unprivileged).

mod link;
mod radio;

pub use link::{Channel, Link, LinkProbe, Nl80211, Station, channel_number};
pub use radio::{
    Band, COUNTRIES, Radio, channels, check_radio, parse_radio, rewrite_radio, set_directives,
    settle, widths,
};

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use lehu_core::settings::Wifi;

/// WPA2's limits; hostapd refuses to start outside them, and a rejected
/// config would leave the board with no AP at all.
pub const SSID_MIN: usize = 1;
pub const SSID_MAX: usize = 32;
pub const PASS_MIN: usize = 8;
pub const PASS_MAX: usize = 63;

/// The access point as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    /// The interface hostapd is bound to.
    pub iface: String,
    /// What to tell the phone to join.
    pub ssid: String,
    /// The WPA2 pre-shared key.
    pub passphrase: String,
    pub channel: u32,
    /// Our IPv4 on that interface (the phone's endpoint).
    pub ip: String,
    /// The interface's MAC.
    pub bssid: String,
}

impl Params {
    /// The AP looks usable: a network name was found and the interface is
    /// up with an address. Not proof that hostapd is beaconing, but it
    /// catches "the AP was never started", the case worth reporting.
    pub fn valid(&self) -> bool {
        !self.ssid.is_empty() && !self.ip.is_empty()
    }
}

#[derive(Debug)]
pub enum Error {
    /// The SSID is not 1 to 32 bytes.
    SsidLength(usize),
    /// The passphrase is not 8 to 63 bytes.
    PassphraseLength(usize),
    /// A newline would inject a hostapd directive.
    Newline,
    /// White space at either end of the name or passphrase.
    Spaces,
    /// The country is not two capital letters.
    Country,
    /// 5 GHz without a country: the world domain allows no AP there.
    NoCountry,
    /// A width the band does not have.
    Width(u32),
    /// A channel the band, width and country do not allow.
    Channel(u32),
    Read(io::Error),
    Write(io::Error),
}

impl Error {
    /// The same, short enough for a screen's header.
    pub fn short(&self) -> String {
        let denied = |e: &io::Error| e.kind() == io::ErrorKind::PermissionDenied;
        match self {
            Error::SsidLength(_) => format!("The name needs {SSID_MIN}-{SSID_MAX} characters"),
            Error::PassphraseLength(_) => {
                format!("The password needs {PASS_MIN}-{PASS_MAX} characters")
            }
            Error::Newline | Error::Spaces => "No spaces at the ends".into(),
            Error::Country => "Not a country code".into(),
            Error::NoCountry => "5 GHz needs the country".into(),
            Error::Width(w) => format!("{w} MHz is not possible here"),
            Error::Channel(c) => format!("Channel {c} is not allowed here"),
            Error::Read(e) if e.kind() == io::ErrorKind::NotFound => {
                "No access point config".into()
            }
            Error::Read(e) | Error::Write(e) if denied(e) => "Not allowed to change it".into(),
            Error::Read(_) => "Cannot read its config".into(),
            Error::Write(_) => "Cannot save its config".into(),
        }
    }
}

/// How a change that was written took effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The AP was restarted with it.
    Restarted,
    /// Written, but the restart did not run: it applies at the next boot.
    AtBoot,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::SsidLength(n) => {
                write!(
                    f,
                    "the network name is {n} bytes, not {SSID_MIN} to {SSID_MAX}"
                )
            }
            Error::PassphraseLength(n) => {
                write!(
                    f,
                    "the passphrase is {n} bytes, not {PASS_MIN} to {PASS_MAX}"
                )
            }
            Error::Newline => f.write_str("the name and passphrase cannot contain newlines"),
            Error::Spaces => {
                f.write_str("the name and passphrase cannot start or end with a space")
            }
            Error::Country => f.write_str("the country is not a two-letter code"),
            Error::NoCountry => f.write_str("an access point on 5 GHz needs the country"),
            Error::Width(w) => write!(f, "{w} MHz is not a width of that band"),
            Error::Channel(c) => {
                write!(
                    f,
                    "channel {c} is not allowed with that band, width and country"
                )
            }
            Error::Read(e) => write!(f, "cannot read the access point config: {e}"),
            Error::Write(e) => write!(f, "cannot write the access point config: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The system's access point.
pub struct AccessPoint;

impl AccessPoint {
    /// Read the effective configuration. Cheap; read it when needed rather
    /// than caching, so a change made through the screens is picked up.
    pub fn read(settings: &Wifi) -> Params {
        let mut p = Params {
            iface: settings.interface.clone(),
            ..Default::default()
        };
        match fs::read_to_string(&settings.hostapd_conf) {
            Ok(text) => parse_into(&text, &mut p),
            Err(e) => {
                log::warn!(
                    "wifi: cannot read {} ({e}); no system access point configured?",
                    settings.hostapd_conf
                );
            }
        }
        p.ip = ipv4_of(&p.iface).unwrap_or_default();
        p.bssid = mac_of(&p.iface).unwrap_or_default();
        p
    }

    /// The radio part of the effective configuration.
    pub fn read_radio(settings: &Wifi) -> Result<Radio, Error> {
        let text = fs::read_to_string(&settings.hostapd_conf).map_err(Error::Read)?;
        Ok(parse_radio(&text))
    }

    /// Change the advertised network: rewrite ssid and passphrase in
    /// hostapd's config atomically, then restart the AP with the configured
    /// command so the change takes effect. The config is left untouched
    /// when the values are outside what WPA2 accepts.
    pub fn configure(settings: &Wifi, ssid: &str, passphrase: &str) -> Result<Applied, Error> {
        check(ssid, passphrase)?;
        Self::edit(settings, |text| rewrite(text, ssid, passphrase))?;
        log::info!("wifi: access point -> '{ssid}'");
        Ok(restart(settings))
    }

    /// Change the radio -- band, channel, width, country -- the same way.
    /// Refused, the file untouched, unless hostapd would start with it.
    pub fn configure_radio(settings: &Wifi, radio: &Radio) -> Result<Applied, Error> {
        check_radio(radio)?;
        Self::edit(settings, |text| rewrite_radio(text, radio))?;
        log::info!(
            "wifi: access point -> {} channel {}, {} MHz, country '{}'",
            if radio.band == Band::G5 {
                "5 GHz"
            } else {
                "2.4 GHz"
            },
            radio.channel,
            radio.width,
            radio.country
        );
        Ok(restart(settings))
    }

    /// The config read, changed and written back atomically.
    fn edit(settings: &Wifi, change: impl FnOnce(&str) -> String) -> Result<(), Error> {
        let path = Path::new(&settings.hostapd_conf);
        let text = fs::read_to_string(path).map_err(Error::Read)?;
        write_atomically(path, change(&text).as_bytes()).map_err(Error::Write)
    }
}

/// Restart through the image's command rather than stopping and starting
/// the interface: on this board taking the AP down unloads the radio's
/// transport module, which takes the interface away entirely instead of
/// just the AP.
fn restart(settings: &Wifi) -> Applied {
    let restart = settings.ap_restart_command.trim();
    if restart.is_empty() {
        log::info!("wifi: no restart command; the change applies at the next boot");
        return Applied::AtBoot;
    }
    log::info!("wifi: restarting the access point with `{restart}`");
    match lehu_core::shell::privileged(restart).status() {
        Ok(s) if s.success() => Applied::Restarted,
        Ok(s) => {
            log::warn!(
                "wifi: the restart command exited with {s}; the change applies at the next boot"
            );
            Applied::AtBoot
        }
        Err(e) => {
            log::warn!(
                "wifi: the restart command did not run ({e}); the change applies at the next boot"
            );
            Applied::AtBoot
        }
    }
}

/// What WPA2 accepts.
pub fn check(ssid: &str, passphrase: &str) -> Result<(), Error> {
    if !(SSID_MIN..=SSID_MAX).contains(&ssid.len()) {
        return Err(Error::SsidLength(ssid.len()));
    }
    if !(PASS_MIN..=PASS_MAX).contains(&passphrase.len()) {
        return Err(Error::PassphraseLength(passphrase.len()));
    }
    if ssid.contains(['\r', '\n']) || passphrase.contains(['\r', '\n']) {
        return Err(Error::Newline);
    }
    // The config is read back trimmed, so white space at either end would
    // be lost on the way and the phone told a name the AP does not use.
    if ssid.trim() != ssid || passphrase.trim() != passphrase {
        return Err(Error::Spaces);
    }
    Ok(())
}

/// `key=value`, ignoring comments. hostapd.conf has no quoting or
/// continuations, so this is the whole format.
pub fn setting(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// The directives this crate reads, from the config text into `p`; the
/// last of a repeated key counts, as hostapd takes it.
pub fn parse_into(text: &str, p: &mut Params) {
    for line in text.lines() {
        match setting(line) {
            Some(("interface", v)) => p.iface = v.to_string(),
            Some(("ssid", v)) => p.ssid = v.to_string(),
            Some(("wpa_passphrase", v)) => p.passphrase = v.to_string(),
            Some(("channel", v)) => p.channel = v.parse().unwrap_or(0),
            _ => {}
        }
    }
}

/// The config text with its ssid and wpa_passphrase lines replaced (or
/// appended when missing), every other line kept as it is: the file is
/// hand-maintained on the image and carries the driver settings the AP
/// depends on.
pub fn rewrite(text: &str, ssid: &str, passphrase: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let (mut wrote_ssid, mut wrote_pass) = (false, false);
    for line in text.lines() {
        match setting(line) {
            Some(("ssid", _)) => {
                out.push_str("ssid=");
                out.push_str(ssid);
                wrote_ssid = true;
            }
            Some(("wpa_passphrase", _)) => {
                out.push_str("wpa_passphrase=");
                out.push_str(passphrase);
                wrote_pass = true;
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if !wrote_ssid {
        out.push_str("ssid=");
        out.push_str(ssid);
        out.push('\n');
    }
    if !wrote_pass {
        out.push_str("wpa_passphrase=");
        out.push_str(passphrase);
        out.push('\n');
    }
    out
}

/// Write `body` to `path` through a temporary file next to it, synced and
/// renamed into place, mode 0600: it holds the passphrase, and a torn write
/// would leave the board with an AP that will not start.
pub fn write_atomically(path: &Path, body: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
        drop(f);
        fs::rename(tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// The interface's IPv4 address, if it has one.
pub fn ipv4_of(iface: &str) -> Option<String> {
    addresses(iface, libc::AF_INET, |sa| {
        // SAFETY: the family was checked: this is a sockaddr_in.
        let s4 = unsafe { &*(sa as *const libc::sockaddr_in) };
        let b = s4.sin_addr.s_addr.to_ne_bytes();
        Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
    })
}

/// The interface's IPv6 link-local address (`fe80::...`), if it has one.
/// A phone on the AP reaches us over it, scoped to its own link.
pub fn link_local_of(iface: &str) -> Option<String> {
    addresses(iface, libc::AF_INET6, |sa| {
        // SAFETY: the family was checked: this is a sockaddr_in6.
        let s6 = unsafe { &*(sa as *const libc::sockaddr_in6) };
        let a = std::net::Ipv6Addr::from(s6.sin6_addr.s6_addr);
        a.is_unicast_link_local().then(|| a.to_string())
    })
}

/// The first address of `family` on `iface` that `pick` accepts.
fn addresses(
    iface: &str,
    family: libc::c_int,
    pick: impl Fn(*const libc::sockaddr) -> Option<String>,
) -> Option<String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: plain call; the list is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: a node of the list getifaddrs returned.
        let a = unsafe { &*cur };
        cur = a.ifa_next;
        if a.ifa_addr.is_null() || a.ifa_name.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated name from getifaddrs.
        let name = unsafe { std::ffi::CStr::from_ptr(a.ifa_name) };
        if name.to_bytes() != iface.as_bytes() {
            continue;
        }
        // SAFETY: a live sockaddr.
        if unsafe { (*a.ifa_addr).sa_family } != family as libc::sa_family_t {
            continue;
        }
        if let Some(s) = pick(a.ifa_addr) {
            found = Some(s);
            break;
        }
    }
    // SAFETY: the list getifaddrs gave us.
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The interface's MAC, from sysfs.
pub fn mac_of(iface: &str) -> Option<String> {
    let text = fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?;
    let mac = text.trim();
    (!mac.is_empty()).then(|| mac.to_string())
}

/// Whether the interface has carrier: for an AP interface, that hostapd
/// is beaconing on it. `None` when the interface does not exist (or is
/// down, when the kernel refuses the read).
pub fn carrier(iface: &str) -> Option<bool> {
    let text = fs::read_to_string(format!("/sys/class/net/{iface}/carrier")).ok()?;
    Some(text.trim() == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "# the board's AP\n\
interface=wlan0\n\
driver=nl80211\n\
ssid = Old Name\n\
hw_mode=g\n\
channel=6\n\
wpa=2\n\
wpa_passphrase=oldsecret\n\
wpa_key_mgmt=WPA-PSK\n";

    #[test]
    fn the_config_parses_and_rewrites_in_place() {
        let mut p = Params::default();
        parse_into(CONF, &mut p);
        assert_eq!(p.iface, "wlan0");
        assert_eq!(p.ssid, "Old Name");
        assert_eq!(p.passphrase, "oldsecret");
        assert_eq!(p.channel, 6);
        assert!(!p.valid(), "no address yet");
        let out = rewrite(CONF, "LEHU Car", "newsecret1");
        assert_eq!(
            out,
            "# the board's AP\n\
interface=wlan0\n\
driver=nl80211\n\
ssid=LEHU Car\n\
hw_mode=g\n\
channel=6\n\
wpa=2\n\
wpa_passphrase=newsecret1\n\
wpa_key_mgmt=WPA-PSK\n"
        );
        let mut back = Params::default();
        parse_into(&out, &mut back);
        assert_eq!(back.ssid, "LEHU Car");
        assert_eq!(back.passphrase, "newsecret1");
        assert_eq!(back.channel, 6);
        // Missing directives are appended.
        let out = rewrite("interface=wlan0\n", "N", "12345678");
        assert_eq!(out, "interface=wlan0\nssid=N\nwpa_passphrase=12345678\n");
        // Comments and blanks are kept, a commented ssid is not a setting.
        assert_eq!(setting("# ssid=x"), None);
        assert_eq!(setting(""), None);
        assert_eq!(setting("key = value "), Some(("key", "value")));
        assert_eq!(setting("novalue"), None);
    }

    #[test]
    fn wpa2_limits_are_enforced() {
        assert!(check("LEHU", "lehu1234").is_ok());
        assert!(matches!(check("", "lehu1234"), Err(Error::SsidLength(0))));
        assert!(matches!(
            check(&"x".repeat(33), "lehu1234"),
            Err(Error::SsidLength(33))
        ));
        assert!(matches!(
            check("LEHU", "short"),
            Err(Error::PassphraseLength(5))
        ));
        assert!(matches!(
            check("LEHU", &"p".repeat(64)),
            Err(Error::PassphraseLength(64))
        ));
        assert!(matches!(check("LE\nHU", "lehu1234"), Err(Error::Newline)));
        assert!(matches!(check("LEHU", "lehu\r1234"), Err(Error::Newline)));
    }

    #[test]
    fn the_file_is_replaced_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lehu-wifi-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hostapd.conf");
        fs::write(&path, CONF).unwrap();
        let settings = Wifi {
            hostapd_conf: path.to_string_lossy().into_owned(),
            ap_restart_command: String::new(),
            ..Wifi::default()
        };
        assert_eq!(
            AccessPoint::configure(&settings, "New", "newsecret").unwrap(),
            Applied::AtBoot
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("ssid=New\n"));
        assert!(text.contains("wpa_passphrase=newsecret\n"));
        assert!(text.contains("driver=nl80211\n"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!dir.join("hostapd.conf.tmp").exists());
        let p = AccessPoint::read(&settings);
        assert_eq!(p.ssid, "New");
        assert_eq!(p.iface, "wlan0");
        assert!(AccessPoint::configure(&settings, "", "newsecret").is_err());
        assert!(fs::read_to_string(&path).unwrap().contains("ssid=New\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_radio_is_written_checked_and_refused_without_permission() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lehu-wifi-radio-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hostapd.conf");
        fs::write(&path, CONF).unwrap();
        let settings = Wifi {
            hostapd_conf: path.to_string_lossy().into_owned(),
            ap_restart_command: String::new(),
            ..Wifi::default()
        };
        let before = AccessPoint::read_radio(&settings).unwrap();
        assert_eq!(
            (before.band, before.channel, before.width),
            (Band::G2, 6, 20)
        );
        let five = Radio {
            band: Band::G5,
            channel: 44,
            width: 80,
            country: "DE".into(),
        };
        assert_eq!(
            AccessPoint::configure_radio(&settings, &five).unwrap(),
            Applied::AtBoot
        );
        assert_eq!(AccessPoint::read_radio(&settings).unwrap(), five);
        let p = AccessPoint::read(&settings);
        assert_eq!((p.ssid.as_str(), p.channel), ("Old Name", 44));
        // Refused before the file is touched.
        let bad = Radio {
            channel: 149,
            ..five.clone()
        };
        let e = AccessPoint::configure_radio(&settings, &bad).unwrap_err();
        assert_eq!(e.short(), "Channel 149 is not allowed here");
        assert_eq!(AccessPoint::read_radio(&settings).unwrap(), five);
        // A directory LEHU may not write: said so, the file as it was.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let denied = AccessPoint::configure_radio(
            &settings,
            &Radio {
                channel: 36,
                ..five.clone()
            },
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        // Root writes anyway; everyone else is told.
        if !lehu_core::shell::is_root() {
            let e = denied.unwrap_err();
            assert!(matches!(e, Error::Write(_)), "{e}");
            assert_eq!(e.short(), "Not allowed to change it");
            assert_eq!(AccessPoint::read_radio(&settings).unwrap(), five);
        }
        let missing = Wifi {
            hostapd_conf: dir.join("none.conf").to_string_lossy().into_owned(),
            ..settings
        };
        assert_eq!(
            AccessPoint::read_radio(&missing).unwrap_err().short(),
            "No access point config"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_config_reads_as_an_empty_ap() {
        let settings = Wifi {
            hostapd_conf: "/nonexistent/hostapd.conf".into(),
            interface: "lo".into(),
            ..Wifi::default()
        };
        let p = AccessPoint::read(&settings);
        assert!(p.ssid.is_empty());
        assert!(!p.valid());
        assert_eq!(p.iface, "lo");
        assert_eq!(ipv4_of("lo").as_deref(), Some("127.0.0.1"));
        assert_eq!(link_local_of("lo"), None);
        assert_eq!(carrier("no-such-interface"), None);
        assert!(matches!(
            AccessPoint::configure(&settings, "LEHU", "lehu1234"),
            Err(Error::Read(_))
        ));
    }
}
