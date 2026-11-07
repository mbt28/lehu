# The Buildroot host tools: the ARM toolchain, and Rust with the armv5te std.
# Usage:
#   export BR=/path/to/buildroot/output/host
#   source scripts/env.sh   (then `cargo test` runs on the host,
#                            `scripts/build.sh` builds for the board)
if [ -z "${BR:-}" ]; then
	echo "env.sh: set BR to a Buildroot output's host directory (.../output/host)" >&2
	return 1 2>/dev/null || exit 1
fi
if [ ! -x "$BR/bin/arm-buildroot-linux-gnueabi-gcc" ]; then
	echo "env.sh: no ARM toolchain in $BR/bin; is BR a Buildroot output's host directory?" >&2
	return 1 2>/dev/null || exit 1
fi
export PATH="$BR/bin:$PATH"
export LEHU_TARGET=armv5te-unknown-linux-gnueabi
export CC_armv5te_unknown_linux_gnueabi=arm-buildroot-linux-gnueabi-gcc
export CXX_armv5te_unknown_linux_gnueabi=arm-buildroot-linux-gnueabi-g++
export AR_armv5te_unknown_linux_gnueabi=arm-buildroot-linux-gnueabi-ar
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG="$BR/bin/pkg-config"
