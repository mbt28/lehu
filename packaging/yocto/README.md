# Yocto packaging for LEHU

`meta-lehu` is a layer with the `lehu` recipe. Add it to the build's
`bblayers.conf` and the image's packages:

```
BBLAYERS += "/path/to/lehu/packaging/yocto/meta-lehu"
IMAGE_INSTALL:append = " lehu"
```

The recipe builds LEHU from its git repository, installs it with its
systemd unit, its settings layer for a systemd image, the udev rules for
phones, the D-Bus policy for BlueZ and the sudo rule, and creates the user
`lehu`. Build hosts without access to GitHub point it at a local checkout:

```
LEHU_GIT_URI = "git:///path/to/lehu;protocol=file;branch=main"
```

## In the image's layer

What belongs to one board or image goes into a `lehu_%.bbappend` there:

```
# The release this image ships.
SRCREV = "<commit>"
# Code for the board's CPU.
RUSTFLAGS:append = " -Ctarget-cpu=arm926ej-s"
# The image's own settings layer, users or services.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"
RDEPENDS:${PN}:append = " my-image-users"
```

## The crate list

The recipe fetches the crates of `Cargo.lock` itself
([lehu-crates.inc](meta-lehu/recipes-lehu/lehu/lehu-crates.inc)). After
any change to `Cargo.lock`, write it again:

```sh
packaging/yocto/update-crates.py
```
