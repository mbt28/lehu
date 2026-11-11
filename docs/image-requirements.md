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
- `CONFIG_EXFAT_FS`, for the map partition (below).

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
| `/maps/` | the map partition: regions, voice sets (below) |
| `/usr/share/zoneinfo/` | time zones: at least `Europe/Budapest` |

## Map data

The map's data lives on a partition of its own, written on a PC
(Windows, macOS or Linux), so it is **exFAT**. What goes on it, and how a
region is built and copied, is in [map.md](map.md#data-on-the-card).

- **Partition:** the SD card's 7th, `LEHU-MAPS`, exFAT (MBR type `0x07`),
  filling the card's space after the system's. On the F1C200s's MBR card
  it is a logical partition inside the extended p4. An image file can't
  hold a partition that fills a card of unknown size, so the first boot
  makes it: Buildroot's `S02maps` grows p4, adds p7 (4 MiB-aligned) with
  `parted` and formats it with `mkfs.exfat`. A card already in use is not
  repartitioned.
- **Mount:** at `/maps`, always read only:
  `LABEL=LEHU-MAPS /maps exfat ro,noatime,nofail,uid=1000,gid=1000,fmask=0133,dmask=0022 0 0`
  with LEHU's user and group pinned to 1000 (`f1c` on the development
  board, `lehu` on LEHU OS). Numbers, not names: BusyBox `mount` passes
  the options to the kernel unchanged, and exFAT takes only numbers. exFAT has no owners and no journal: the mount
  gives the files to LEHU's user, and being read only is what keeps it
  whole through a power cut. LEHU never writes it: the partition is
  written only on a PC, the card in its reader. `nofail`: the system starts without it, and
  LEHU then falls back to `/var/lib/lehu/maps` and
  `/var/lib/lehu/voice/en`.
- **Tools:** `exfatprogs` (`fsck.exfat`, `mkfs.exfat`) in the image.
- **Nothing in the image:** no regions and no voice set. The user puts
  them on the partition from a PC (the PC tool `tools/lehu-maps`, or the
  scripts by hand). OSTree system updates and LEHU's own updates
  (`lehu-update`) never carry map data, and LEHU downloads none.

## Time

- `tzdata` with at least `Europe/Budapest` (and the zones of the regions
  offered), so arrival times and the clock show local time.

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
