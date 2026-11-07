# LEHU on Raspberry Pi

Debian packages of LEHU for Raspberry Pi OS (64-bit, Trixie).

## Install

```sh
sudo curl -fsSLo /usr/share/keyrings/lehu-archive-keyring.gpg \
    https://mbt28.github.io/lehu/lehu-archive-keyring.gpg
echo "deb [arch=arm64 signed-by=/usr/share/keyrings/lehu-archive-keyring.gpg] \
https://mbt28.github.io/lehu $(. /etc/os-release && echo $VERSION_CODENAME) main" |
    sudo tee /etc/apt/sources.list.d/lehu.list
sudo apt update
sudo apt install lehu
```

Updates then come with `sudo apt upgrade`.

## Starting at login

LEHU runs as the user logged in on the screen's console, such as `pi`.
The user who installed it with `sudo apt install` is added to the group
`lehu` and to the device groups LEHU needs. Another user is added with
`sudo adduser USER lehu`. The groups take effect at the user's next login.

To start LEHU at every boot:

1. Log the user in on the console automatically:
   `sudo raspi-config nonint do_boot_behaviour B2`
   (or `raspi-config`, System Options, Boot / Auto Login, Console Autologin).
2. Add this line at the end of the user's `~/.profile`:

   ```sh
   [ "$(tty)" = /dev/tty1 ] && exec systemd-cat -t lehu lehu
   ```

   LEHU then starts on the screen's console only, not on SSH logins. When
   it exits, the console logs in again and starts it again.

To run it once by hand from the console, run `lehu`.

## Raspberry Pi models

The packages are for the **Raspberry Pi 5**. LEHU uses its stateless HEVC
decoder through Raspberry Pi OS's own ffmpeg: CarPlay is asked for HEVC,
and a phone that only sends H.264 gets no picture.

The Raspberry Pi Zero 2 W, 3 and 4 decode H.264 with a stateful decoder,
which LEHU does not drive yet; support for them is on the roadmap.

## Setup

- **Display**: LEHU needs the screen to itself. Use Raspberry Pi OS Lite,
  or boot to the console (the autologin above does).
- **CarPlay** needs an Apple MFi authentication coprocessor on I²C. Enable
  I²C (`raspi-config`, Interface Options); LEHU finds the chip on any bus.
- **Wireless phones** need an access point run by hostapd from
  `/etc/hostapd/hostapd.conf` on `wlan0`, with a DHCP server, and BlueZ.
  Wireless Android Auto also needs a hands-free audio gateway, such as
  bluez-alsa. The package makes hostapd's configuration readable by the
  group `lehu` only (`/usr/lib/tmpfiles.d/lehu.conf`, applied at install
  and at every boot).
- **Settings**: `/etc/lehu/lehu.toml` holds this system's settings;
  `/usr/share/doc/lehu/lehu.toml.example` lists all of them. LEHU writes
  the user's settings, phones and pairings to `~/.lehu/`.
- **Log**: `journalctl -t lehu`.

## What the group lehu may do

- Run four commands as root, through `/etc/sudoers.d/lehu`: reboot, power
  off, set the clock from the phone, and restart hostapd.
- Use BlueZ (`/usr/share/dbus-1/system.d/lehu.conf`).
- Read hostapd's configuration.
- Run threads at real-time priority (`/etc/security/limits.d/lehu.conf`).

## Remove

`sudo apt remove lehu` removes the program; `sudo apt purge lehu` also
removes the group `lehu`. The users' settings in `~/.lehu/` stay. Take
the line out of `~/.profile` too.

## Source

Each [release](https://github.com/mbt28/lehu/releases) carries the Debian
source package as one tar (`lehu_VERSION-1+trixie.source.tar`). Building it
is described in LEHU's
[docs/building.md](https://github.com/mbt28/lehu/blob/main/docs/building.md).
