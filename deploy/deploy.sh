#!/usr/bin/env bash
#
# Cross-build a binary and push it to the Pi, along with everything install.sh needs.
#
#   ./deploy/deploy.sh pi@stratux.local                 # builds avionics (the display)
#   BIN=gfx-spike ./deploy/deploy.sh pi@stratux.local   # builds the M1 rendering spike
#   TARGET=armv7-unknown-linux-gnueabihf ./deploy/deploy.sh pi@stratux.local
#
set -euo pipefail

HOST="${1:?usage: deploy.sh user@host}"
TARGET="${TARGET:-aarch64-unknown-linux-gnu}"
BIN="${BIN:-avionics}"
DEST="${DEST:-/tmp}"
# Where the deploy scripts land, so install.sh can find the systemd unit next to itself.
STAGE="${STAGE:-/tmp/avionics-deploy}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

case "$TARGET" in
  aarch64-*) TRIPLE=aarch64-linux-gnu ;;
  armv7-*)   TRIPLE=arm-linux-gnueabihf ;;
  *) echo "!!! Unsupported TARGET '$TARGET'." >&2; exit 1 ;;
esac

# Check for the versioned library, not just the linker symlink. A directory containing only a
# hand-made libgbm.so would satisfy the link step but record DT_NEEDED as "libgbm.so" instead
# of "libgbm.so.1", producing a binary that then needs libgbm-dev installed on the Pi.
if [[ ! -e "sysroot/usr/lib/$TRIPLE/libgbm.so.1" ]]; then
  echo "!!! sysroot/usr/lib/$TRIPLE/libgbm.so.1 is missing." >&2
  echo "    Run: ./deploy/sync-sysroot.sh --offline        (no Pi network needed)" >&2
  echo "     or: ./deploy/sync-sysroot.sh $HOST $TRIPLE" >&2
  exit 1
fi

echo "==> Building $BIN for $TARGET (kms only; the offscreen backend is a dev-machine thing)"
cargo build --release --target "$TARGET" -p "$BIN" --no-default-features --features kms

# A cross-link against the wrong glibc still produces a perfectly valid-looking ELF; it fails
# only when the Pi tries to exec it. Catch it here rather than over SSH.
"$ROOT/deploy/check-glibc.sh" --max "${GLIBC_MAX:-2.36}" "target/$TARGET/release/$BIN"

echo "==> Copying the binary to $HOST:$DEST/$BIN"
rsync -az --info=progress2 "target/$TARGET/release/$BIN" "$HOST:$DEST/$BIN"

# install.sh reads the systemd unit from alongside itself, so the whole directory has to travel,
# not just the binary. Without this the documented install flow fails at the last step.
echo "==> Copying the deploy scripts to $HOST:$STAGE"
rsync -az --info=progress2 --delete \
  --exclude '__pycache__' \
  deploy/ "$HOST:$STAGE/"
rsync -az README.md "$HOST:$STAGE/../avionics-README.md" 2>/dev/null || true

if [[ "$BIN" == "gfx-spike" ]]; then
  cat <<EOF

==> Deployed. To run the M1 rendering spike on the panel:

    ssh -t $HOST 'sudo $DEST/gfx-spike'

  Stop it with Ctrl-C; it restores the console on the way out. If a panic ever leaves the
  console blank (the release profile aborts rather than unwinding, so Drop does not run),
  recover with:

    ssh $HOST 'sudo chvt 1'
EOF
else
  cat <<EOF

==> Deployed. Next, on the Pi:

    ssh -t $HOST
    sudo $DEST/$BIN --check              # verify before wiring anything up
    cd $STAGE
    sudo AVIONICS_BINARY=$DEST/$BIN ./install.sh --dry-run
    sudo AVIONICS_BINARY=$DEST/$BIN ./install.sh
    sudo systemctl start avionics
    journalctl -u avionics -f

  To try it without installing a service, just run it:

    ssh -t $HOST 'sudo $DEST/$BIN'
EOF
fi
