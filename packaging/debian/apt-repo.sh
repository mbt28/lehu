#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Add packages to LEHU's apt repository and write its signed index.
#
#   packaging/debian/apt-repo.sh REPO DEB...
#
# REPO is the repository's root (the gh-pages checkout). Each package goes
# into the pool of the suite its version names (1+trixie), and
# each suite's index is rebuilt from its pool. With LEHU_GPG_KEY set to a
# key in the GPG keyring, the index is signed and the public key exported
# as lehu-archive-keyring.gpg; without it the repository is unsigned, for a
# local test only.
set -eu

repo=${1:?usage: apt-repo.sh REPO DEB...}
shift
mkdir -p "$repo"
repo=$(cd "$repo" && pwd)

for deb in "$@"; do
	version=$(dpkg-deb -f "$deb" Version)
	suite=${version##*+}
	case "$suite" in
	trixie) ;;
	*) echo "apt-repo: $deb names no known suite ($version)" >&2; exit 1 ;;
	esac
	mkdir -p "$repo/pool/$suite/main/l/lehu"
	cp "$deb" "$repo/pool/$suite/main/l/lehu/"
done

cd "$repo"
for pool in pool/*; do
	[ -d "$pool" ] || continue
	suite=$(basename "$pool")
	dir=dists/$suite/main/binary-arm64
	mkdir -p "$dir"
	apt-ftparchive packages "$pool" > "$dir/Packages"
	gzip -9kf "$dir/Packages"
	apt-ftparchive \
		-o APT::FTPArchive::Release::Origin=LEHU \
		-o APT::FTPArchive::Release::Label=LEHU \
		-o APT::FTPArchive::Release::Suite="$suite" \
		-o APT::FTPArchive::Release::Codename="$suite" \
		-o APT::FTPArchive::Release::Architectures=arm64 \
		-o APT::FTPArchive::Release::Components=main \
		-o APT::FTPArchive::Release::Description="LEHU for Raspberry Pi OS ($suite)" \
		release "dists/$suite" > "dists/$suite/Release"
	rm -f "dists/$suite/InRelease" "dists/$suite/Release.gpg"
	if [ -n "${LEHU_GPG_KEY:-}" ]; then
		gpg --batch --yes --local-user "$LEHU_GPG_KEY" --clearsign \
			-o "dists/$suite/InRelease" "dists/$suite/Release"
		gpg --batch --yes --local-user "$LEHU_GPG_KEY" --armor --detach-sign \
			-o "dists/$suite/Release.gpg" "dists/$suite/Release"
	fi
done

if [ -n "${LEHU_GPG_KEY:-}" ]; then
	gpg --batch --yes --export "$LEHU_GPG_KEY" > lehu-archive-keyring.gpg
fi
# GitHub Pages serves the files as they are, not through Jekyll.
touch .nojekyll
echo "apt-repo: $repo updated"
