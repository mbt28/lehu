# Building

LEHU is a Cargo workspace and needs Rust 1.96 or later. It links ffmpeg
(libavcodec, libavutil), libopus, ALSA and OpenSSL.

| Target | How |
|---|---|
| F1C200s and other small ARM boards | cross-compiled with a Buildroot image's toolchain, from an x86 or ARM PC |
| Raspberry Pi 5 | built natively on Raspberry Pi OS |
| x86-64 PC | built natively, for development and tests |

## F1C200s (Buildroot)

The board build cross-compiles for `armv5te-unknown-linux-gnueabi` with
the toolchain and sysroot of a Buildroot image: the Rust in Buildroot's
host tools has the ARMv5 standard library, and the sysroot has ffmpeg with
the v4l2-request patches. The PC running the build can be x86 or ARM.

```sh
export BR=/path/to/buildroot/output/host   # the image's host tools
source scripts/env.sh                       # PATH, the cross compilers, pkg-config
scripts/build.sh                            # target/armv5te-unknown-linux-gnueabi/release/lehu
```

The release profile is tuned for size (opt-level `z`, fat LTO, one codegen
unit, `panic = "abort"`, stripped): every byte of the binary is RAM on a
64 MB board.

On a Buildroot image, the external tree in
[packaging/buildroot](../packaging/buildroot) builds and installs LEHU with
its settings, init script and permissions; on a Yocto image, the layer in
[packaging/yocto](../packaging/yocto) does the same.

## Raspberry Pi

On Raspberry Pi OS (64-bit, Trixie), with Rust from
[rustup](https://rustup.rs):

```sh
sudo apt install build-essential pkg-config libavcodec-dev libavutil-dev \
    libopus-dev libasound2-dev libssl-dev
cargo build --release -p lehu      # target/release/lehu
```

Do not source `scripts/env.sh` for a native build. Raspberry Pi OS's own
ffmpeg drives the Pi 5's stateless HEVC decoder (`rpi-hevc-dec`), so
CarPlay is asked for HEVC and decoded in hardware; Android Auto is offered
HEVC as well. The Pi 5 has no H.264 decoder, so a phone that only sends
H.264 gets no picture. The earlier models' H.264 decoder is a stateful
one, which LEHU does not drive yet (see the [roadmap](ROADMAP.md)). `lehu --decoders` shows what
was found. LEHU needs the display to itself: run it from a console, with
no desktop session on the screen.

## x86-64 PC

```sh
sudo apt install build-essential pkg-config libavcodec-dev libavutil-dev \
    libopus-dev libasound2-dev libssl-dev     # Debian, Ubuntu
cargo build --release -p lehu
```

An x86 build runs the tests, the screens and the protocols. PC graphics
decode video through VA-API rather than a V4L2 stateless decoder, so on
most PCs LEHU finds no hardware decoder and shows no phone video; it is
meant for development, not for a car.

Without the ffmpeg development headers, a native build still succeeds but
its video decoder is a stub (the build prints a warning); ALSA and Opus
fall back to the runtime libraries when their headers are missing.

## Tests

```sh
cargo test --workspace
```

The tests run on the host, with either a rustup toolchain or Buildroot's
(after `source scripts/env.sh`). They include the screens' controller against a
fake host, the protocols against scripted phones, and the fuzz targets in
`crates/lehu-fuzz` (`LEHU_FUZZ_ITERS=400000` runs them for longer).

The screens can be rendered to images without a board:

```sh
LEHU_SHOTS=/tmp/shots cargo test -p lehu-ui shots -- --ignored
```

## Lint and format

Clippy and rustfmt are not part of Buildroot's toolchain; run them from a
rustup toolchain, in a target directory of their own so the two compilers
never share build artifacts:

```sh
CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Debian packages for Raspberry Pi

The Debian packaging is in [packaging/debian/debian](../packaging/debian/debian).
A source package is made from the commit checked out:

```sh
packaging/debian/build-source.sh trixie dist
```

It has three parts:
- the commit, `lehu_0.2.0.orig.tar.xz`;
- the crates of `Cargo.lock`, `lehu_0.2.0.orig-cargo-vendor.tar.xz`. Crates
  that no Linux build compiles are left as stubs;
- the packaging, `lehu_0.2.0-1+trixie.debian.tar.xz`, described by its
  `.dsc`.

The suite (`trixie`) goes into the version, so one repository can hold a
package for each Raspberry Pi OS release.

The binary package is built from it offline, on a 64-bit Raspberry Pi OS
release the package is for:

```sh
dpkg-source -x dist/lehu_0.2.0-1+trixie.dsc lehu-0.2.0
cd lehu-0.2.0
sudo apt-get build-dep ./
dpkg-buildpackage -b -us -uc
```

Raspberry Pi OS's cargo is older than LEHU needs, so `build-dep` installs
rustup; its toolchain comes from `rustup default stable`. The build runs
the tests, unless `DEB_BUILD_OPTIONS=nocheck` is set.

The package holds:
- the binary;
- the settings layer for Raspberry Pi OS;
- the udev rule for phones;
- the rights of the group `lehu`: the D-Bus policy for BlueZ, the sudoers
  entry and the real-time priority limit.

LEHU runs as the user logged in on the console, in the group `lehu`;
installing the package creates the group and adds the installing user.
`packaging/debian/apt-repo.sh REPO DEB...` adds packages to a signed apt
repository.

## Adding the Debian repository

Each release is published to LEHU's apt repository, for Raspberry Pi OS
Trixie (64-bit) on a Raspberry Pi 5. Add it to the system once, with its
signing key:

```sh
sudo curl -fsSLo /usr/share/keyrings/lehu-archive-keyring.gpg \
    https://mbt28.github.io/lehu/lehu-archive-keyring.gpg
echo "deb [arch=arm64 signed-by=/usr/share/keyrings/lehu-archive-keyring.gpg] \
https://mbt28.github.io/lehu $(. /etc/os-release && echo $VERSION_CODENAME) main" |
    sudo tee /etc/apt/sources.list.d/lehu.list
sudo apt update
```

Then install LEHU:

```sh
sudo apt install lehu
```

LEHU runs as the user logged in on the screen's console, and starts at
login from a line in `~/.profile`: see
[Starting at login](../packaging/debian/README.md#starting-at-login).

Updates come with `sudo apt upgrade`. To remove the repository:

```sh
sudo rm /etc/apt/sources.list.d/lehu.list /usr/share/keyrings/lehu-archive-keyring.gpg
sudo apt update
```

Setting up the Raspberry Pi for LEHU (display, MFi chip, access point) is
described in [packaging/debian/README.md](../packaging/debian/README.md).

## On the board

`lehu` runs without arguments as an unprivileged user and creates its
settings file on the first run. The image's requirements (groups, device
nodes, the real-time priority limit, BlueZ and hostapd access) are in
[image-requirements.md](image-requirements.md); the settings and their
defaults are in [config/lehu.toml](../config/lehu.toml).

A few tools for bring-up:

| Command | What it does |
|---|---|
| `lehu --check-config` | validates the settings layers and names the line of an error |
| `lehu --print-config` | prints the settings in force |
| `lehu --decoders` | lists the hardware decoders found and what each phone is offered |
| `lehu --cpu` | prints the CPU features found |
| `lehu --display-test` | draws on the panel's planes |
| `lehu --camera-test [SECONDS]` | shows the camera and lists its inputs and levels |
| `lehu --import PRESET [USERSETTINGS]` | converts FastCarPlay settings files |
