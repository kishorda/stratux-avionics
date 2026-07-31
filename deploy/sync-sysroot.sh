#!/usr/bin/env bash
#
# Pull a link sysroot off the Pi so the dev machine can cross-link against it.
#
# Only libgbm genuinely has to be linked (the `drm` crate reaches the kernel through
# rustix/linux-raw-sys, and khronos-egl dlopen's libEGL at runtime), but we mirror the whole
# library directories anyway: it is a one-time ~300 MB, it makes the linker's DT_NEEDED
# resolution just work, and it means adding a dependency later doesn't send you back here.
#
#   ./deploy/sync-sysroot.sh pi@stratux.local
#   ./deploy/sync-sysroot.sh pi@stratux.local arm-linux-gnueabihf   # 32-bit image
#
set -euo pipefail

HOST="${1:?usage: sync-sysroot.sh user@host [multiarch-triple]}"
TRIPLE="${2:-aarch64-linux-gnu}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYSROOT="$ROOT/sysroot"

echo "==> Checking the target's architecture"
REMOTE_ARCH="$(ssh "$HOST" 'dpkg --print-architecture')"
echo "    remote dpkg architecture: $REMOTE_ARCH"
case "$REMOTE_ARCH:$TRIPLE" in
  arm64:aarch64-linux-gnu|armhf:arm-linux-gnueabihf) ;;
  *)
    echo "!!! Mismatch: remote is '$REMOTE_ARCH' but you asked for '$TRIPLE'." >&2
    echo "    arm64 -> aarch64-linux-gnu (Rust target aarch64-unknown-linux-gnu)" >&2
    echo "    armhf -> arm-linux-gnueabihf (Rust target armv7-unknown-linux-gnueabihf)" >&2
    exit 1
    ;;
esac

echo "==> Ensuring the GBM headers/libraries are present on the target"
ssh "$HOST" 'sudo apt-get update -qq && sudo apt-get install -y libgbm-dev libdrm-dev libegl1-mesa-dev'

echo "==> Mirroring into $SYSROOT"
mkdir -p "$SYSROOT/usr/lib/$TRIPLE" "$SYSROOT/lib/$TRIPLE" "$SYSROOT/usr/include"

# --copy-unsafe-links turns symlinks that point outside the tree into real files, so the
# sysroot is self-contained and the linker doesn't chase absolute paths back onto the host.
rsync -a --copy-unsafe-links --info=progress2 \
  "$HOST:/usr/lib/$TRIPLE/" "$SYSROOT/usr/lib/$TRIPLE/"
rsync -a --copy-unsafe-links --info=progress2 \
  "$HOST:/lib/$TRIPLE/" "$SYSROOT/lib/$TRIPLE/"
rsync -a --copy-unsafe-links --info=progress2 \
  "$HOST:/usr/include/" "$SYSROOT/usr/include/"

echo
echo "==> Done. Sanity check:"
ls -l "$SYSROOT/usr/lib/$TRIPLE/libgbm.so"* 2>/dev/null || {
  echo "!!! libgbm.so is missing from the sysroot; cross-linking will fail." >&2
  exit 1
}
echo
echo "Note: .cargo/config.toml hardcodes the sysroot path. If you moved the checkout,"
echo "update the --sysroot link-args there to match $SYSROOT."
