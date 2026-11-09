// SPDX-License-Identifier: GPL-3.0-or-later
//! What the head unit reads from its own files, which a torn write, a bad
//! image or a hand edit can leave as anything: the settings layers, the
//! FastCarPlay importer, the known-device registry and the CarPlay
//! identity and pairings.

use lehu_core::devices::{Kind, Known, Registry};
use lehu_core::settings::{self, Paths, Settings, import};
use lehu_cp::pairing::Store;
use toml::{Table, Value};

use crate::{Target, TempDir, bounded, ensure};

const TOML_WORDS: &[&str] = &[
    "[display]",
    "[audio]",
    "[wifi]",
    "[camera]",
    "[[camera.inputs]]",
    "[system]",
    "[android-auto]",
    "[carplay]",
    "buffer-ms = ",
    "volume = ",
    "fade = ",
    "night-mode = ",
    "\"night\"",
    "\"auto\"",
    "ssid = ",
    "passphrase = ",
    "resolution = ",
    "fps = ",
    "standard = \"pal\"",
    "true",
    "false",
    "nan",
    "inf",
    "-inf",
    "1e308",
    "4294967296",
    "-1",
    "0x10",
    "\"\"",
    "'''",
    "\"\"\"",
    "{}",
    "[]",
    "=",
    "\n",
    ".",
    "#",
];

/// The text a target works on: lossy, so every input is some text.
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// What `load` promises of the settings in force, whatever the files.
fn in_range(s: &Settings) -> Result<(), String> {
    ensure(s.audio.buffer_ms <= settings::BUFFER_MAX_MS, || {
        "buffer-ms out of range".into()
    })?;
    ensure(
        s.audio.volume.is_none_or(|v| v <= settings::VOLUME_MAX),
        || "volume out of range".into(),
    )?;
    ensure((1..=3).contains(&s.android_auto.resolution), || {
        "resolution out of range".into()
    })?;
    ensure((0.0..=10.0).contains(&s.audio.fade), || {
        "fade out of range".into()
    })?;
    ensure((1..=32).contains(&s.wifi.ssid.len()), || {
        "ssid out of range".into()
    })?;
    ensure((8..=63).contains(&s.wifi.passphrase.len()), || {
        "passphrase out of range".into()
    })?;
    ensure(s.camera.inputs.len() <= 4, || {
        "too many camera inputs".into()
    })
}

#[test]
fn settings_files() {
    let dir = TempDir::new("settings");
    let paths = Paths {
        image: dir.path().join("lehu.toml"),
        user: dir.path().join("settings.toml"),
    };
    let defaults = settings::to_toml(&Settings::default()).into_bytes();
    Target::new("settings_files")
        .seed(defaults)
        .seed(b"[audio]\nbuffer-ms = 99999\nvolume = 400\nfade = nan\n[wifi]\nssid = \"\"\n".to_vec())
        .seed(b"[[camera.inputs]]\nstandard = \"pal\"\nbrightness = 40\n[[camera.inputs]]\n[display]\nnight-mode = \"night\"\n".to_vec())
        .dict(TOML_WORDS)
        .max_len(8192)
        .cost(2)
        .run(|b| {
            // The image's layer is the input; the user's layer is its
            // second half, so both readers and the merge see it.
            let t = text(b);
            let cut = (0..=t.len() / 2).rev().find(|&i| t.is_char_boundary(i)).unwrap_or(0);
            std::fs::write(&paths.image, &t).map_err(|e| e.to_string())?;
            std::fs::write(&paths.user, &t[cut..]).map_err(|e| e.to_string())?;
            // A syntax error names a line inside the text.
            if let Err(settings::Error::Syntax { line, .. }) = settings::parse(&t, &paths.image) {
                ensure(line <= t.lines().count() + 1, || format!("syntax error on line {line}"))?;
            }
            let Ok(loaded) = settings::load(&paths) else {
                return Ok(());
            };
            in_range(&loaded.settings)?;
            bounded("clamp notes", loaded.notes.len(), 16, 1, 0)?;
            // The settings in force print as TOML that reads back the same.
            let printed = settings::to_toml(&loaded.settings);
            let table = settings::parse(&printed, &paths.user).map_err(|e| e.to_string())?;
            let back = settings::rebuild(&Table::new(), &table).map_err(|e| e.to_string())?;
            ensure(back == loaded.settings, || "printed settings read back differently".into())?;
            let again = settings::rebuild(&loaded.image, loaded.user.table()).map_err(|e| e.to_string())?;
            ensure(again == loaded.settings, || "a rebuild changes the settings".into())
        });
}

