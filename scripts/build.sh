#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Release build for the board. Extra arguments go to cargo.
set -e
cd "$(dirname "$0")/.."
. scripts/env.sh
cargo build --release --target "$LEHU_TARGET" "$@"
ls -l "target/$LEHU_TARGET/release/lehu"
