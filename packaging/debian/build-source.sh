#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Make LEHU's Debian source package from the commit checked out.
#
#   packaging/debian/build-source.sh SUITE [OUTDIR]
#
# SUITE is the Raspberry Pi OS release the package is for (trixie); it
# goes into the version (0.1.0-1+trixie), so one repository can hold a
# package per release. OUTDIR gets:
#
#   lehu_VERSION.orig.tar.xz               the commit (git archive)
#   lehu_VERSION.orig-cargo-vendor.tar.xz  the crates of Cargo.lock
#   lehu_VERSION-1+SUITE.debian.tar.xz     packaging/debian/debian
#   lehu_VERSION-1+SUITE.dsc
#
# The crates come from cargo's cache or crates.io; the package then builds
# offline (dpkg-source -x, then dpkg-buildpackage). Changes not committed
# are not in it.
set -eu

suite=${1:?usage: build-source.sh SUITE [OUTDIR]}
out=${2:-dist}

here=$(cd "$(dirname "$0")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
full="$version-1+$suite"
maintainer=$(sed -n 's/^Maintainer: //p' "$here/debian/control")
# Every file and date in the tarballs is the commit's, so the same commit
# makes the same source package.
when=$(git -C "$root" log -1 --format=%ct HEAD)

mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tree=$work/lehu-$version
orig=$work/lehu_$version.orig.tar.xz
vendor=$work/lehu_$version.orig-cargo-vendor.tar.xz

git -C "$root" archive --format=tar --prefix="lehu-$version/" HEAD | xz -9 -T1 > "$orig"
tar -C "$work" -xJf "$orig"

# The crates, without the sources of those no Linux build compiles
# (Windows' alone are 300 MB).
(cd "$tree" && cargo vendor --locked --versioned-dirs --quiet cargo-vendor >/dev/null)
python3 -I "$here/prune-vendor.py" "$tree" aarch64-unknown-linux-gnu
tar -C "$tree" --sort=name --mtime="@$when" --owner=0 --group=0 --numeric-owner \
	--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
	-cf - cargo-vendor | xz -9 -T1 > "$vendor"

cp -r "$here/debian" "$tree/debian"
cat > "$tree/debian/changelog" <<EOF
lehu ($full) $suite; urgency=medium

  * LEHU $version for Raspberry Pi OS ($suite).
    Release notes: https://github.com/mbt28/lehu/releases

 -- $maintainer  $(LC_ALL=C date -u -R -d "@$when")
EOF
find "$tree/debian" -exec touch -h -d "@$when" {} +

(cd "$work" && dpkg-source --compression-level=9 -b "lehu-$version" >/dev/null)
cp "$orig" "$vendor" "$work/lehu_$full.debian.tar.xz" "$work/lehu_$full.dsc" "$out/"
echo "$out/lehu_$full.dsc"