#[test]
fn fastcarplay_import() {
    let preset = b"# A preset\nprotocol = carplay-wired\nui = true\nsource-fps = 60\nandroid-dpi = 135\nlog-level = 3\nmfi-i2c-addr = 0x10\nnight-mode = 2\nvolume = -1\n";
    let user = b"night-mode = 1\ncamera-key = 88\ncamera-fastswitch = true\ncamera-0 = standard=pal,brightness=40,contrast=140,chroma-enhance=0\ncamera-3 = standard=ntsc\naudio-buffer-ms = 800 # tuned\nfade = 0.5\n";
    Target::new("fastcarplay_import")
        .seed(preset.to_vec())
        .seed(user.to_vec())
        .dict(&[
            "=",
            " #",
            "\n",
            "camera-",
            "camera-0 = ",
            "camera-4 = ",
            "standard=",
            "brightness=",
            ",",
            "0x",
            "0X",
            "-1",
            "9223372036854775808",
            "night-mode = ",
            "log-level = ",
            "volume = ",
            "audio-buffer-ms = ",
            "wifi-ssid = ",
            "true",
            "1",
            "nan",
        ])
        .run(|b| {
            let t = text(b);
            let cut = (0..=t.len() / 2)
                .rev()
                .find(|&i| t.is_char_boundary(i))
                .unwrap_or(0);
            let imported = import::from_fastcarplay(&[
                ("settings.txt", &t[..cut]),
                ("usersettings.txt", &t[cut..]),
            ]);
            let lines = t[..cut].lines().count() + t[cut..].lines().count();
            ensure(imported.warnings.len() <= lines, || {
                "more warnings than lines".into()
            })?;
            // What it wrote layers over the defaults or is refused as a
            // whole; never a panic.
            let _ = settings::rebuild(&Table::new(), &imported.table);
            let _ = Value::Table(imported.table);
            Ok(())
        });
}

#[test]
fn device_registry() {
    let dir = TempDir::new("devices");
    let file = dir.path().join("devices.toml");
    let seed = String::from(
        "[[devices]]\nid = \"ctl-1\"\nkind = \"car-play\"\nname = \"iPhone\"\nbt-address = \"AA:BB:CC:DD:EE:FF\"\nusb-serial = \"0000\"\nlast-seen = 100\nheld = true\n\n[[devices]]\nid = \"abc\"\nkind = \"android-auto\"\nname = \"Pixel\"\nlast-seen = 18446744073709551615\n",
    );
    Target::new("device_registry")
        .seed(seed.into_bytes())
        .dict(&[
            "[[devices]]\n",
            "id = ",
            "kind = \"car-play\"",
            "kind = \"android-auto\"",
            "name = ",
            "bt-address = ",
            "usb-serial = ",
            "last-seen = ",
            "held = true",
            "\"\"",
            "\n",
            "-1",
        ])
        .max_len(8192)
        .cost(2)
        .run(|b| {
            std::fs::write(&file, b).map_err(|e| e.to_string())?;
            let mut r = Registry::open(dir.path());
            let before: Vec<_> = r.all().to_vec();
            // What the loop does with it: a session, a link, a hold, a save.
            r.seen(
                "ctl-9",
                Kind::CarPlay,
                "New",
                "11:22:33:44:55:66",
                "serial",
                300,
            );
            r.session_ended();
            r.bt_present("aa:bb:cc:dd:ee:ff", true);
            r.usb_present("0000", false);
            if let Some(first) = before.first() {
                let id = first.id.clone();
                r.hold(&id, !r.is_held(&id));
                let _ = r.by_bt(&first.bt_address);
                let _ = r.by_usb(&first.usb_serial);
            }
            r.save_if_dirty().map_err(|e| e.to_string())?;
            // Saved and read again, it is the same list (presence is not
            // saved).
            let strip = |v: &[Known]| {
                v.iter()
                    .map(|d| {
                        (
                            d.id.clone(),
                            d.kind,
                            d.name.clone(),
                            d.bt_address.clone(),
                            d.usb_serial.clone(),
                            d.last_seen,
                            d.held,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let again = Registry::open(dir.path());
            ensure(strip(again.all()) == strip(r.all()), || {
                "the registry does not read back what it saved".into()
            })
        });
}

/// `Store::open` and `Store::pairing` read hex out of their two files.
#[test]
fn pairing_store_files() {
    let dir = TempDir::new("store");
    let fresh = Store::open(dir.path()).expect("an identity");
    let identity = std::fs::read(dir.path().join("identity.txt")).expect("the identity file");
    fresh.save_pairing("PHONE-1", &[7; 32]).expect("a pairing");
    let pairings = std::fs::read(dir.path().join("pairings.txt")).expect("the pairings file");
    Target::new("pairing_store_files")
        .seed(identity)
        .seed(pairings)
        .dict(&[
            "priv=", "pub=", "pi=", "\n", "=", " ", "PHONE-1 ", "00", "ff", "0",
        ])
        .max_len(1024)
        .cost(8)
        // Non-ASCII hex: bugs::pairing_store_hex_splits_a_character.
        .run(|b| {
            let t = text(b);
            let work = TempDir::new("store-input");
            std::fs::write(work.path().join("identity.txt"), &t).map_err(|e| e.to_string())?;
            std::fs::write(work.path().join("pairings.txt"), &t).map_err(|e| e.to_string())?;
            let store = Store::open(work.path()).map_err(|e| e.to_string())?;
            for line in t.lines() {
                if let Some((id, _)) = line.split_once(' ') {
                    let _ = store.pairing(id);
                }
            }
            let _ = store.pairing("PHONE-1");
            Ok(())
        });
}
