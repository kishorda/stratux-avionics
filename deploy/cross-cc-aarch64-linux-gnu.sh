#!/bin/sh
#
# Linker driver for cross-builds. Cargo's `linker =` takes a bare executable with no
# arguments, and that is the whole reason this file exists.
#
# The target triple comes from this script's own filename, so the armhf variant is just a
# symlink to this file (cross-cc-arm-linux-gnueabihf.sh).
#
# # Why a wrapper instead of rustflags
#
# The sysroot's library path has to be searched BEFORE the toolchain's built-in
# /usr/<triple>/lib, or `-lm` resolves against the dev machine's glibc. On Ubuntu 26.04 that
# is glibc 2.43, whose libm re-versioned the float math functions (acosf@@GLIBC_2.43 rather
# than Bookworm's acosf@@GLIBC_2.17). The link succeeds, and the binary then dies on the Pi
# with "version `GLIBC_2.43' not found".
#
# ld resolves each -l against the -L paths seen SO FAR, so the -L has to come first on the
# command line. Neither `-C link-arg=-L…` nor `-L native=…` achieves that — rustc emits both
# after its own -l flags, which is too late. Putting them ahead of "$@" here does.
#
# The sysroot is located relative to this script, so moving the checkout does not break the
# build the way the old hardcoded paths in .cargo/config.toml did.
#
set -eu

TRIPLE="$(basename "$0" .sh)"
TRIPLE="${TRIPLE#cross-cc-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SYSROOT="$ROOT/sysroot"

if [ ! -d "$SYSROOT/usr/lib/$TRIPLE" ]; then
  echo "!!! No sysroot at $SYSROOT/usr/lib/$TRIPLE" >&2
  echo "    Build one first:  ./deploy/sync-sysroot.sh --offline" >&2
  echo "                 or:  ./deploy/sync-sysroot.sh pi@stratux.local" >&2
  exit 1
fi

exec "$TRIPLE-gcc" \
  --sysroot="$SYSROOT" \
  -B"$SYSROOT/usr/lib/$TRIPLE" \
  -L"$SYSROOT/usr/lib/$TRIPLE" \
  -L"$SYSROOT/lib/$TRIPLE" \
  -Wl,-rpath-link="$SYSROOT/usr/lib/$TRIPLE" \
  "$@"
