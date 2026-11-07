SUMMARY = "LEHU, a lightweight embedded head unit: CarPlay and Android Auto"
DESCRIPTION = "CarPlay and Android Auto, wired and wireless, for small Linux \
boards: hardware video decoding through ffmpeg's v4l2-request, its own \
screens in Slint, ALSA audio. Runs as the user lehu from lehu.service."
HOMEPAGE = "https://github.com/mbt28/lehu"
LICENSE = "GPL-3.0-or-later"
LIC_FILES_CHKSUM = "file://LICENSE;md5=1ebbd3e34237af26da5dc08a4e440464"

# The repository, at a revision the image pins (SRCREV in a bbappend or
# site.conf); a local checkout can stand in for it through LEHU_GIT_URI.
LEHU_GIT_URI ?= "git://github.com/mbt28/lehu.git;protocol=https;branch=main"
SRC_URI = " \
    ${LEHU_GIT_URI};destsuffix=lehu \
    file://lehu.toml \
    file://lehu-sudoers \
    file://lehu.tmpfiles \
"
SRCREV ?= "${AUTOREV}"
PV = "0.1.0+git"
S = "${UNPACKDIR}/lehu"

inherit cargo cargo-update-recipe-crates pkgconfig systemd useradd
# The crates of Cargo.lock, written by packaging/yocto/update-crates.py.
require ${BPN}-crates.inc

DEPENDS = "alsa-lib ffmpeg libopus openssl"

# The binary only, from the workspace.
CARGO_BUILD_FLAGS:append = " -p lehu"
# The workspace's profile strips; leave that to OE, which keeps -dbg.
export CARGO_PROFILE_RELEASE_STRIP = "false"
export OPENSSL_NO_VENDOR = "1"

do_install() {
	install -D -m 0755 ${B}/target/${CARGO_TARGET_SUBDIR}/lehu ${D}${bindir}/lehu
	install -D -m 0644 ${UNPACKDIR}/lehu.toml ${D}${sysconfdir}/lehu/lehu.toml
	install -d -m 0750 ${D}${sysconfdir}/sudoers.d
	install -m 0440 ${UNPACKDIR}/lehu-sudoers ${D}${sysconfdir}/sudoers.d/lehu
	install -D -m 0644 ${UNPACKDIR}/lehu.tmpfiles ${D}${nonarch_libdir}/tmpfiles.d/lehu.conf
	# What every LEHU package installs, from the source tree. The iPhone's
	# network link is systemd-networkd's to bring up, so no rule for it.
	install -D -m 0644 ${S}/packaging/common/lehu-dbus.conf ${D}${datadir}/dbus-1/system.d/lehu.conf
	install -D -m 0644 ${S}/packaging/common/99-lehu.rules ${D}${nonarch_base_libdir}/udev/rules.d/99-lehu.rules
	install -D -m 0644 ${S}/packaging/common/lehu.service ${D}${systemd_system_unitdir}/lehu.service
	sed -i 's,/usr/bin/lehu,${bindir}/lehu,' ${D}${systemd_system_unitdir}/lehu.service
}

USERADD_PACKAGES = "${PN}"
GROUPADD_PARAM:${PN} = "--system i2c; --system netdev; --system plugdev"
USERADD_PARAM:${PN} = "--system --home-dir /var/lib/lehu --no-create-home \
    --shell /sbin/nologin --user-group \
    --groups video,input,audio,i2c,plugdev,netdev lehu"

SYSTEMD_SERVICE:${PN} = "lehu.service"

FILES:${PN} += " \
    ${datadir}/dbus-1/system.d/lehu.conf \
    ${nonarch_base_libdir}/udev/rules.d/99-lehu.rules \
    ${nonarch_libdir}/tmpfiles.d/lehu.conf \
"
# libdbus is opened at run time (dlopen). No fonts: LEHU draws with glyphs
# embedded at build time.
RDEPENDS:${PN} = "dbus-lib sudo"
