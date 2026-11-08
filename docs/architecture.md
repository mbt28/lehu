# Architecture

LEHU is one process, built as a single `lehu` binary from a Cargo
workspace. It is designed for the Allwinner F1C200s: one ARM926EJ-S core
at 408 MHz, 64 MB of RAM, no FPU, no GPU, an H.264 decoder (cedrus) and a
display engine with hardware scaling. Whatever runs there runs on larger
boards too.

## Principles

- **Threads and `poll`, no async runtime.** A main loop owns the display,
  the screens and the session state; protocol, audio, decoder, camera and
  Bluetooth threads hand it fixed-size events through bounded queues.
- **No allocation on hot paths.** Buffers are reused, queues are bounded
  with a stated drop policy, and the audio path is fixed point (Q15).
- **Wire data is checked before it is indexed.** Every parser rejects a
  length before it uses it; no `unwrap` on a protocol path. The
  `lehu-fuzz` crate runs fuzz targets over every parser in plain
  `cargo test`.
- **Features at run time, not compile time.** One binary per
  architecture; `lehu_core::cpu` reads the CPU's capabilities once and
  faster paths are chosen from them. The decoders the system has are
  probed at start (`lehu --decoders`), and the phones are offered only
  what decodes in hardware.
- **Unprivileged.** LEHU runs as its own user. What the image must grant
  is listed in [image-requirements.md](image-requirements.md).
- **Settings in three layers.** Compiled-in defaults, the image's
  `/etc/lehu/lehu.toml`, and the user's file, written atomically a moment
  after the last change. Every setting applies as it is changed.

## Crates

| Crate | What it holds |
|---|---|
| `lehu` | the program: the main loop, the session driver, the host of the screens |
| `lehu-core` | CPU features, clocks, bounded queues, logging, settings, the device registry |
| `lehu-session` | what every phone protocol shows the loop: phases, events, commands, sinks |
| `lehu-cp` | CarPlay: usbmux, lockdown, iAP2, MFi, pairing, RTSP/AV, the wireless bootstrap |
| `lehu-aa` | Android Auto: AOAP, framing and TLS, protobuf, the channels, the wireless handoff |
| `lehu-video` | hardware decoding through ffmpeg's v4l2-request, the frame presenter |
| `lehu-audio` | the fixed-point mixer on ALSA, Opus and AAC, the volume, the microphone |
| `lehu-drm` | DRM/KMS planes, dma-buf import, the overlay window |
| `lehu-input` | evdev: the touchscreen and Bluetooth keyboards, routed to the screens or the phone |
| `lehu-usb` | USB hotplug from the kernel's uevent socket, the bus scan, the port reset |
| `lehu-bt` | BlueZ over libdbus: the pairing agent, HID devices, RFCOMM profiles |
| `lehu-wifi` | the access point's configuration, read from hostapd and rewritten |
| `lehu-camera` | the V4L2 reversing camera on the video plane, its inputs and picture levels |
| `lehu-image` | JPEG and PNG decoding for album art |
| `lehu-update` | LEHU OS updates: the page the phone carries them to, the installer's commands |
| `lehu-ui` | the Slint screens and their controller |
| `lehu-fuzz` | fuzz targets for the parsers, run on the host |

## C libraries and pure Rust

| Through C (on the image already) | Pure Rust |
|---|---|
| ffmpeg: hardware decode via v4l2-request, fixed-point AAC | DRM and V4L2 ioctls |
| libopus (fixed point) | USB (`nusb`, usbfs) |
| ALSA | CarPlay crypto (ChaCha20-Poly1305, X25519, Ed25519, SRP, HKDF, P-256) |
| OpenSSL, for TLS | plist, protobuf, TOML, the Slint UI |
| libdbus, loaded at run time, for BlueZ | JPEG and PNG |

## Sessions

The main loop owns every session, device and the screen's "front";
nothing is shared behind a mutex. Protocol threads parse and decrypt in
place and push events into one bounded ring, which the loop polls along
with the DRM fd, evdev, a timerfd and the screens' wake fd.

A session moves through Spawned, Identified, Ready, Live, Backgrounded,
Closing and Closed; a phone's sleep and wake re-enter Ready without ending
it. Every link listens at once: wired and wireless CarPlay and Android
Auto. A phone on the cable always comes first, a wireless session never
takes over from a cable, and the known-device list keeps each phone with
the identities its links gave it. The decoder is built once, for the
active session; the camera parks it.

## Video

At start, LEHU probes the system's V4L2 memory-to-memory decoders and what
ffmpeg can drive through v4l2-request. CarPlay is asked for HEVC only
where it decodes in hardware, H.264 otherwise; Android Auto is offered one
configuration per decodable codec, at a resolution the decoder takes.
`display.codec` can force one. The decoder hands out dma-bufs, which the
display scans out on the primary plane with no copy; the display engine
scales them to the panel.

## Screens

The screens are [Slint](https://slint.dev), rendered in software into an
RGB565 overlay plane above the video. Only the rows that changed are
written, so an unchanged screen costs nothing. While the phone or the
camera is in front, the overlay shows only the window it needs (the volume
bar, the navigation card, the camera's settings column), as an opaque
rectangle: RGB565 has no alpha.

The glyphs are rendered at build time in four sizes of Open Sans, so the
board needs no fonts; text from outside (phone and device names) is reduced
to the embedded characters. Lists are updated in place by id, never
rebuilt, so the knob's focus survives a change. How the screens work for
the user is in the [user guide](user-guide.md).
