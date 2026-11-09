# Board image requirements

What a Linux image needs to run LEHU. The Buildroot, Yocto and Debian
packaging in [packaging/](../packaging) sets all of this up.

## Kernel

- DRM/KMS with an overlay plane, and a V4L2 stateless (request API) video
  decoder such as cedrus, with enough CMA for one decoding session
  (24 MiB at 800 × 480).
- USB host with `cdc_ncm` and `ipheth` (wired CarPlay's network link),
  IPv6, and the Android accessory mode from user space (usbfs).
- `i2c-dev` for the MFi coprocessor; `evdev` and `uhid` for touch and
  Bluetooth input; Bluetooth with RFCOMM.
- On the F1C200s: boot with `musb_hdrc.use_dma=0`. The USB controller's
  DMA loses bulk transfers and is slower than its PIO mode.

## Libraries and services

| Needed | For |
|---|---|
| ffmpeg 6.1 or 8.x with the v4l2-request patches | video decoding, AAC |
| libopus, ALSA, OpenSSL | audio, Android Auto's TLS |
| libdbus, BlueZ with the `input` and `hog` plugins | Bluetooth (loaded at run time; without it, Bluetooth is off) |
| an HFP audio gateway (e.g. bluez-alsa) | wireless Android Auto: phones require a hands-free profile |
| hostapd, a DHCP server on its interface | wireless phones |

No fonts or fontconfig are needed: the UI's glyphs are built into the
binary.

## User and permissions

LEHU runs as an unprivileged user: `lehu` on the Buildroot and Yocto
images, the console's login user in the group `lehu` on Raspberry Pi OS.
The user is in the groups `video`, `input`, `audio`, `i2c`, `plugdev` and `netdev`.

- `/dev/dri/card0`, `/dev/video*`, `/dev/input/event*`, `/dev/snd/*` and
  the MFi coprocessor's `/dev/i2c-N` readable and writable by those groups.
- A udev rule giving phones' USB devices to `plugdev`, and making their
  `bConfigurationValue` group-writable (Apple vendor `05ac`; Android
  phones before and after the accessory switch, `18d1:2d00/2d01`). See
  [99-lehu.rules](../packaging/common/99-lehu.rules).
- A D-Bus policy allowing the user to call `org.bluez` and to receive its
  calls (the pairing agent and the RFCOMM profiles). See
  [lehu-dbus.conf](../packaging/common/lehu-dbus.conf).
- Real-time priority: `RLIMIT_RTPRIO` of at least 50 (for example
  `ulimit -r 50` before switching to the user, or `LimitRTPRIO=50` in a
  systemd unit). Without it LEHU runs, but its audio and USB threads may be
  starved under load.
- Root actions run through `sudo -n`: reboot, power off, setting the clock
  from the phone, and restarting the access point. See
  [lehu-sudoers](../packaging/buildroot/package/lehu/files/lehu-sudoers).
- hostapd's configuration readable by the user's group (it holds the
  passphrase, so mode 0640), and writable if the access point is to be
  changed from the screens.

## Files

| Path | Use |
|---|---|
| `/etc/lehu/lehu.toml` | the image's default settings (optional) |
| `/var/lib/lehu/` | settings, known phones, pairings, the last update shown (or `~/.lehu` for a user without access to it) |
| `/run/lehu/status.json` | the session's state, for other services to read |
| `/var/cache/lehu-update/` | an update received from the phone |

## Updates

Settings › System › Update needs LEHU OS's installer, which LEHU OS and
the f1c200s Buildroot image carry; without
`/usr/sbin/lehu-os-update` the row says the update is not available.

- `/usr/sbin/lehu-os-update` with `check`, `apply` and `status`, printing
  `name=value` lines (exit 1 with `message=` last), and
  `/run/lehu-os-update/progress` readable by LEHU's user while `apply`
  runs.
- A sudo rule for `check` and `apply` (`status` runs as LEHU's user):
  `lehu ALL=(root) NOPASSWD: /usr/sbin/lehu-os-update check *, /usr/sbin/lehu-os-update apply *, /usr/sbin/lehu-os-update status`.
- `/var/cache/lehu-update`, `1770` owned by root and LEHU's group, on persistent storage with
  room for an update (tens of MB) and its unpacked copy: LEHU receives the
  update there, never in RAM.
- The access point (above) with an IPv4 address: the page is served on it,
  TCP `update.port` (8080) from the phones on it.
- A way to tell that an apply LEHU did not start is still running: under
  systemd, `systemctl is-active lehu-os-update`; without systemd, `apply`
  holds an exclusive `flock` on `/run/lehu-os-update/lock`, which LEHU's
  user can open to read.

## Starting

LEHU needs no arguments. It starts as its user, with the real-time limit
raised, and stops on SIGTERM (it says goodbye to the phone and saves its
state within a few seconds). Any of these starts it:
- an init script, such as
  [S99lehu](../packaging/buildroot/package/lehu/files/S99lehu);
- a systemd unit, such as [lehu.service](../packaging/common/lehu.service);
- the login shell of a user logged in automatically on the console, such
  as `exec lehu` in `~/.profile` (the Raspberry Pi package's way; see
  [its README](../packaging/debian/README.md#starting-at-login)).

## Wireless phones

- The access point interface needs an IPv4 address with a DHCP server,
  and its IPv6 link-local address.
- Reachable on it: UDP 5353 (mDNS), TCP 7000 (CarPlay) and TCP 5277
  (Android Auto).
- Some phones accept wireless Android Auto only on 5 GHz; 5 GHz channels
  need the kernel's regulatory database and a country code.
- `dtim_period=1` in hostapd's configuration keeps Android Auto smooth for
  phones in Wi-Fi power save.
- No other Bluetooth agent may be the default agent.
