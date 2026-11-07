# Buildroot packaging for LEHU

A Buildroot external tree with the `lehu` package. Add it beside the
image's own external tree:

```sh
make BR2_EXTERNAL=/path/to/image-external:/path/to/lehu/packaging/buildroot <defconfig>
```

and enable `BR2_PACKAGE_LEHU`. The package builds LEHU from its git
repository; for local work point the output's `local.mk` at a checkout:

```make
LEHU_OVERRIDE_SRCDIR = /path/to/lehu
LEHU_OVERRIDE_SRCDIR_RSYNC_EXCLUSIONS = --exclude=/target
```

## What it installs

- `/usr/bin/lehu` and the settings layer `/etc/lehu/lehu.toml`
  ([package/lehu/files/lehu.toml](package/lehu/files/lehu.toml)). Its
  access point and power commands suit the F1C200s image; another image
  replaces the file from its rootfs overlay.
- The udev rules for phones and for wired CarPlay's network link, from
  [packaging/common](../common).
- With `BR2_PACKAGE_LEHU_SERVICE` (the default): the user `lehu` and its
  groups, the init script `S99lehu` (or the systemd unit on a systemd
  image), the sudo rule and the D-Bus policy for BlueZ. Without it, the
  image starts LEHU itself and provides the user and permissions.

What the image must provide besides is in
[docs/image-requirements.md](../../docs/image-requirements.md).
