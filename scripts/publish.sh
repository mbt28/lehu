#!/bin/sh
# Prepare the public repository: the tree of the current commit as a single
# first commit, no history, nothing untracked, in a directory beside this
# one. Nothing is pushed; the last lines say how.
#
#   scripts/publish.sh [remote-url]     default github.com/mbt28/lehu
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
remote=${1:-git@github.com:mbt28/lehu.git}
out=$here/../lehu-public
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$here/Cargo.toml" | head -1)

if [ -n "$(git -C "$here" status --porcelain)" ]; then
	echo "publish: the working tree is not clean" >&2
	exit 1
fi
rm -rf "$out"
mkdir -p "$out"
git -C "$here" archive HEAD | tar -x -C "$out"
git -C "$out" init -q -b main
git -C "$out" add -A
git -C "$out" -c user.name=mbt28 -c user.email=btekbas@gmail.com commit -q -m "LEHU $version

A lightweight embedded head unit: CarPlay and Android Auto, wired and
wireless, for small Linux boards, in Rust. See README.md, NOTICE and
docs/ROADMAP.md."
git -C "$out" remote add origin "$remote"
echo "prepared $out at $(git -C "$out" rev-parse --short HEAD) ($(git -C "$out" ls-files | wc -l) files)"
echo "to publish:  git -C $out push -u origin main"
